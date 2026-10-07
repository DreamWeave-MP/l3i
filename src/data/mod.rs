//! The generic bulk-data plane: the `dream.data` extension, module `@dream/data`.
//!
//! JSL and Luau are the control plane: policy, control flow, domain semantics. This module is
//! the data plane: dense typed storage, selection, movement, ordering and reduction over Luau
//! buffers, executed natively with **no per-element callback and no per-element host
//! crossing**. A package describes the bulk operation it needs; L3i owns the element loop.
//!
//! The operand is a **typed contiguous span** of a buffer: `(buffer, kind, offset, count)`,
//! where `kind` names the element representation (`"u8"`, `"i8"`, `"u16"`, `"i16"`, `"u32"`,
//! `"i32"`, `"f32"`, `"f64"`, little-endian as the `buffer` library reads them), `offset` is a
//! byte offset and `count` an element count. A kind may carry a stride, `"f32@16"`: one f32
//! field of every 16-byte record, which is how packed records (a simulated body, an entity slot)
//! live in buffers; the default stride is the element size. Every span is bounds-checked once,
//! before any element is touched; a span that does not fit raises before the operation starts.
//! Spans are descriptors, not objects: nothing is allocated to name one.
//!
//! Element values cross as Luau numbers with exactly the `buffer` library's conversions: an
//! integer kind reads as its exact value and writes by truncating toward zero and wrapping to
//! its width (NaN writes zero); `f32` rounds on write. Reductions accumulate in `f64` in
//! element order, so `sum` over a span equals the handwritten Luau loop bit for bit, including
//! for `f32` elements, and integer sums stay exact below 2^53.
//!
//! "These elements" without materializing them is a [`Selection`]: a bitset over the element
//! positions of one span length, produced by `compare`, combined with `intersect`,
//! `union`, `xor`, `difference`, `complement`, counted, or turned into an index vector. Index vectors are `u32`
//! buffers of zero-based element positions, the input of `gather` and `scatter` and the
//! output of `argsort` and `partition`. Every operation that produces output takes the output
//! storage from the caller (`into` selections, `out` buffers), so steady state allocates
//! nothing.
//!
//! Aliasing is defined, never assumed: element-wise operations (`add`, `scale`, `clamp`, the
//! copies inside `gather` and `scatter`) read each element then write its result before the
//! next, so an output that overlaps an input sees the sequential result, and in-place forms
//! (`out` equal to the input span) are the ordinary case. Out-of-range indices in `gather` and
//! `scatter` raise before anything is written.
//!
//! ```lua
//! local data = require('@dream/data')
//! local total = data.sum(samples, 'f32', 0, n)
//! local hot = data.compare(samples, 'f32', 0, n, 'gt', threshold)
//! local count = hot:count()
//! local order = buffer.create(n * 4)
//! data.argsort(samples, 'f32', 0, n, order)
//! data.gather(samples, 'f32', 0, n, order, sorted, 0)
//! ```
//!
//! The lowering of recognized JSL pipelines calls these same functions through the global
//! alias the extension installs (`__l3i_data`), with the slice's bounds already normalized by
//! the ordinary JSL prologue, so the explicit API and the recognized form share one
//! implementation and one semantic contract; see `DATA_PLANE.md`.

#[cfg(feature = "jit")]
pub mod lowering;

use std::cell::{Cell, RefCell};
use std::cmp::Ordering;

use crate::bind::{Call, StackResults};
use crate::convert::{BufferView, Exact, NewBuffer, Push};
use crate::error::{Error, Result};
use crate::extension::{CompilerTypePolicy, Extension, ExtensionDescriptor, InstallContext, ModuleDecl, TagPolicy};
use crate::userdata::Owned;

/// The extension id.
pub const EXTENSION_ID: &str = "dream.data";
/// The module path.
pub const MODULE: &str = "@dream/data";
/// The global under which the module is also published, for lowered JSL pipelines.
pub const LOWERING_GLOBAL: &str = "__l3i_data";

/// The `dream.data` extension.
#[derive(Clone, Copy, Debug, Default)]
pub struct DataExtension;

/// An element representation inside a buffer, named as the `buffer` library names its reads.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    U8,
    I8,
    U16,
    I16,
    U32,
    I32,
    F32,
    F64,
}

impl Kind {
    /// The kind named by a script, or an argument error naming every spelling.
    pub fn parse(what: &str, name: &str) -> Result<Kind> {
        Ok(match name {
            "u8" => Kind::U8,
            "i8" => Kind::I8,
            "u16" => Kind::U16,
            "i16" => Kind::I16,
            "u32" => Kind::U32,
            "i32" => Kind::I32,
            "f32" => Kind::F32,
            "f64" => Kind::F64,
            other => {
                return Err(Error::runtime(format!(
                    "{what}: unknown element kind '{other}' (expected u8, i8, u16, i16, u32, i32, f32 or f64)"
                )));
            }
        })
    }

    pub fn name(self) -> &'static str {
        match self {
            Kind::U8 => "u8",
            Kind::I8 => "i8",
            Kind::U16 => "u16",
            Kind::I16 => "i16",
            Kind::U32 => "u32",
            Kind::I32 => "i32",
            Kind::F32 => "f32",
            Kind::F64 => "f64",
        }
    }

    /// Bytes per element.
    pub fn size(self) -> usize {
        match self {
            Kind::U8 | Kind::I8 => 1,
            Kind::U16 | Kind::I16 => 2,
            Kind::U32 | Kind::I32 | Kind::F32 => 4,
            Kind::F64 => 8,
        }
    }

    /// Reads one element as the number `buffer.read<kind>` would return.
    ///
    /// # Safety
    ///
    /// `ptr` points at `size()` readable bytes.
    #[inline(always)]
    unsafe fn read(self, ptr: *const u8) -> f64 {
        // SAFETY: the caller guarantees `size()` readable bytes; unaligned reads are fine.
        unsafe {
            match self {
                Kind::U8 => f64::from(ptr.read()),
                Kind::I8 => f64::from(ptr.cast::<i8>().read()),
                Kind::U16 => f64::from(ptr.cast::<u16>().read_unaligned()),
                Kind::I16 => f64::from(ptr.cast::<i16>().read_unaligned()),
                Kind::U32 => f64::from(ptr.cast::<u32>().read_unaligned()),
                Kind::I32 => f64::from(ptr.cast::<i32>().read_unaligned()),
                Kind::F32 => f64::from(ptr.cast::<f32>().read_unaligned()),
                Kind::F64 => ptr.cast::<f64>().read_unaligned(),
            }
        }
    }

    /// Writes one element as `buffer.write<kind>` would: integers truncate toward zero and
    /// wrap to the width (NaN becomes zero), `f32` rounds.
    ///
    /// # Safety
    ///
    /// `ptr` points at `size()` writable bytes.
    #[inline(always)]
    unsafe fn write(self, ptr: *mut u8, value: f64) {
        // Truncation toward zero as the C casts in lbuflib do for in-range values; out of
        // range, Rust saturates to i64 before the wrap, which is the documented rule.
        let wide = value as i64;
        // SAFETY: the caller guarantees `size()` writable bytes; unaligned writes are fine.
        unsafe {
            match self {
                Kind::U8 => ptr.write(wide as u8),
                Kind::I8 => ptr.cast::<i8>().write(wide as i8),
                Kind::U16 => ptr.cast::<u16>().write_unaligned(wide as u16),
                Kind::I16 => ptr.cast::<i16>().write_unaligned(wide as i16),
                Kind::U32 => ptr.cast::<u32>().write_unaligned(wide as u32),
                Kind::I32 => ptr.cast::<i32>().write_unaligned(wide as i32),
                Kind::F32 => ptr.cast::<f32>().write_unaligned(value as f32),
                Kind::F64 => ptr.cast::<f64>().write_unaligned(value),
            }
        }
    }
}

/// An element kind with the distance between consecutive elements: `"f32"` is contiguous
/// (stride = size); `"f32@16"` is one f32 field of a 16-byte record, the shape packed records
/// take in buffers (a simulated body, an entity slot). A stride below the element size is an
/// error; a stride above it skips the rest of the record.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Layout {
    pub kind: Kind,
    pub stride: usize,
}

impl Layout {
    pub fn parse(what: &str, name: &str) -> Result<Layout> {
        let (kind, stride) = match name.split_once('@') {
            Some((kind, stride)) => {
                let kind = Kind::parse(what, kind)?;
                let stride = stride.parse::<usize>().ok().filter(|stride| *stride >= kind.size()).ok_or_else(|| {
                    Error::runtime(format!(
                        "{what}: stride '{stride}' must be a whole number of bytes of at least {} for {}",
                        kind.size(),
                        kind.name()
                    ))
                })?;
                (kind, stride)
            }
            None => {
                let kind = Kind::parse(what, name)?;
                (kind, kind.size())
            }
        };
        Ok(Layout { kind, stride })
    }

    pub fn contiguous(kind: Kind) -> Layout {
        Layout { kind, stride: kind.size() }
    }
}

/// A bounds-checked typed span: `count` elements of a layout starting `offset` bytes into a
/// buffer. Built once per operation; element access from here is unchecked by construction.
#[derive(Clone, Copy)]
struct Span {
    base: *mut u8,
    kind: Kind,
    stride: usize,
    count: usize,
}

impl Span {
    fn new(what: &str, buffer: &BufferView<'_>, layout: Layout, offset: Exact<i64>, count: Exact<i64>) -> Result<Span> {
        let Layout { kind, stride } = layout;
        let offset =
            usize::try_from(offset.0).map_err(|_| Error::runtime(format!("{what}: negative offset {}", offset.0)))?;
        let count =
            usize::try_from(count.0).map_err(|_| Error::runtime(format!("{what}: negative count {}", count.0)))?;
        // The last element ends at offset + (count - 1) * stride + size; an empty span needs
        // only its offset to be in range.
        let bytes = if count == 0 {
            Some(offset)
        } else {
            (count - 1)
                .checked_mul(stride)
                .and_then(|bytes| bytes.checked_add(kind.size()))
                .and_then(|bytes| bytes.checked_add(offset))
        };
        match bytes {
            Some(end) if end <= buffer.len() => {}
            _ => {
                return Err(Error::runtime(format!(
                    "{what}: span of {count} {}{} at offset {offset} exceeds the buffer length {}",
                    kind.name(),
                    if stride == kind.size() { String::new() } else { format!("@{stride}") },
                    buffer.len()
                )));
            }
        }
        // SAFETY: the buffer's storage holds every element's bytes, verified above; the pointer
        // is only dereferenced for elements inside the span.
        let base = unsafe { buffer.bytes_unchecked().as_ptr().add(offset) }.cast_mut();
        Ok(Span { base, kind, stride, count })
    }

    #[inline(always)]
    fn get(&self, index: usize) -> f64 {
        debug_assert!(index < self.count);
        // SAFETY: `index < count`, and the span was bounds-checked at construction.
        unsafe { self.kind.read(self.base.add(index * self.stride)) }
    }

    #[inline(always)]
    fn set(&self, index: usize, value: f64) {
        debug_assert!(index < self.count);
        // SAFETY: as `get`; buffers are writable storage.
        unsafe { self.kind.write(self.base.add(index * self.stride), value) }
    }
}

/// A `u32` index vector: zero-based element positions in a buffer, `count` of them from
/// `offset`. The input of `gather`/`scatter`, the output of `argsort`/`partition` and
/// `Selection:indices`.
struct Indices {
    base: *mut u8,
    count: usize,
}

impl Indices {
    fn new(what: &str, buffer: &BufferView<'_>, offset: Exact<i64>, count: Option<Exact<i64>>) -> Result<Indices> {
        let offset =
            usize::try_from(offset.0).map_err(|_| Error::runtime(format!("{what}: negative offset {}", offset.0)))?;
        let count = match count {
            Some(count) => {
                usize::try_from(count.0).map_err(|_| Error::runtime(format!("{what}: negative count {}", count.0)))?
            }
            None => buffer.len().saturating_sub(offset) / 4,
        };
        match count.checked_mul(4).and_then(|bytes| bytes.checked_add(offset)) {
            Some(end) if end <= buffer.len() => {}
            _ => {
                return Err(Error::runtime(format!(
                    "{what}: {count} u32 indices at offset {offset} exceed the buffer length {}",
                    buffer.len()
                )));
            }
        }
        // SAFETY: as `Span::new`.
        Ok(Indices { base: unsafe { buffer.bytes_unchecked().as_ptr().add(offset) }.cast_mut(), count })
    }

    #[inline(always)]
    fn get(&self, index: usize) -> usize {
        debug_assert!(index < self.count);
        // SAFETY: `index < count`, bounds-checked at construction.
        unsafe { self.base.add(index * 4).cast::<u32>().read_unaligned() as usize }
    }

    #[inline(always)]
    fn set(&self, index: usize, value: usize) {
        debug_assert!(index < self.count);
        // SAFETY: as `get`.
        unsafe { self.base.add(index * 4).cast::<u32>().write_unaligned(value as u32) }
    }
}

/// A comparison, as a script names it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Comparison {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
}

impl Comparison {
    pub fn parse(what: &str, name: &str) -> Result<Comparison> {
        Ok(match name {
            "eq" => Comparison::Eq,
            "ne" => Comparison::Ne,
            "lt" => Comparison::Lt,
            "le" => Comparison::Le,
            "gt" => Comparison::Gt,
            "ge" => Comparison::Ge,
            other => {
                return Err(Error::runtime(format!(
                    "{what}: unknown comparison '{other}' (expected eq, ne, lt, le, gt or ge)"
                )));
            }
        })
    }

    /// IEEE semantics, as Luau's operators: every comparison with NaN is false except `ne`.
    #[inline(always)]
    fn test(self, value: f64, threshold: f64) -> bool {
        match self {
            Comparison::Eq => value == threshold,
            Comparison::Ne => value != threshold,
            Comparison::Lt => value < threshold,
            Comparison::Le => value <= threshold,
            Comparison::Gt => value > threshold,
            Comparison::Ge => value >= threshold,
        }
    }
}

/// `dream.data.Selection`: which positions of a span of `len` elements are selected, as a
/// bitset. Produced by `compare`, combined with the boolean algebra, counted, iterated as an
/// index vector. One selection serves as reusable output for any number of operations over
/// spans of the same length.
pub struct Selection {
    len: Cell<usize>,
    bits: RefCell<Vec<u64>>,
}

// SAFETY: the payload holds no Lua references.
unsafe impl crate::userdata::Userdata for Selection {
    const NAME: &'static str = "dream.data.Selection";
}

impl Selection {
    pub fn new(len: usize) -> Selection {
        Selection { len: Cell::new(len), bits: RefCell::new(vec![0; len.div_ceil(64)]) }
    }

    pub fn len(&self) -> usize {
        self.len.get()
    }

    pub fn is_empty(&self) -> bool {
        self.len.get() == 0
    }

    /// Resizes to `len` positions, all unselected.
    fn reset(&self, len: usize) {
        let mut bits = self.bits.borrow_mut();
        bits.clear();
        bits.resize(len.div_ceil(64), 0);
        self.len.set(len);
    }

    pub fn count(&self) -> usize {
        self.bits.borrow().iter().map(|word| word.count_ones() as usize).sum()
    }

    pub fn get(&self, index: usize) -> bool {
        index < self.len.get() && self.bits.borrow()[index / 64] & (1 << (index % 64)) != 0
    }

    /// Clears the bits past `len` in the last word so counts and `all` stay exact.
    fn trim(&self) {
        let len = self.len.get();
        if !len.is_multiple_of(64) {
            let mut bits = self.bits.borrow_mut();
            let last = bits.len() - 1;
            bits[last] &= (1u64 << (len % 64)) - 1;
        }
    }

    fn require_same_len(&self, what: &str, other: &Selection) -> Result<()> {
        if self.len.get() != other.len.get() {
            return Err(Error::runtime(format!(
                "{what}: selections cover different lengths ({} and {})",
                self.len.get(),
                other.len.get()
            )));
        }
        Ok(())
    }
}

/// The selection a producing operation writes into: the caller's `into`, resized to `len`, or
/// a new one.
enum Target<'a> {
    Reused(&'a Selection),
    Fresh(Selection),
}

impl Target<'_> {
    fn prepare(into: Option<&Selection>, len: usize) -> Target<'_> {
        match into {
            Some(selection) => {
                selection.reset(len);
                Target::Reused(selection)
            }
            None => Target::Fresh(Selection::new(len)),
        }
    }

    fn selection(&self) -> &Selection {
        match self {
            Target::Reused(selection) => selection,
            Target::Fresh(selection) => selection,
        }
    }

    /// Returns the selection to the script: the reused argument itself, or the new value.
    fn finish(self, call: &Call<'_>, into_argument: std::ffi::c_int) -> Result<StackResults> {
        match self {
            Target::Reused(_) => {
                Push::push_only(&call.arg(into_argument), call)?;
            }
            Target::Fresh(selection) => {
                crate::userdata::push_owned(call.stack(), selection)?;
            }
        }
        Ok(StackResults)
    }
}

// ---------------------------------------------------------------------------------------------
// Reductions
// ---------------------------------------------------------------------------------------------

fn sum(buffer: BufferView<'_>, kind: &str, offset: Exact<i64>, count: Exact<i64>) -> Result<f64> {
    let span = Span::new("data.sum", &buffer, Layout::parse("data.sum", kind)?, offset, count)?;
    let mut total = 0.0;
    for i in 0..span.count {
        total += span.get(i);
    }
    Ok(total)
}

/// The index of the first least (or greatest) element by Luau `<` (`>`): a NaN that arrives
/// first stays, later NaNs never replace. None for an empty span.
fn extreme(span: &Span, greatest: bool) -> Option<usize> {
    if span.count == 0 {
        return None;
    }
    let mut best = 0;
    let mut value = span.get(0);
    for i in 1..span.count {
        let candidate = span.get(i);
        if if greatest { candidate > value } else { candidate < value } {
            best = i;
            value = candidate;
        }
    }
    Some(best)
}

fn min(buffer: BufferView<'_>, kind: &str, offset: Exact<i64>, count: Exact<i64>) -> Result<Option<f64>> {
    let span = Span::new("data.min", &buffer, Layout::parse("data.min", kind)?, offset, count)?;
    Ok(extreme(&span, false).map(|i| span.get(i)))
}

fn max(buffer: BufferView<'_>, kind: &str, offset: Exact<i64>, count: Exact<i64>) -> Result<Option<f64>> {
    let span = Span::new("data.max", &buffer, Layout::parse("data.max", kind)?, offset, count)?;
    Ok(extreme(&span, true).map(|i| span.get(i)))
}

fn argmin(buffer: BufferView<'_>, kind: &str, offset: Exact<i64>, count: Exact<i64>) -> Result<Option<f64>> {
    let span = Span::new("data.argmin", &buffer, Layout::parse("data.argmin", kind)?, offset, count)?;
    Ok(extreme(&span, false).map(|i| i as f64))
}

fn argmax(buffer: BufferView<'_>, kind: &str, offset: Exact<i64>, count: Exact<i64>) -> Result<Option<f64>> {
    let span = Span::new("data.argmax", &buffer, Layout::parse("data.argmax", kind)?, offset, count)?;
    Ok(extreme(&span, true).map(|i| i as f64))
}

/// How many elements satisfy `comparison` against `threshold`, without a selection.
fn count(
    buffer: BufferView<'_>,
    kind: &str,
    offset: Exact<i64>,
    count: Exact<i64>,
    comparison: &str,
    threshold: f64,
) -> Result<f64> {
    let span = Span::new("data.count", &buffer, Layout::parse("data.count", kind)?, offset, count)?;
    let comparison = Comparison::parse("data.count", comparison)?;
    let mut selected = 0usize;
    for i in 0..span.count {
        selected += usize::from(comparison.test(span.get(i), threshold));
    }
    Ok(selected as f64)
}

// ---------------------------------------------------------------------------------------------
// Comparison and selection
// ---------------------------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
fn compare(
    call: &Call<'_>,
    buffer: BufferView<'_>,
    kind: &str,
    offset: Exact<i64>,
    count: Exact<i64>,
    comparison: &str,
    threshold: f64,
    into: Option<&Selection>,
) -> Result<StackResults> {
    let span = Span::new("data.compare", &buffer, Layout::parse("data.compare", kind)?, offset, count)?;
    let comparison = Comparison::parse("data.compare", comparison)?;
    let target = Target::prepare(into, span.count);
    {
        let selection = target.selection();
        let mut bits = selection.bits.borrow_mut();
        for (word, chunk) in bits.iter_mut().zip((0..span.count).step_by(64)) {
            let mut mask = 0u64;
            for bit in 0..64.min(span.count - chunk) {
                mask |= u64::from(comparison.test(span.get(chunk + bit), threshold)) << bit;
            }
            *word = mask;
        }
    }
    target.finish(call, 7)
}

fn selection(len: Exact<i64>) -> Result<Owned<Selection>> {
    let len =
        usize::try_from(len.0).map_err(|_| Error::runtime(format!("data.selection: negative length {}", len.0)))?;
    Ok(Owned(Selection::new(len)))
}

fn combine(
    call: &Call<'_>,
    what: &str,
    left: &Selection,
    right: &Selection,
    into: Option<&Selection>,
    op: impl Fn(u64, u64) -> u64,
) -> Result<StackResults> {
    left.require_same_len(what, right)?;
    // Computing into a fresh vector first keeps `into == left` or `into == right` well defined
    // (the script may pass one selection twice) and never borrows one RefCell twice.
    let words: Vec<u64> = left.bits.borrow().iter().zip(right.bits.borrow().iter()).map(|(a, b)| op(*a, *b)).collect();
    let target = Target::prepare(into, left.len());
    *target.selection().bits.borrow_mut() = words;
    target.selection().trim();
    target.finish(call, 3)
}

fn describe_selection(d: &mut ExtensionDescriptor) {
    let mut selection = d.userdata::<Selection>("dream.data.Selection");
    selection
        .tag(TagPolicy::Preferred)
        .compiler_type(CompilerTypePolicy::Never)
        .doc("Which positions of a span are selected: a bitset produced by compare, combined, counted or turned into indices.");
    selection
        .method("count", |selection: &Selection| selection.count() as f64)
        .signature("(self): number")
        .doc("How many positions are selected.");
    selection
        .method("len", |selection: &Selection| selection.len() as f64)
        .signature("(self): number")
        .doc("How many positions the selection covers.");
    selection
        .method("get", |selection: &Selection, index: Exact<i64>| -> bool {
            usize::try_from(index.0).is_ok_and(|index| selection.get(index))
        })
        .signature("(self, index: number): boolean")
        .doc("Whether the zero-based position is selected; false outside the length.");
    selection
        .method("any", |selection: &Selection| selection.bits.borrow().iter().any(|word| *word != 0))
        .signature("(self): boolean");
    selection.method("all", |selection: &Selection| selection.count() == selection.len()).signature("(self): boolean");
    selection
        .method("clear", |selection: &Selection| {
            selection.bits.borrow_mut().iter_mut().for_each(|word| *word = 0);
        })
        .signature("(self): ()")
        .doc("Unselects every position.");
    selection
        .method("intersect", |left: &Selection, call: &Call<'_>, right: &Selection, into: Option<&Selection>| {
            combine(call, "Selection:intersect", left, right, into, |a, b| a & b)
        })
        .signature("(self, other: dream_data_Selection, into: dream_data_Selection?): dream_data_Selection")
        .doc("Positions selected in both; written into `into` when given, else a new selection.");
    selection
        .method("union", |left: &Selection, call: &Call<'_>, right: &Selection, into: Option<&Selection>| {
            combine(call, "Selection:union", left, right, into, |a, b| a | b)
        })
        .signature("(self, other: dream_data_Selection, into: dream_data_Selection?): dream_data_Selection")
        .doc("Positions selected in either.");
    selection
        .method("xor", |left: &Selection, call: &Call<'_>, right: &Selection, into: Option<&Selection>| {
            combine(call, "Selection:xor", left, right, into, |a, b| a ^ b)
        })
        .signature("(self, other: dream_data_Selection, into: dream_data_Selection?): dream_data_Selection")
        .doc("Positions selected in exactly one.");
    selection
        .method("difference", |left: &Selection, call: &Call<'_>, right: &Selection, into: Option<&Selection>| {
            combine(call, "Selection:difference", left, right, into, |a, b| a & !b)
        })
        .signature("(self, other: dream_data_Selection, into: dream_data_Selection?): dream_data_Selection")
        .doc("Positions selected here and not in other.");
    selection
        .method("complement", |source: &Selection, call: &Call<'_>, into: Option<&Selection>| -> Result<StackResults> {
            let words: Vec<u64> = source.bits.borrow().iter().map(|word| !word).collect();
            let target = Target::prepare(into, source.len());
            *target.selection().bits.borrow_mut() = words;
            target.selection().trim();
            target.finish(call, 2)
        })
        .signature("(self, into: dream_data_Selection?): dream_data_Selection")
        .doc("The complement within the length.");
    selection
        .method(
            "indices",
            |selection: &Selection, call: &Call<'_>, out: Option<BufferView<'_>>, offset: Option<Exact<i64>>| -> Result<StackResults> {
                let count = selection.count();
                let bits = selection.bits.borrow();
                let positions = (0..selection.len()).filter(|&i| bits[i / 64] & (1 << (i % 64)) != 0);
                match out {
                    Some(out) => {
                        let indices = Indices::new("Selection:indices", &out, offset.unwrap_or(Exact(0)), Some(Exact(count as i64)))?;
                        for (slot, position) in positions.enumerate() {
                            indices.set(slot, position);
                        }
                        Push::push_only(&call.arg(2), call)?;
                    }
                    None => {
                        let mut bytes = Vec::with_capacity(count * 4);
                        for position in positions {
                            bytes.extend_from_slice(&(position as u32).to_le_bytes());
                        }
                        Push::push_only(&NewBuffer(bytes), call)?;
                    }
                }
                Push::push_only(&(count as f64), call)?;
                Ok(StackResults)
            },
        )
        .signature("(self, out: buffer?, offset: number?): (buffer, number)")
        .doc("The selected positions as a u32 index vector and their count; into `out` from `offset` when given (it must hold count * 4 bytes), else a new exact buffer.");
}

// ---------------------------------------------------------------------------------------------
// Movement by index, fill, element-wise arithmetic
// ---------------------------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
fn gather(
    source: BufferView<'_>,
    kind: &str,
    source_offset: Exact<i64>,
    source_count: Exact<i64>,
    indices: BufferView<'_>,
    destination: BufferView<'_>,
    destination_offset: Exact<i64>,
    index_count: Option<Exact<i64>>,
) -> Result<f64> {
    let kind = Layout::parse("data.gather", kind)?;
    let source = Span::new("data.gather", &source, kind, source_offset, source_count)?;
    let indices = Indices::new("data.gather", &indices, Exact(0), index_count)?;
    // The source may be a record field (a strided layout); the gathered column is contiguous.
    let destination = Span::new(
        "data.gather",
        &destination,
        Layout::contiguous(kind.kind),
        destination_offset,
        Exact(indices.count as i64),
    )?;
    for i in 0..indices.count {
        let index = indices.get(i);
        if index >= source.count {
            return Err(Error::runtime(format!(
                "data.gather: index {index} at position {i} is outside the source span of {} elements",
                source.count
            )));
        }
    }
    // Sequential semantics: each element is read then written before the next, so an aliased
    // destination observes earlier writes, deterministically.
    for i in 0..indices.count {
        destination.set(i, source.get(indices.get(i)));
    }
    Ok(indices.count as f64)
}

#[allow(clippy::too_many_arguments)]
fn scatter(
    source: BufferView<'_>,
    kind: &str,
    source_offset: Exact<i64>,
    source_count: Exact<i64>,
    indices: BufferView<'_>,
    destination: BufferView<'_>,
    destination_offset: Exact<i64>,
    destination_count: Exact<i64>,
) -> Result<f64> {
    let kind = Layout::parse("data.scatter", kind)?;
    // The scattered column is contiguous; the destination may be a record field (strided).
    let source = Span::new("data.scatter", &source, Layout::contiguous(kind.kind), source_offset, source_count)?;
    let indices = Indices::new("data.scatter", &indices, Exact(0), Some(Exact(source.count as i64)))?;
    let destination = Span::new("data.scatter", &destination, kind, destination_offset, destination_count)?;
    for i in 0..indices.count {
        let index = indices.get(i);
        if index >= destination.count {
            return Err(Error::runtime(format!(
                "data.scatter: index {index} at position {i} is outside the destination span of {} elements",
                destination.count
            )));
        }
    }
    for i in 0..source.count {
        destination.set(indices.get(i), source.get(i));
    }
    Ok(source.count as f64)
}

fn fill(buffer: BufferView<'_>, kind: &str, offset: Exact<i64>, count: Exact<i64>, value: f64) -> Result<()> {
    let span = Span::new("data.fill", &buffer, Layout::parse("data.fill", kind)?, offset, count)?;
    for i in 0..span.count {
        span.set(i, value);
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn add(
    left: BufferView<'_>,
    kind: &str,
    left_offset: Exact<i64>,
    right: BufferView<'_>,
    right_offset: Exact<i64>,
    out: BufferView<'_>,
    out_offset: Exact<i64>,
    count: Exact<i64>,
) -> Result<()> {
    let kind = Layout::parse("data.add", kind)?;
    let left = Span::new("data.add", &left, kind, left_offset, count)?;
    let right = Span::new("data.add", &right, kind, right_offset, count)?;
    let out = Span::new("data.add", &out, kind, out_offset, count)?;
    for i in 0..out.count {
        out.set(i, left.get(i) + right.get(i));
    }
    Ok(())
}

fn scale(
    source: BufferView<'_>,
    kind: &str,
    offset: Exact<i64>,
    count: Exact<i64>,
    factor: f64,
    out: BufferView<'_>,
    out_offset: Exact<i64>,
) -> Result<()> {
    let kind = Layout::parse("data.scale", kind)?;
    let source = Span::new("data.scale", &source, kind, offset, count)?;
    let out = Span::new("data.scale", &out, kind, out_offset, count)?;
    for i in 0..out.count {
        out.set(i, source.get(i) * factor);
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn clamp(
    source: BufferView<'_>,
    kind: &str,
    offset: Exact<i64>,
    count: Exact<i64>,
    low: f64,
    high: f64,
    out: BufferView<'_>,
    out_offset: Exact<i64>,
) -> Result<()> {
    let kind = Layout::parse("data.clamp", kind)?;
    if !matches!(low.partial_cmp(&high), Some(Ordering::Less | Ordering::Equal)) {
        return Err(Error::runtime(format!("data.clamp: low {low} is not at most high {high}")));
    }
    let source = Span::new("data.clamp", &source, kind, offset, count)?;
    let out = Span::new("data.clamp", &out, kind, out_offset, count)?;
    for i in 0..out.count {
        // As math.clamp: NaN passes through, since neither bound compares.
        let value = source.get(i);
        out.set(
            i,
            if value < low {
                low
            } else if value > high {
                high
            } else {
                value
            },
        );
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// Ordering and partition
// ---------------------------------------------------------------------------------------------

/// Ascending by value with NaN last; equal keys keep their index order (stable).
fn key_order(a: f64, b: f64) -> Ordering {
    match a.partial_cmp(&b) {
        Some(order) => order,
        None => b.is_nan().cmp(&a.is_nan()).reverse(),
    }
}

/// The byte range a span or index vector occupies, for aliasing checks.
fn byte_range(base: *mut u8, count: usize, stride: usize, size: usize) -> (usize, usize) {
    let begin = base as usize;
    (begin, if count == 0 { begin } else { begin + (count - 1) * stride + size })
}

fn overlaps(a: (usize, usize), b: (usize, usize)) -> bool {
    a.0 < b.1 && b.0 < a.1
}

#[allow(clippy::too_many_arguments)]
fn argsort(
    keys: BufferView<'_>,
    kind: &str,
    offset: Exact<i64>,
    count: Exact<i64>,
    out: BufferView<'_>,
    out_offset: Option<Exact<i64>>,
    scratch: Option<BufferView<'_>>,
) -> Result<f64> {
    let keys = Span::new("data.argsort", &keys, Layout::parse("data.argsort", kind)?, offset, count)?;
    let out = Indices::new("data.argsort", &out, out_offset.unwrap_or(Exact(0)), Some(Exact(keys.count as i64)))?;
    match scratch {
        None => {
            // Without scratch the keys are read once into a temporary, so the sort never
            // touches the buffer again and an `out` that overlaps `keys` is well defined.
            let mut order: Vec<(f64, u32)> = (0..keys.count).map(|i| (keys.get(i), i as u32)).collect();
            order.sort_by(|a, b| key_order(a.0, b.0));
            for (slot, (_, index)) in order.iter().enumerate() {
                out.set(slot, *index as usize);
            }
        }
        Some(scratch) => {
            // Steady state, no allocation: a bottom-up stable merge sort of the index vector,
            // alternating between `out` and the caller's scratch, reading keys from the span as
            // it compares. The keys are read throughout, so neither index vector may overlap
            // them, and the two index vectors may not overlap each other.
            let scratch = Indices::new("data.argsort", &scratch, Exact(0), Some(Exact(keys.count as i64)))?;
            let key_bytes = byte_range(keys.base, keys.count, keys.stride, keys.kind.size());
            let out_bytes = byte_range(out.base, out.count, 4, 4);
            let scratch_bytes = byte_range(scratch.base, scratch.count, 4, 4);
            if overlaps(key_bytes, out_bytes)
                || overlaps(key_bytes, scratch_bytes)
                || overlaps(out_bytes, scratch_bytes)
            {
                return Err(Error::runtime(
                    "data.argsort: keys, out and scratch must not overlap when scratch is given",
                ));
            }
            let total = keys.count;
            for position in 0..total {
                out.set(position, position);
            }
            let (mut from, mut to) = (&out, &scratch);
            let mut width = 1;
            while width < total {
                let mut left = 0;
                while left < total {
                    let mid = (left + width).min(total);
                    let right = (left + 2 * width).min(total);
                    let (mut lower, mut upper, mut slot) = (left, mid, left);
                    while lower < mid && upper < right {
                        let (first, second) = (from.get(lower), from.get(upper));
                        // Strictly less moves the upper run's element first; ties keep the
                        // lower (earlier) run's element: stable.
                        if key_order(keys.get(second), keys.get(first)) == Ordering::Less {
                            to.set(slot, second);
                            upper += 1;
                        } else {
                            to.set(slot, first);
                            lower += 1;
                        }
                        slot += 1;
                    }
                    while lower < mid {
                        to.set(slot, from.get(lower));
                        lower += 1;
                        slot += 1;
                    }
                    while upper < right {
                        to.set(slot, from.get(upper));
                        upper += 1;
                        slot += 1;
                    }
                    left = right;
                }
                std::mem::swap(&mut from, &mut to);
                width *= 2;
            }
            if !std::ptr::eq(from, &out) {
                for position in 0..total {
                    out.set(position, from.get(position));
                }
            }
        }
    }
    Ok(keys.count as f64)
}

#[allow(clippy::too_many_arguments)]
fn partition(
    keys: BufferView<'_>,
    kind: &str,
    offset: Exact<i64>,
    count: Exact<i64>,
    comparison: &str,
    threshold: f64,
    out: BufferView<'_>,
    out_offset: Option<Exact<i64>>,
) -> Result<f64> {
    let keys = Span::new("data.partition", &keys, Layout::parse("data.partition", kind)?, offset, count)?;
    let comparison = Comparison::parse("data.partition", comparison)?;
    let out = Indices::new("data.partition", &out, out_offset.unwrap_or(Exact(0)), Some(Exact(keys.count as i64)))?;
    let accepted = (0..keys.count).filter(|&i| comparison.test(keys.get(i), threshold)).count();
    let (mut front, mut back) = (0, accepted);
    for i in 0..keys.count {
        if comparison.test(keys.get(i), threshold) {
            out.set(front, i);
            front += 1;
        } else {
            out.set(back, i);
            back += 1;
        }
    }
    Ok(accepted as f64)
}

// ---------------------------------------------------------------------------------------------
// The typed receiver: the same reductions as methods, lowered to native loops under jit
// ---------------------------------------------------------------------------------------------

/// Words of scratch in the receiver's payload for the native lowering's loop state.
const RECEIVER_SCRATCH_WORDS: usize = 4;

/// `dream.data.Kind`, from `data.kind(name)`: a receiver whose methods are the span reductions
/// for one element kind, `K:sum(buffer, offset, count)` and friends. The bound methods are the
/// semantic path; under `jit` a namecall on an annotated receiver lowers to a native loop
/// ([`lowering::KindLowering`]) that keeps its accumulator and index in the scratch words.
#[repr(C)]
pub struct KindReceiver {
    kind: u64,
    /// The stride in bytes, as a double so native code adds it to its cursor directly.
    stride: f64,
    scratch: [Cell<u64>; RECEIVER_SCRATCH_WORDS],
}

// SAFETY: the payload holds no Lua references.
unsafe impl crate::userdata::Userdata for KindReceiver {
    const NAME: &'static str = "dream.data.Kind";
}

impl KindReceiver {
    pub fn new(layout: Layout) -> KindReceiver {
        KindReceiver { kind: layout.kind as u64, stride: layout.stride as f64, scratch: Default::default() }
    }

    pub fn layout(&self) -> Layout {
        Layout { kind: self.kind(), stride: self.stride as usize }
    }

    pub fn kind(&self) -> Kind {
        match self.kind {
            0 => Kind::U8,
            1 => Kind::I8,
            2 => Kind::U16,
            3 => Kind::I16,
            4 => Kind::U32,
            5 => Kind::I32,
            6 => Kind::F32,
            _ => Kind::F64,
        }
    }

    fn span(&self, what: &str, buffer: &BufferView<'_>, offset: Exact<i64>, count: Exact<i64>) -> Result<Span> {
        Span::new(what, buffer, self.layout(), offset, count)
    }
}

/// Byte offsets native code reads at, pinned here at compile time.
pub(crate) mod layout {
    use super::KindReceiver;

    pub const KIND: i32 = 0;
    pub const STRIDE: i32 = 8;
    pub const SCRATCH: i32 = 16;

    const _: () = {
        assert!(std::mem::offset_of!(KindReceiver, kind) == KIND as usize);
        assert!(std::mem::offset_of!(KindReceiver, stride) == STRIDE as usize);
        assert!(std::mem::offset_of!(KindReceiver, scratch) == SCRATCH as usize);
    };
}

fn describe_kind_receiver(d: &mut ExtensionDescriptor) {
    let mut receiver = d.userdata::<KindReceiver>("dream.data.Kind");
    receiver
        .tag(TagPolicy::Required)
        .compiler_type(CompilerTypePolicy::Required)
        .doc("The span reductions for one element kind as methods; native loops under jit.");
    receiver
        .method(
            "sum",
            |receiver: &KindReceiver, buffer: BufferView<'_>, offset: Exact<i64>, count: Exact<i64>| -> Result<f64> {
                let span = receiver.span("Kind:sum", &buffer, offset, count)?;
                let mut total = 0.0;
                for i in 0..span.count {
                    total += span.get(i);
                }
                Ok(total)
            },
        )
        .signature("(self, buffer: buffer, offset: number, count: number): number")
        .doc("As data.sum for this kind.");
    receiver
        .method(
            "min",
            |receiver: &KindReceiver,
             buffer: BufferView<'_>,
             offset: Exact<i64>,
             count: Exact<i64>|
             -> Result<Option<f64>> {
                let span = receiver.span("Kind:min", &buffer, offset, count)?;
                Ok(extreme(&span, false).map(|i| span.get(i)))
            },
        )
        .signature("(self, buffer: buffer, offset: number, count: number): number?")
        .doc("As data.min for this kind.");
    receiver
        .method(
            "max",
            |receiver: &KindReceiver,
             buffer: BufferView<'_>,
             offset: Exact<i64>,
             count: Exact<i64>|
             -> Result<Option<f64>> {
                let span = receiver.span("Kind:max", &buffer, offset, count)?;
                Ok(extreme(&span, true).map(|i| span.get(i)))
            },
        )
        .signature("(self, buffer: buffer, offset: number, count: number): number?")
        .doc("As data.max for this kind.");
    for (name, comparison) in [
        ("countEq", Comparison::Eq),
        ("countNe", Comparison::Ne),
        ("countLt", Comparison::Lt),
        ("countLe", Comparison::Le),
        ("countGt", Comparison::Gt),
        ("countGe", Comparison::Ge),
    ] {
        let what = format!("Kind:{name}");
        receiver
            .method(
                name,
                move |receiver: &KindReceiver,
                      buffer: BufferView<'_>,
                      offset: Exact<i64>,
                      count: Exact<i64>,
                      threshold: f64|
                      -> Result<f64> {
                    let span = receiver.span(&what, &buffer, offset, count)?;
                    let mut selected = 0usize;
                    for i in 0..span.count {
                        selected += usize::from(comparison.test(span.get(i), threshold));
                    }
                    Ok(selected as f64)
                },
            )
            .signature("(self, buffer: buffer, offset: number, count: number, threshold: number): number")
            .doc("How many elements compare so against threshold; as data.count with that comparison.");
    }
}

fn kind(name: &str) -> Result<Owned<KindReceiver>> {
    Ok(Owned(KindReceiver::new(Layout::parse("data.kind", name)?)))
}

// ---------------------------------------------------------------------------------------------
// The extension
// ---------------------------------------------------------------------------------------------

fn describe_module(module: &mut ModuleDecl) {
    // `kind` is an element kind or a layout `kind@stride` (bytes between elements).
    const SPAN: &str = "buffer: buffer, kind: dream_data_ElementKind, offset: number, count: number";
    module
        .function("kind", kind)
        .signature("(kind: dream_data_ElementKind) -> dream_data_Kind")
        .doc("The receiver whose methods reduce spans of this kind; annotate it (`local U8: dream_data_Kind = data.kind(\"u8\")`) so jit lowers its calls to native loops.")
        .function("sum", sum)
        .signature(format!("({SPAN}) -> number"))
        .doc("The elements added in order into a Luau number; 0 for an empty span.")
        .function("min", min)
        .signature(format!("({SPAN}) -> number?"))
        .doc("The first least element by Luau <, or nil for an empty span; a leading NaN stays.")
        .function("max", max)
        .signature(format!("({SPAN}) -> number?"))
        .doc("The first greatest element by Luau >, or nil for an empty span.")
        .function("argmin", argmin)
        .signature(format!("({SPAN}) -> number?"))
        .doc("The zero-based position of the first least element, or nil.")
        .function("argmax", argmax)
        .signature(format!("({SPAN}) -> number?"))
        .doc("The zero-based position of the first greatest element, or nil.")
        .function("count", count)
        .signature(format!("({SPAN}, comparison: dream_data_Comparison, threshold: number) -> number"))
        .doc("How many elements satisfy the comparison against threshold.")
        .function("compare", compare)
        .signature(format!(
            "({SPAN}, comparison: dream_data_Comparison, threshold: number, into: dream_data_Selection?) -> dream_data_Selection"
        ))
        .doc("The positions whose element satisfies the comparison; written into `into` when given.")
        .function("selection", selection)
        .signature("(len: number) -> dream_data_Selection")
        .doc("An empty selection over len positions, for reuse as `into`.")
        .function("gather", gather)
        .signature("(source: buffer, kind: dream_data_Kind, sourceOffset: number, sourceCount: number, indices: buffer, destination: buffer, destinationOffset: number, indexCount: number?) -> number")
        .doc("destination[i] = source[indices[i]] for every u32 index (all of the indices buffer by default); the count written. The source may be a strided field; the destination column is contiguous. Every index is checked before anything is written.")
        .function("scatter", scatter)
        .signature("(source: buffer, kind: dream_data_Kind, sourceOffset: number, sourceCount: number, indices: buffer, destination: buffer, destinationOffset: number, destinationCount: number) -> number")
        .doc("destination[indices[i]] = source[i] for every source element; the count written. The source column is contiguous; the destination may be a strided field. Every index is checked against destinationCount before anything is written.")
        .function("fill", fill)
        .signature(format!("({SPAN}, value: number) -> ()"))
        .doc("Every element set to value, converted as buffer.write<kind> converts.")
        .function("add", add)
        .signature("(left: buffer, kind: dream_data_Kind, leftOffset: number, right: buffer, rightOffset: number, out: buffer, outOffset: number, count: number) -> ()")
        .doc("out[i] = left[i] + right[i], element by element; out may be either input.")
        .function("scale", scale)
        .signature(format!("({SPAN}, factor: number, out: buffer, outOffset: number) -> ()"))
        .doc("out[i] = source[i] * factor; out may be the source.")
        .function("clamp", clamp)
        .signature(format!("({SPAN}, low: number, high: number, out: buffer, outOffset: number) -> ()"))
        .doc("out[i] = math.clamp(source[i], low, high); NaN passes through; out may be the source.")
        .function("argsort", argsort)
        .signature(format!("({SPAN}, out: buffer, outOffset: number?, scratch: buffer?) -> number"))
        .doc("The u32 permutation that orders the keys ascending, stable for equal keys, NaN last; the count written. With scratch (count * 4 bytes, not overlapping keys or out) nothing is allocated.")
        .function("partition", partition)
        .signature(format!("({SPAN}, comparison: dream_data_Comparison, threshold: number, out: buffer, outOffset: number?) -> number"))
        .doc("The u32 index vector with every position satisfying the comparison first, then the rest, each part in original order; how many satisfied it.");
}

impl Extension for DataExtension {
    fn id(&self) -> &'static str {
        EXTENSION_ID
    }

    fn describe(&self, d: &mut ExtensionDescriptor) -> Result<()> {
        d.type_alias(
            "dream_data_ElementKind",
            "\"u8\" | \"i8\" | \"u16\" | \"i16\" | \"u32\" | \"i32\" | \"f32\" | \"f64\"",
        );
        d.type_alias("dream_data_Comparison", "\"eq\" | \"ne\" | \"lt\" | \"le\" | \"gt\" | \"ge\"");
        describe_selection(d);
        describe_kind_receiver(d);
        #[cfg(feature = "jit")]
        d.native_hooks(lowering::KindLowering);
        let module = d.module(MODULE);
        module.doc("Typed spans of buffers reduced, compared into selections, gathered, scattered, filled, combined, ordered and partitioned natively, with no per-element callbacks.");
        describe_module(module);
        Ok(())
    }

    fn install(&self, context: &mut InstallContext<'_>) -> Result<()> {
        // Lowered JSL pipelines reach the data plane through a fixed global rather than a
        // require, so compiled code needs no module resolver; absent the global, they take
        // their scalar path.
        let table = context.module(MODULE)?.table().clone();
        context.runtime().stack().with_frame(|frame| {
            table.push_into(frame)?;
            frame.set_global(LOWERING_GLOBAL)
        })
    }
}
