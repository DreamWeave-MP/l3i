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
//! Every loop is written once, over [`Span`]'s `get` and `set`. A contiguous `f32` span runs a
//! second instantiation of it with the kind and stride as constants, chosen once per operation
//! after the spans are checked: the dispatch folds away and LLVM vectorizes what remains, keeping
//! the sequential result under overlap with its own run-time check. The results are the same, bit
//! for bit, with one exception nothing ever promised: given two different NaN operands, which
//! payload survives an addition is the optimizer's operand order, here as in the generic loop,
//! which already answers differently in debug and release.
//!
//! ```lua
//! local data = require('@dream/data')
//! local total = data.sum(samples, 'f32', 0, n)
//! local hot = data.select(samples, 'f32', 0, n, 'gt', threshold)
//! local count = hot:count()
//! local order = buffer.create(n * 4)
//! data.argsort(samples, 'f32', 0, n, order)
//! data.gather(samples, 'f32', 0, n, order, sorted, 0)
//! ```
//!
//! The lowering of recognized JSL pipelines calls these same functions through the global
//! alias the extension installs (`__l3i_data`), with the slice's bounds already normalized by
//! the ordinary JSL prologue, so the explicit API and the recognized form share one
//! implementation and one semantic contract; the dream.data section of the site's Built-in
//! extensions page is the semantic matrix.

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
#[non_exhaustive]
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
#[non_exhaustive]
pub struct Layout {
    pub kind: Kind,
    pub stride: usize,
    /// Bytes from the span offset to the element: a field's position inside a record,
    /// `"f32@16+12"`. Zero for a plain kind.
    pub offset: usize,
}

impl Layout {
    /// Elements of `kind` every `stride` bytes, each `offset` bytes into its record.
    #[must_use]
    pub fn new(kind: Kind, stride: usize, offset: usize) -> Layout {
        Layout { kind, stride, offset }
    }

    pub fn parse(what: &str, name: &str) -> Result<Layout> {
        let (name, offset) = match name.split_once('+') {
            Some((name, offset)) => (
                name,
                offset.parse::<usize>().map_err(|_| {
                    Error::runtime(format!("{what}: field offset '{offset}' must be a whole number of bytes"))
                })?,
            ),
            None => (name, 0),
        };
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
        if offset.checked_add(kind.size()).is_none_or(|end| end > stride) {
            return Err(Error::runtime(format!(
                "{what}: field offset {offset} does not leave room for {} inside a stride of {stride}",
                kind.name()
            )));
        }
        Ok(Layout { kind, stride, offset })
    }

    pub fn contiguous(kind: Kind) -> Layout {
        Layout { kind, stride: kind.size(), offset: 0 }
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
        let Layout { kind, stride, offset: field } = layout;
        let offset = usize::try_from(offset.0)
            .map_err(|_| Error::runtime(format!("{what}: negative offset {}", offset.0)))?
            .checked_add(field)
            .ok_or_else(|| {
                Error::runtime(format!("{what}: offset {} plus field offset {field} overflows", offset.0))
            })?;
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

    /// Contiguous `f32`, the layout worth a loop of its own; see [`Span::contiguous_f32`].
    #[inline(always)]
    fn is_contiguous_f32(&self) -> bool {
        self.kind == Kind::F32 && self.stride == Kind::F32.size()
    }

    /// The same span with its kind and stride as constants. Every loop in this module is written
    /// once over `get` and `set`; inlined with these constants, the per-element dispatch in
    /// `Kind::read` and `Kind::write` folds away and LLVM can vectorize what is left. The call
    /// site still picks one instantiation per operation, after every span has been checked.
    ///
    /// The loop stays a loop over raw pointers: no reference to the storage exists, so overlap
    /// is not undefined behavior, and LLVM may only vectorize where it can keep the sequential
    /// result, which it does with a run-time overlap check that falls back to its scalar loop.
    #[inline(always)]
    fn contiguous_f32(self) -> Span {
        debug_assert!(self.is_contiguous_f32());
        Span { kind: Kind::F32, stride: Kind::F32.size(), ..self }
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

/// `$body` once per comparison, with `$comparison` rebound to a constant, so a loop in the body
/// has no dispatch left to do.
macro_rules! constant_comparison {
    ($comparison:ident => $body:expr) => {
        match $comparison {
            Comparison::Eq => {
                let $comparison = Comparison::Eq;
                $body
            }
            Comparison::Ne => {
                let $comparison = Comparison::Ne;
                $body
            }
            Comparison::Lt => {
                let $comparison = Comparison::Lt;
                $body
            }
            Comparison::Le => {
                let $comparison = Comparison::Le;
                $body
            }
            Comparison::Gt => {
                let $comparison = Comparison::Gt;
                $body
            }
            Comparison::Ge => {
                let $comparison = Comparison::Ge;
                $body
            }
        }
    };
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
#[repr(C)]
pub struct Selection {
    /// How many positions the selection covers; native code reads it at offset 0.
    len: Cell<usize>,
    /// The bitset's storage, re-synced whenever `bits` reallocates; native code reads it at
    /// offset 8 and writes words through it while the Vec is not otherwise borrowed (a native
    /// loop runs inside one call, which borrows nothing).
    words: Cell<*mut u64>,
    bits: RefCell<Vec<u64>>,
}

/// Byte offsets native code reads a selection at.
pub(crate) mod selection_layout {
    use super::Selection;

    pub const LEN: i32 = 0;
    pub const WORDS: i32 = 8;

    const _: () = {
        assert!(std::mem::offset_of!(Selection, len) == LEN as usize);
        assert!(std::mem::offset_of!(Selection, words) == WORDS as usize);
    };
}

// SAFETY: the payload holds no Lua references.
unsafe impl crate::userdata::Userdata for Selection {
    const NAME: &'static str = "dream.data.Selection";
}

impl Selection {
    pub fn new(len: usize) -> Selection {
        let mut bits = vec![0; len.div_ceil(64)];
        Selection { len: Cell::new(len), words: Cell::new(bits.as_mut_ptr()), bits: RefCell::new(bits) }
    }

    /// Replaces the storage and re-syncs the pointer native code reads.
    fn set_bits(&self, words: Vec<u64>) {
        let mut bits = self.bits.borrow_mut();
        *bits = words;
        self.words.set(bits.as_mut_ptr());
    }

    pub fn len(&self) -> usize {
        self.len.get()
    }

    pub fn is_empty(&self) -> bool {
        self.len.get() == 0
    }

    /// Resizes to `len` positions, all unselected.
    fn reset(&self, len: usize) {
        {
            let mut bits = self.bits.borrow_mut();
            bits.clear();
            bits.resize(len.div_ceil(64), 0);
            self.words.set(bits.as_mut_ptr());
        }
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

/// A module reduction: the span, then an optional comparison and threshold selecting the
/// elements it ranges over (`count`, `any` and `all` require them).
#[allow(clippy::too_many_arguments)]
fn reduce(
    call: &Call<'_>,
    what: &str,
    op: ReductionOp,
    buffer: &BufferView<'_>,
    kind: &str,
    offset: Exact<i64>,
    count: Exact<i64>,
    comparison: Option<&str>,
    threshold: Option<f64>,
) -> Result<StackResults> {
    let span = Span::new(what, buffer, Layout::parse(what, kind)?, offset, count)?;
    let filter = match (comparison, threshold) {
        (Some(comparison), Some(_)) => Some(Comparison::parse(what, comparison)?),
        (Some(_), None) => return Err(Error::runtime(format!("{what}: a comparison needs a threshold"))),
        (None, _) if matches!(op, ReductionOp::Count | ReductionOp::Any | ReductionOp::All) => {
            return Err(Error::runtime(format!("{what}: a comparison and threshold are required")));
        }
        (None, _) => None,
    };
    Reduction { op, filter }.run(&span, threshold.unwrap_or(0.0)).push(call)
}

#[allow(clippy::too_many_arguments)]
fn sum(
    call: &Call<'_>,
    buffer: BufferView<'_>,
    kind: &str,
    offset: Exact<i64>,
    count: Exact<i64>,
    comparison: Option<&str>,
    threshold: Option<f64>,
) -> Result<StackResults> {
    reduce(call, "data.sum", ReductionOp::Sum, &buffer, kind, offset, count, comparison, threshold)
}

#[allow(clippy::too_many_arguments)]
fn any(
    call: &Call<'_>,
    buffer: BufferView<'_>,
    kind: &str,
    offset: Exact<i64>,
    count: Exact<i64>,
    comparison: &str,
    threshold: f64,
) -> Result<StackResults> {
    reduce(call, "data.any", ReductionOp::Any, &buffer, kind, offset, count, Some(comparison), Some(threshold))
}

#[allow(clippy::too_many_arguments)]
fn all(
    call: &Call<'_>,
    buffer: BufferView<'_>,
    kind: &str,
    offset: Exact<i64>,
    count: Exact<i64>,
    comparison: &str,
    threshold: f64,
) -> Result<StackResults> {
    reduce(call, "data.all", ReductionOp::All, &buffer, kind, offset, count, Some(comparison), Some(threshold))
}

/// The index of the first least (or greatest) element by Luau `<` (`>`): a NaN that arrives
/// first stays, later NaNs never replace. None for an empty span.
fn extreme(span: &Span, greatest: bool) -> Option<usize> {
    if span.is_contiguous_f32() { extreme_f32(span, greatest) } else { extreme_any(span, greatest) }
}

fn extreme_any(span: &Span, greatest: bool) -> Option<usize> {
    extreme_elements(span, greatest)
}

#[inline(never)]
fn extreme_f32(span: &Span, greatest: bool) -> Option<usize> {
    let span = span.contiguous_f32();
    if greatest {
        extreme_blocks(&span, |candidate, value| candidate > value)
    } else {
        extreme_blocks(&span, |candidate, value| candidate < value)
    }
}

/// Elements a block-wise scan tests before it looks at the answer. Long enough that the test is
/// nearly all vector code; short enough that a hit costs little.
const SCAN_BLOCK: usize = 64;

/// [`extreme_elements`] with a block test in front: a branch on every element is what keeps that
/// loop scalar and mispredicting, and almost no block holds an improvement. A block that does is
/// rescanned by the same replace-when-strictly-better rule, in order, so a tie keeps the first
/// and a NaN (which nothing betters, and which betters nothing) behaves as it does there.
#[inline(always)]
fn extreme_blocks(span: &Span, better: impl Fn(f64, f64) -> bool) -> Option<usize> {
    if span.count == 0 {
        return None;
    }
    let (mut best, mut value, mut begin) = (0, span.get(0), 1);
    while begin < span.count {
        let end = (begin + SCAN_BLOCK).min(span.count);
        let mut improves = false;
        for i in begin..end {
            improves |= better(span.get(i), value);
        }
        if improves {
            for i in begin..end {
                let candidate = span.get(i);
                if better(candidate, value) {
                    best = i;
                    value = candidate;
                }
            }
        }
        begin = end;
    }
    Some(best)
}

/// Whether any element passes `test`, a block at a time. Stopping at the first deciding block
/// instead of the first deciding element is not observable: the test reads and nothing else.
#[inline(always)]
fn any_blocks(span: &Span, test: impl Fn(f64) -> bool) -> bool {
    let mut begin = 0;
    while begin < span.count {
        let end = (begin + SCAN_BLOCK).min(span.count);
        let mut found = false;
        for i in begin..end {
            found |= test(span.get(i));
        }
        if found {
            return true;
        }
        begin = end;
    }
    false
}

#[inline(always)]
fn extreme_elements(span: &Span, greatest: bool) -> Option<usize> {
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

#[allow(clippy::too_many_arguments)]
fn min(
    call: &Call<'_>,
    buffer: BufferView<'_>,
    kind: &str,
    offset: Exact<i64>,
    count: Exact<i64>,
    comparison: Option<&str>,
    threshold: Option<f64>,
) -> Result<StackResults> {
    reduce(call, "data.min", ReductionOp::Min, &buffer, kind, offset, count, comparison, threshold)
}

#[allow(clippy::too_many_arguments)]
fn max(
    call: &Call<'_>,
    buffer: BufferView<'_>,
    kind: &str,
    offset: Exact<i64>,
    count: Exact<i64>,
    comparison: Option<&str>,
    threshold: Option<f64>,
) -> Result<StackResults> {
    reduce(call, "data.max", ReductionOp::Max, &buffer, kind, offset, count, comparison, threshold)
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
    let span = Span::new("data.countWhere", &buffer, Layout::parse("data.countWhere", kind)?, offset, count)?;
    let comparison = Comparison::parse("data.countWhere", comparison)?;
    let selected = if span.is_contiguous_f32() {
        count_f32(span, comparison, threshold)
    } else {
        count_any(span, comparison, threshold)
    };
    Ok(selected as f64)
}

#[inline(always)]
fn count_elements(span: Span, comparison: Comparison, threshold: f64) -> usize {
    let mut selected = 0usize;
    for i in 0..span.count {
        selected += usize::from(comparison.test(span.get(i), threshold));
    }
    selected
}

fn count_any(span: Span, comparison: Comparison, threshold: f64) -> usize {
    count_elements(span, comparison, threshold)
}

#[inline(never)]
fn count_f32(span: Span, comparison: Comparison, threshold: f64) -> usize {
    let span = span.contiguous_f32();
    constant_comparison!(comparison => count_elements(span, comparison, threshold))
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
    let span = Span::new("data.select", &buffer, Layout::parse("data.select", kind)?, offset, count)?;
    let comparison = Comparison::parse("data.select", comparison)?;
    let target = Target::prepare(into, span.count);
    select_words(&span, comparison, threshold, &mut target.selection().bits.borrow_mut());
    target.finish(call, 7)
}

/// Writes the comparison's result for each element into `words`, which hold one bit per element.
fn select_words(span: &Span, comparison: Comparison, threshold: f64, words: &mut [u64]) {
    if span.is_contiguous_f32() {
        select_f32(span, comparison, threshold, words);
    } else {
        select_any(span, comparison, threshold, words);
    }
}

#[inline(always)]
fn select_elements(span: &Span, comparison: Comparison, threshold: f64, words: &mut [u64]) {
    for (word, chunk) in words.iter_mut().zip((0..span.count).step_by(64)) {
        let mut mask = 0u64;
        for bit in 0..64.min(span.count - chunk) {
            mask |= u64::from(comparison.test(span.get(chunk + bit), threshold)) << bit;
        }
        *word = mask;
    }
}

fn select_any(span: &Span, comparison: Comparison, threshold: f64, words: &mut [u64]) {
    select_elements(span, comparison, threshold, words);
}

#[inline(never)]
fn select_f32(span: &Span, comparison: Comparison, threshold: f64, words: &mut [u64]) {
    let span = span.contiguous_f32();
    constant_comparison!(comparison => select_elements(&span, comparison, threshold, words));
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
    target.selection().set_bits(words);
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
            target.selection().set_bits(words);
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
    if span.is_contiguous_f32() {
        fill_f32(span, value);
    } else {
        fill_any(span, value);
    }
    Ok(())
}

#[inline(always)]
fn fill_elements(span: Span, value: f64) {
    for i in 0..span.count {
        span.set(i, value);
    }
}

fn fill_any(span: Span, value: f64) {
    fill_elements(span, value);
}

#[inline(never)]
fn fill_f32(span: Span, value: f64) {
    fill_elements(span.contiguous_f32(), value);
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
    if out.is_contiguous_f32() {
        add_f32(left, right, out);
    } else {
        add_any(left, right, out);
    }
    Ok(())
}

#[inline(always)]
fn add_elements(left: Span, right: Span, out: Span) {
    for i in 0..out.count {
        out.set(i, left.get(i) + right.get(i));
    }
}

fn add_any(left: Span, right: Span, out: Span) {
    add_elements(left, right, out);
}

#[inline(never)]
fn add_f32(left: Span, right: Span, out: Span) {
    let (left, right, out) = (left.contiguous_f32(), right.contiguous_f32(), out.contiguous_f32());
    // In place is the ordinary case and an overlap check would refuse it. Reading and writing
    // through one pointer tells LLVM the distance is zero, so that loop vectorizes too.
    if left.base == out.base {
        add_elements(out, right, out);
    } else if right.base == out.base {
        add_elements(left, out, out);
    } else {
        add_elements(left, right, out);
    }
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
    if out.is_contiguous_f32() {
        scale_f32(source, factor, out);
    } else {
        scale_any(source, factor, out);
    }
    Ok(())
}

#[inline(always)]
fn scale_elements(source: Span, factor: f64, out: Span) {
    for i in 0..out.count {
        out.set(i, source.get(i) * factor);
    }
}

fn scale_any(source: Span, factor: f64, out: Span) {
    scale_elements(source, factor, out);
}

#[inline(never)]
fn scale_f32(source: Span, factor: f64, out: Span) {
    let (source, out) = (source.contiguous_f32(), out.contiguous_f32());
    // As `add_f32`: one pointer for in place.
    if source.base == out.base {
        scale_elements(out, factor, out);
    } else {
        scale_elements(source, factor, out);
    }
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
    if out.is_contiguous_f32() {
        clamp_f32(source, low, high, out);
    } else {
        clamp_any(source, low, high, out);
    }
    Ok(())
}

#[inline(always)]
fn clamp_elements(source: Span, low: f64, high: f64, out: Span) {
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
}

fn clamp_any(source: Span, low: f64, high: f64, out: Span) {
    clamp_elements(source, low, high, out);
}

#[inline(never)]
fn clamp_f32(source: Span, low: f64, high: f64, out: Span) {
    let (source, out) = (source.contiguous_f32(), out.contiguous_f32());
    // As `add_f32`: one pointer for in place.
    if source.base == out.base {
        clamp_elements(out, low, high, out);
    } else {
        clamp_elements(source, low, high, out);
    }
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
// The typed receivers: the same reductions as methods, lowered to native loops under jit
// ---------------------------------------------------------------------------------------------

/// Words of scratch in a receiver's payload for the native lowering's loop state.
const RECEIVER_SCRATCH_WORDS: usize = 5;

/// The kinds by index, the order [`Kind`] is declared in; `Typed<K>` is the receiver for
/// `KINDS[K]`.
pub const KINDS: [Kind; 8] = [Kind::U8, Kind::I8, Kind::U16, Kind::I16, Kind::U32, Kind::I32, Kind::F32, Kind::F64];
const KIND_TYPE_NAMES: [&str; 8] = [
    "dream.data.Kind.u8",
    "dream.data.Kind.i8",
    "dream.data.Kind.u16",
    "dream.data.Kind.i16",
    "dream.data.Kind.u32",
    "dream.data.Kind.i32",
    "dream.data.Kind.f32",
    "dream.data.Kind.f64",
];

/// `dream.data.Kind.<kind>`, from `data.u8()` and friends (or `data.kind(name)`): a receiver
/// whose methods are the span reductions for one element kind, `K:sum(buffer, offset, count)`,
/// `K:countGt(buffer, offset, count, threshold)` and so on. The element kind is part of the
/// *type*, so under `jit` a call on a receiver the script annotated (`local F: dream_data_Kind_f32
/// = data.f32()`) lowers to exactly one native loop pair ([`lowering::KindLowering`]); the
/// stride is a run-time property of the value. The bound methods are the semantic path.
#[repr(C)]
pub struct Typed<const K: u8> {
    kind: u64,
    /// The stride in bytes, as a double so native code adds it to its cursor directly.
    stride: f64,
    scratch: [Cell<u64>; RECEIVER_SCRATCH_WORDS],
}

// SAFETY: the payload holds no Lua references.
unsafe impl<const K: u8> crate::userdata::Userdata for Typed<K> {
    const NAME: &'static str = KIND_TYPE_NAMES[K as usize];
}

impl<const K: u8> Typed<K> {
    pub const KIND: Kind = KINDS[K as usize];

    pub fn new(stride: usize) -> Typed<K> {
        Typed { kind: u64::from(K), stride: stride as f64, scratch: Default::default() }
    }

    pub fn layout(&self) -> Layout {
        Layout { kind: Self::KIND, stride: self.stride as usize, offset: 0 }
    }

    fn span(&self, what: &str, buffer: &BufferView<'_>, offset: Exact<i64>, count: Exact<i64>) -> Result<Span> {
        Span::new(what, buffer, self.layout(), offset, count)
    }
}

/// Byte offsets native code reads at, pinned here at compile time.
pub(crate) mod layout {
    use super::Typed;

    pub const KIND: i32 = 0;
    pub const STRIDE: i32 = 8;
    pub const SCRATCH: i32 = 16;

    const _: () = {
        assert!(std::mem::offset_of!(Typed<0>, kind) == KIND as usize);
        assert!(std::mem::offset_of!(Typed<0>, stride) == STRIDE as usize);
        assert!(std::mem::offset_of!(Typed<0>, scratch) == SCRATCH as usize);
    };
}

fn describe_typed<const K: u8>(d: &mut ExtensionDescriptor) {
    let mut receiver = d.userdata::<Typed<K>>(KIND_TYPE_NAMES[K as usize]);
    receiver.tag(TagPolicy::Preferred).compiler_type(CompilerTypePolicy::Preferred).doc(format!(
        "The span reductions for {} elements as methods; native loops under jit.",
        KINDS[K as usize].name()
    ));
    for (op, filter) in Reduction::MEMBERS {
        let name = op.member_name(filter);
        let what = format!("Kind:{name}");
        let reduction = Reduction { op, filter };
        receiver
            .method(
                name.as_str(),
                move |receiver: &Typed<K>,
                      call: &Call<'_>,
                      buffer: BufferView<'_>,
                      offset: Exact<i64>,
                      count: Exact<i64>,
                      threshold: Option<f64>|
                      -> Result<StackResults> {
                    let span = receiver.span(&what, &buffer, offset, count)?;
                    let threshold = match (filter, threshold) {
                        (Some(_), Some(threshold)) => threshold,
                        (Some(_), None) => return Err(Error::runtime(format!("{what}: missing threshold"))),
                        (None, _) => 0.0,
                    };
                    reduction.run(&span, threshold).push(call)
                },
            )
            .signature(if filter.is_some() {
                format!(
                    "(self, buffer: buffer, offset: number, count: number, threshold: number): {}",
                    op.result_type()
                )
            } else {
                format!("(self, buffer: buffer, offset: number, count: number): {}", op.result_type())
            })
            .doc(op.doc(filter));
    }
    describe_select::<K>(&mut receiver);
}

/// `K:selectGt(buffer, offset, count, threshold, selection)`: the positions whose element
/// compares so, written into the caller's selection (resized to `count`), which is returned.
fn describe_select<const K: u8>(receiver: &mut crate::extension::UserdataBuilder<'_, Typed<K>>) {
    for comparison in Comparison::ALL {
        let name = format!("select{}", comparison.suffix());
        let what = format!("Kind:{name}");
        receiver
            .method(
                name.as_str(),
                move |receiver: &Typed<K>,
                      call: &Call<'_>,
                      buffer: BufferView<'_>,
                      offset: Exact<i64>,
                      count: Exact<i64>,
                      threshold: f64,
                      selection: &Selection|
                      -> Result<StackResults> {
                    let span = receiver.span(&what, &buffer, offset, count)?;
                    selection.reset(span.count);
                    select_words(&span, comparison, threshold, &mut selection.bits.borrow_mut());
                    Push::push_only(&call.arg(6), call)?;
                    Ok(StackResults)
                },
            )
            .signature("(self, buffer: buffer, offset: number, count: number, threshold: number, selection: dream_data_Selection): dream_data_Selection")
            .doc(format!("The positions whose element compares {} against threshold, written into selection (resized to count) and returned.", comparison.name()));
    }
}

/// The stride a receiver constructor was given, validated for its kind.
fn receiver_stride(what: &str, kind: Kind, stride: Option<Exact<i64>>) -> Result<usize> {
    match stride {
        None => Ok(kind.size()),
        Some(stride) => usize::try_from(stride.0).ok().filter(|stride| *stride >= kind.size()).ok_or_else(|| {
            Error::runtime(format!("{what}: stride must be at least {} bytes for {}", kind.size(), kind.name()))
        }),
    }
}

/// `data.kind("f32@16")`: the receiver for a layout, as a value of the matching typed class.
fn kind(call: &Call<'_>, name: &str) -> Result<StackResults> {
    let layout = Layout::parse("data.kind", name)?;
    macro_rules! push {
        ($($index:literal),*) => {
            match layout.kind {
                $(kind if kind == KINDS[$index] => crate::userdata::push_owned(call.stack(), Typed::<$index>::new(layout.stride))?,)*
                _ => unreachable!("every kind has a receiver type"),
            }
        };
    }
    push!(0, 1, 2, 3, 4, 5, 6, 7);
    Ok(StackResults)
}

fn describe_receivers(d: &mut ExtensionDescriptor) {
    describe_typed::<0>(d);
    describe_typed::<1>(d);
    describe_typed::<2>(d);
    describe_typed::<3>(d);
    describe_typed::<4>(d);
    describe_typed::<5>(d);
    describe_typed::<6>(d);
    describe_typed::<7>(d);
    d.type_alias(
        "dream_data_Kind",
        "dream_data_Kind_u8 | dream_data_Kind_i8 | dream_data_Kind_u16 | dream_data_Kind_i16 | dream_data_Kind_u32 | dream_data_Kind_i32 | dream_data_Kind_f32 | dream_data_Kind_f64",
    );
}

/// `data.u8(stride?)` and friends: a receiver whose static type names its kind.
fn describe_constructors(module: &mut ModuleDecl) {
    macro_rules! constructor {
        ($index:literal, $name:literal) => {
            module
                .function($name, |stride: Option<Exact<i64>>| -> Result<Owned<Typed<$index>>> {
                    Ok(Owned(Typed::<$index>::new(receiver_stride(concat!("data.", $name), KINDS[$index], stride)?)))
                })
                .signature(concat!("(stride: number?) -> dream_data_Kind_", $name))
                .doc(concat!("The ", $name, " receiver, contiguous or at the given byte stride; annotate the local with its type so jit lowers its calls."));
        };
    }
    constructor!(0, "u8");
    constructor!(1, "i8");
    constructor!(2, "u16");
    constructor!(3, "i16");
    constructor!(4, "u32");
    constructor!(5, "i32");
    constructor!(6, "f32");
    constructor!(7, "f64");
}

/// A reduction over the elements that satisfy an optional comparison: the receiver's members.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum ReductionOp {
    Sum,
    Min,
    Max,
    Count,
    Any,
    All,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct Reduction {
    pub op: ReductionOp,
    pub filter: Option<Comparison>,
}

impl Reduction {
    /// `op` over every element, or over the elements a comparison with the threshold accepts.
    #[must_use]
    pub fn new(op: ReductionOp, filter: Option<Comparison>) -> Reduction {
        Reduction { op, filter }
    }
}

/// What a reduction answers.
enum Answer {
    Number(f64),
    Nil,
    Bool(bool),
}

impl Answer {
    fn push(self, call: &Call<'_>) -> Result<StackResults> {
        match self {
            Answer::Number(value) => Push::push_only(&value, call)?,
            Answer::Nil => {
                call.stack().push_nil()?;
            }
            Answer::Bool(value) => Push::push_only(&value, call)?,
        }
        Ok(StackResults)
    }
}

impl ReductionOp {
    const ALL: [ReductionOp; 6] =
        [ReductionOp::Sum, ReductionOp::Min, ReductionOp::Max, ReductionOp::Count, ReductionOp::Any, ReductionOp::All];

    fn name(self) -> &'static str {
        match self {
            ReductionOp::Sum => "sum",
            ReductionOp::Min => "min",
            ReductionOp::Max => "max",
            ReductionOp::Count => "count",
            ReductionOp::Any => "any",
            ReductionOp::All => "all",
        }
    }

    /// `sum`, `sumGt`, `countEq`, `anyLt`: the operation then the capitalized comparison.
    pub fn member_name(self, filter: Option<Comparison>) -> String {
        match filter {
            None => self.name().to_owned(),
            Some(comparison) => format!("{}{}", self.name(), comparison.suffix()),
        }
    }

    fn result_type(self) -> &'static str {
        match self {
            ReductionOp::Sum | ReductionOp::Count => "number",
            ReductionOp::Min | ReductionOp::Max => "number?",
            ReductionOp::Any | ReductionOp::All => "boolean",
        }
    }

    fn doc(self, filter: Option<Comparison>) -> String {
        let subject = match filter {
            None => "every element".to_owned(),
            Some(comparison) => format!("the elements that compare {} against threshold", comparison.name()),
        };
        match self {
            ReductionOp::Sum => format!("The sum of {subject}, added in order into a Luau number; 0 when none."),
            ReductionOp::Min => {
                format!("The first least of {subject} by Luau <, or nil when none; a leading NaN stays.")
            }
            ReductionOp::Max => format!("The first greatest of {subject} by Luau >, or nil when none."),
            ReductionOp::Count => format!("How many of {subject}."),
            ReductionOp::Any => format!("Whether any of {subject} exists; stops at the first."),
            ReductionOp::All => format!(
                "Whether every element compares {} against threshold; stops at the first that does not.",
                filter.map_or("", Comparison::name)
            ),
        }
    }
}

impl Comparison {
    pub const ALL: [Comparison; 6] =
        [Comparison::Eq, Comparison::Ne, Comparison::Lt, Comparison::Le, Comparison::Gt, Comparison::Ge];

    pub fn name(self) -> &'static str {
        match self {
            Comparison::Eq => "eq",
            Comparison::Ne => "ne",
            Comparison::Lt => "lt",
            Comparison::Le => "le",
            Comparison::Gt => "gt",
            Comparison::Ge => "ge",
        }
    }

    pub fn suffix(self) -> &'static str {
        match self {
            Comparison::Eq => "Eq",
            Comparison::Ne => "Ne",
            Comparison::Lt => "Lt",
            Comparison::Le => "Le",
            Comparison::Gt => "Gt",
            Comparison::Ge => "Ge",
        }
    }
}

impl Reduction {
    /// Every member: the unfiltered sum/min/max, and each operation with each comparison
    /// (count, any and all need one).
    const MEMBERS: [(ReductionOp, Option<Comparison>); 39] = {
        let mut members = [(ReductionOp::Sum, None); 39];
        members[1] = (ReductionOp::Min, None);
        members[2] = (ReductionOp::Max, None);
        let mut at = 3;
        let mut op = 0;
        while op < 6 {
            let mut comparison = 0;
            while comparison < 6 {
                members[at] = (ReductionOp::ALL[op], Some(Comparison::ALL[comparison]));
                at += 1;
                comparison += 1;
            }
            op += 1;
        }
        members
    };

    /// The bound semantics, element by element in order: the oracle for the native loops.
    fn run(self, span: &Span, threshold: f64) -> Answer {
        if span.is_contiguous_f32() { self.run_f32(span, threshold) } else { self.run_any(span, threshold) }
    }

    fn run_any(self, span: &Span, threshold: f64) -> Answer {
        self.run_elements(span, threshold)
    }

    #[inline(never)]
    fn run_f32(self, span: &Span, threshold: f64) -> Answer {
        let span = span.contiguous_f32();
        match (self.op, self.filter) {
            (ReductionOp::Min | ReductionOp::Max, None) => {
                extreme_f32(&span, self.op == ReductionOp::Max).map_or(Answer::Nil, |i| Answer::Number(span.get(i)))
            }
            (ReductionOp::Count, Some(comparison)) => Answer::Number(count_f32(span, comparison, threshold) as f64),
            (ReductionOp::Any, Some(comparison)) => Answer::Bool(
                constant_comparison!(comparison => any_blocks(&span, |value| comparison.test(value, threshold))),
            ),
            (ReductionOp::All, Some(comparison)) => Answer::Bool(
                !constant_comparison!(comparison => any_blocks(&span, |value| !comparison.test(value, threshold))),
            ),
            // A running sum in order is a chain of additions no vector can shorten, and a filtered
            // minimum or maximum has two conditions per element: the constant kind is all they get.
            _ => self.run_elements(&span, threshold),
        }
    }

    #[inline(always)]
    fn run_elements(self, span: &Span, threshold: f64) -> Answer {
        let accept = |value: f64| self.filter.is_none_or(|comparison| comparison.test(value, threshold));
        match self.op {
            ReductionOp::Sum => {
                let mut total = 0.0;
                for i in 0..span.count {
                    let value = span.get(i);
                    if accept(value) {
                        total += value;
                    }
                }
                Answer::Number(total)
            }
            ReductionOp::Count => Answer::Number((0..span.count).filter(|&i| accept(span.get(i))).count() as f64),
            ReductionOp::Min | ReductionOp::Max if self.filter.is_none() => {
                // The specialized scan: half the instructions of the generic path below.
                extreme_elements(span, self.op == ReductionOp::Max).map_or(Answer::Nil, |i| Answer::Number(span.get(i)))
            }
            ReductionOp::Min | ReductionOp::Max => {
                let mut best: Option<f64> = None;
                for i in 0..span.count {
                    let value = span.get(i);
                    if !accept(value) {
                        continue;
                    }
                    best = Some(match best {
                        None => value,
                        Some(current) => {
                            if if self.op == ReductionOp::Min { value < current } else { value > current } {
                                value
                            } else {
                                current
                            }
                        }
                    });
                }
                best.map_or(Answer::Nil, Answer::Number)
            }
            ReductionOp::Any => Answer::Bool((0..span.count).any(|i| accept(span.get(i)))),
            ReductionOp::All => Answer::Bool((0..span.count).all(|i| accept(span.get(i)))),
        }
    }
}

// ---------------------------------------------------------------------------------------------
// The extension
// ---------------------------------------------------------------------------------------------

fn describe_module(module: &mut ModuleDecl) {
    // `kind` is an element kind or a layout `kind@stride` (bytes between elements).
    const SPAN: &str = "buffer: buffer, kind: dream_data_Layout, offset: number, count: number";
    module
        .function("kind", kind)
        .signature("(kind: dream_data_Layout) -> dream_data_Kind")
        .doc("The receiver for a kind or layout named at run time (one of the typed receivers); prefer data.u8() and friends, whose static type lets jit lower the calls.")
        .function("sum", sum)
        .signature(format!("({SPAN}, comparison: dream_data_Comparison?, threshold: number?) -> number"))
        .doc("The elements (those satisfying the comparison, when given) added in order into a Luau number; 0 when none.")
        .function("min", min)
        .signature(format!("({SPAN}, comparison: dream_data_Comparison?, threshold: number?) -> number?"))
        .doc("The first least element (among those satisfying the comparison, when given) by Luau <, or nil when none; a leading NaN stays.")
        .function("max", max)
        .signature(format!("({SPAN}, comparison: dream_data_Comparison?, threshold: number?) -> number?"))
        .doc("The first greatest element (among those satisfying the comparison, when given) by Luau >, or nil when none.")
        .function("any", any)
        .signature(format!("({SPAN}, comparison: dream_data_Comparison, threshold: number) -> boolean"))
        .doc("Whether any element satisfies the comparison; stops at the first.")
        .function("all", all)
        .signature(format!("({SPAN}, comparison: dream_data_Comparison, threshold: number) -> boolean"))
        .doc("Whether every element satisfies the comparison; stops at the first that does not.")
        .function("argmin", argmin)
        .signature(format!("({SPAN}) -> number?"))
        .doc("The zero-based position of the first least element, or nil.")
        .function("argmax", argmax)
        .signature(format!("({SPAN}) -> number?"))
        .doc("The zero-based position of the first greatest element, or nil.")
        .function("countWhere", count)
        .signature(format!("({SPAN}, comparison: dream_data_Comparison, threshold: number) -> number"))
        .doc("How many elements satisfy the comparison against threshold.")
        .function("select", compare)
        .signature(format!(
            "({SPAN}, comparison: dream_data_Comparison, threshold: number, into: dream_data_Selection?) -> dream_data_Selection"
        ))
        .doc("The positions whose element satisfies the comparison; written into `into` when given.")
        .function("selection", selection)
        .signature("(len: number) -> dream_data_Selection")
        .doc("An empty selection over len positions, for reuse as `into`.")
        .function("gather", gather)
        .signature("(source: buffer, kind: dream_data_Layout,sourceOffset: number, sourceCount: number, indices: buffer, destination: buffer, destinationOffset: number, indexCount: number?) -> number")
        .doc("destination[i] = source[indices[i]] for every u32 index (all of the indices buffer by default); the count written. The source may be a strided field; the destination column is contiguous. Every index is checked before anything is written.")
        .function("scatter", scatter)
        .signature("(source: buffer, kind: dream_data_Layout,sourceOffset: number, sourceCount: number, indices: buffer, destination: buffer, destinationOffset: number, destinationCount: number) -> number")
        .doc("destination[indices[i]] = source[i] for every source element; the count written. The source column is contiguous; the destination may be a strided field. Every index is checked against destinationCount before anything is written.")
        .function("fill", fill)
        .signature(format!("({SPAN}, value: number) -> ()"))
        .doc("Every element set to value, a number, written as every data-plane write is: an integer kind truncates toward zero and wraps to the width (NaN is zero, beyond 2^63 saturates first), f32 rounds.")
        .function("add", add)
        .signature("(left: buffer, kind: dream_data_Layout,leftOffset: number, right: buffer, rightOffset: number, out: buffer, outOffset: number, count: number) -> ()")
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
        // What a span's `kind` takes: a bare kind, or one with a stride and a field offset
        // (`"f32@16+12"`), which no union of literals can spell.
        d.type_alias("dream_data_Layout", "dream_data_ElementKind | string");
        d.type_alias("dream_data_Comparison", "\"eq\" | \"ne\" | \"lt\" | \"le\" | \"gt\" | \"ge\"");
        describe_selection(d);
        describe_receivers(d);
        #[cfg(feature = "jit")]
        d.native_hooks(lowering::KindLowering);
        let module = d.module(MODULE);
        module.doc("Typed spans of buffers reduced, compared into selections, gathered, scattered, filled, combined, ordered and partitioned natively, with no per-element callbacks.");
        describe_module(module);
        describe_constructors(module);
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

/// The two instantiations of each loop held to each other, byte for byte. The integration tests
/// compare the public functions with handwritten Luau; these compare what LLVM made of one source
/// under a constant kind with what it made of it under a run-time kind, including which NaN
/// payload survives. They only test anything optimized: run them with `--release` as well.
#[cfg(test)]
mod f32_paths {
    use super::{
        Answer, Comparison, Kind, Reduction, ReductionOp, Span, add_any, add_f32, clamp_any, clamp_f32, count_any,
        count_f32, extreme_any, extreme_f32, fill_any, fill_f32, scale_any, scale_f32, select_any, select_f32,
    };

    const PATTERNS: [u32; 26] = [
        0x0000_0000,
        0x8000_0000,
        0x7f80_0000,
        0xff80_0000,
        0x7fc0_0000,
        0xffc0_0000,
        0x7fc1_2345,
        0xffc5_4321,
        0x7f80_0001,
        0xff81_2345,
        0x0000_0001,
        0x8000_0001,
        0x007f_ffff,
        0x807f_ffff,
        0x0080_0000,
        0x7f7f_ffff,
        0xff7f_ffff,
        0x7f00_0000,
        0x3f80_0000,
        0x3f80_0001,
        0x3f7f_ffff,
        0xbf80_0000,
        0x3380_0000,
        0x3300_0000,
        0x4b80_0000,
        0x4b80_0001,
    ];
    const LENGTHS: [usize; 16] = [0, 1, 2, 3, 4, 5, 7, 8, 9, 15, 16, 17, 33, 64, 65, 257];
    /// Output placement relative to an input, in bytes: identical, overlapping either way by
    /// whole or split elements, and beyond any vector width.
    const SHIFTS: [isize; 17] = [-132, -68, -64, -9, -8, -5, -4, -2, -1, 0, 1, 2, 4, 5, 8, 64, 132];
    const MIDDLE: usize = 136;

    struct Words(u32);

    impl Words {
        /// The numerical recipes LCG: deterministic, and the words go straight into buffers.
        fn next(&mut self) -> u32 {
            self.0 = self.0.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            self.0
        }

        fn corpus(&mut self, words: usize) -> Vec<u8> {
            (0..words)
                .flat_map(|_| {
                    let pick = self.next() as usize % (PATTERNS.len() * 2);
                    let word = if pick < PATTERNS.len() { PATTERNS[pick] } else { self.next() };
                    word.to_ne_bytes()
                })
                .collect()
        }
    }

    fn span(bytes: &mut [u8], at: usize, count: usize) -> Span {
        assert!(at + count * 4 <= bytes.len());
        // SAFETY: inside the slice, checked above; the slice outlives the loop that uses the span
        // and no reference to it is used while the loop runs.
        Span { base: unsafe { bytes.as_mut_ptr().add(at) }, kind: Kind::F32, stride: 4, count }
    }

    /// Makes `count` words from `at` finite. With two different NaNs the surviving payload is
    /// whichever operand the optimizer put first, and the two paths may differ (the generic one
    /// differs from itself between debug and release); the integration tests hold that case to
    /// "a quieted copy of one of them". A finite operand leaves one NaN at most, which determines
    /// every result, even where an overlapping write splits the words of the other operand.
    fn finite(bytes: &mut [u8], at: usize, count: usize) {
        for word in bytes[at..at + count * 4].as_chunks_mut::<4>().0 {
            let bits = u32::from_ne_bytes(*word);
            if bits & 0x7f80_0000 == 0x7f80_0000 {
                *word = (bits & 0xbfff_ffff).to_ne_bytes();
            }
        }
    }

    fn first_difference(a: &[u8], b: &[u8]) -> Option<usize> {
        a.iter().zip(b).position(|(a, b)| a != b)
    }

    /// Runs `operation` on two copies of `base`, one through each path, and compares every byte.
    fn agree(what: &str, base: &[u8], operation: impl Fn(&mut [u8], bool)) {
        let (mut fast, mut generic) = (base.to_vec(), base.to_vec());
        operation(&mut fast, true);
        operation(&mut generic, false);
        if let Some(at) = first_difference(&fast, &generic) {
            let word = at & !3;
            let read = |bytes: &[u8]| u32::from_ne_bytes(bytes[word..word + 4].try_into().unwrap());
            panic!("{what}: byte {at}: f32 path {:#010x}, generic path {:#010x}", read(&fast), read(&generic));
        }
    }

    fn size(count: usize) -> usize {
        MIDDLE * 2 + (count + 40) * 4
    }

    #[test]
    fn add_agrees_at_every_placement() {
        let mut words = Words(0x2545_f491);
        for count in LENGTHS {
            // Past anything the output can reach, the other operand when only one overlaps.
            let size = (size(count) + count * 4) / 4 * 4;
            let mut base = words.corpus(size / 4);
            let far = size - (count + 1) * 4;
            finite(&mut base, far, count);
            for shift in SHIFTS {
                let out = MIDDLE.wrapping_add_signed(shift);
                for (place, left, right) in [("left", MIDDLE, far), ("right", far, MIDDLE), ("both", MIDDLE, MIDDLE)] {
                    agree(&format!("add count={count} shift={shift} {place}"), &base, |bytes, fast| {
                        let (l, r, o) = (span(bytes, left, count), span(bytes, right, count), span(bytes, out, count));
                        if fast { add_f32(l, r, o) } else { add_any(l, r, o) }
                    });
                }
            }
        }
    }

    #[test]
    fn scale_agrees_at_every_placement_and_factor() {
        let mut words = Words(0x1234_5678);
        for count in LENGTHS {
            let size = size(count) / 4 * 4;
            let base = words.corpus(size / 4);
            for factor in [1.0 / 3.0, 0.5, -1.0, 0.0, -0.0, 1e300, 1e-320, 1e39, f64::INFINITY] {
                for shift in SHIFTS {
                    let out = MIDDLE.wrapping_add_signed(shift);
                    agree(&format!("scale count={count} factor={factor} shift={shift}"), &base, |bytes, fast| {
                        let (s, o) = (span(bytes, MIDDLE, count), span(bytes, out, count));
                        if fast { scale_f32(s, factor, o) } else { scale_any(s, factor, o) }
                    });
                }
            }
        }
    }

    #[test]
    fn clamp_agrees_at_every_placement_and_bound() {
        let mut words = Words(0x0bad_cafe);
        let bounds = [
            (-5.0, 5.0),
            (0.0, 0.0),
            (-0.0, 0.0),
            (0.0, -0.0),
            (0.1, 0.2),
            (f64::NEG_INFINITY, f64::INFINITY),
            (1.0 + 2f64.powi(-30), 1.0 + 2f64.powi(-29)),
            (1.0 + 2f64.powi(-24), 1.0 + 2f64.powi(-24)),
            (1e39, 1e40),
            (-1e-50, 1e-50),
        ];
        for count in LENGTHS {
            let size = size(count) / 4 * 4;
            let base = words.corpus(size / 4);
            for (low, high) in bounds {
                for shift in SHIFTS {
                    let out = MIDDLE.wrapping_add_signed(shift);
                    agree(&format!("clamp count={count} [{low}, {high}] shift={shift}"), &base, |bytes, fast| {
                        let (s, o) = (span(bytes, MIDDLE, count), span(bytes, out, count));
                        if fast { clamp_f32(s, low, high, o) } else { clamp_any(s, low, high, o) }
                    });
                }
            }
        }
    }

    #[test]
    fn fill_agrees_for_every_value() {
        for count in LENGTHS {
            let base = vec![0xa5; size(count)];
            for at in [0, 1, 2, 3, 4] {
                for value in [1.5, 0.1, -0.0, f64::NAN, -f64::NAN, f64::INFINITY, 1e39, -1e-46, 1.0 + 2f64.powi(-24)] {
                    agree(&format!("fill count={count} at={at} value={value}"), &base, |bytes, fast| {
                        let s = span(bytes, at, count);
                        if fast { fill_f32(s, value) } else { fill_any(s, value) }
                    });
                }
            }
        }
    }

    /// An answer as bits, so that which NaN and which zero came back counts.
    fn answer(answer: &Answer) -> (u8, u64) {
        match answer {
            Answer::Number(value) => (0, value.to_bits()),
            Answer::Nil => (1, 0),
            Answer::Bool(value) => (2, u64::from(*value)),
        }
    }

    const THRESHOLDS: [f64; 10] = [0.0, -0.0, 1.0, 0.1, -1.0, 1e39, -1e39, 1e-46, f64::INFINITY, f64::NAN];
    /// Block boundaries and their neighbours, for the scans that work a block at a time.
    const SCAN_LENGTHS: [usize; 15] = [0, 1, 2, 63, 64, 65, 66, 127, 128, 129, 130, 191, 192, 193, 257];

    /// Every reduction member, comparison and threshold through both paths, on `bytes`.
    fn reductions_agree(what: &str, bytes: &mut [u8], at: usize, count: usize) {
        let s = span(bytes, at, count);
        assert_eq!(extreme_f32(&s, false), extreme_any(&s, false), "argmin {what}");
        assert_eq!(extreme_f32(&s, true), extreme_any(&s, true), "argmax {what}");
        let ops = [
            ReductionOp::Sum,
            ReductionOp::Min,
            ReductionOp::Max,
            ReductionOp::Count,
            ReductionOp::Any,
            ReductionOp::All,
        ];
        for op in [ReductionOp::Sum, ReductionOp::Min, ReductionOp::Max] {
            let reduction = Reduction::new(op, None);
            assert_eq!(answer(&reduction.run_f32(&s, 0.0)), answer(&reduction.run_any(&s, 0.0)), "{op:?} {what}");
        }
        for comparison in Comparison::ALL {
            for threshold in THRESHOLDS {
                let label = format!("{comparison:?} {threshold} {what}");
                for op in ops {
                    let reduction = Reduction::new(op, Some(comparison));
                    let (fast, generic) = (reduction.run_f32(&s, threshold), reduction.run_any(&s, threshold));
                    assert_eq!(answer(&fast), answer(&generic), "{op:?} {label}");
                }
                assert_eq!(count_f32(s, comparison, threshold), count_any(s, comparison, threshold), "count {label}");
                let mut fast = vec![u64::MAX; count.div_ceil(64)];
                let mut generic = vec![0; count.div_ceil(64)];
                select_f32(&s, comparison, threshold, &mut fast);
                select_any(&s, comparison, threshold, &mut generic);
                assert_eq!(fast, generic, "select {label}");
            }
        }
    }

    #[test]
    fn reductions_agree_on_the_corpus() {
        let mut words = Words(0x0dea_df00);
        for count in LENGTHS.into_iter().chain(SCAN_LENGTHS) {
            for at in [0, 1, 2, 3] {
                let mut bytes = vec![0; at];
                bytes.extend(words.corpus(count));
                reductions_agree(&format!("count={count} at={at}"), &mut bytes, at, count);
            }
        }
    }

    /// One deciding element at every position around the blocks, over a constant background: the
    /// improvement a block test must not miss, the tie it must not take, the NaN it must ignore.
    #[test]
    fn reductions_agree_with_one_deciding_element_anywhere() {
        let backgrounds = [1.0f32, 0.0, -0.0, f32::NAN];
        let deciders = [-2.0f32, 2.0, 1.0, 0.0, -0.0, f32::NAN, f32::INFINITY, f32::NEG_INFINITY, f32::from_bits(1)];
        for count in [64usize, 65, 129, 130] {
            for position in 0..count {
                for background in backgrounds {
                    for decider in deciders {
                        let mut bytes: Vec<u8> = (0..count)
                            .flat_map(|i| (if i == position { decider } else { background }).to_ne_bytes())
                            .collect();
                        let what = format!("count={count} position={position} {background} then {decider}");
                        reductions_agree(&what, &mut bytes, 0, count);
                    }
                }
            }
        }
    }

    /// Strictly improving and strictly worsening runs: the block scan's worst case and best case.
    #[test]
    fn extremes_agree_on_monotonic_runs() {
        for count in SCAN_LENGTHS {
            let mut rising: Vec<u8> = (0..count).flat_map(|i| (i as f32).to_ne_bytes()).collect();
            let mut falling: Vec<u8> = (0..count).flat_map(|i| (-(i as f32)).to_ne_bytes()).collect();
            reductions_agree(&format!("rising {count}"), &mut rising, 0, count);
            reductions_agree(&format!("falling {count}"), &mut falling, 0, count);
        }
    }

    #[test]
    fn eligibility_is_exactly_contiguous_f32() {
        let mut bytes = vec![0u8; 64];
        let base = span(&mut bytes, 0, 2);
        assert!(base.is_contiguous_f32());
        for kind in [Kind::U8, Kind::I8, Kind::U16, Kind::I16, Kind::U32, Kind::I32, Kind::F64] {
            assert!(!Span { kind, ..base }.is_contiguous_f32(), "{kind:?}");
            assert!(!Span { kind, stride: 4, ..base }.is_contiguous_f32(), "{kind:?} at the same stride");
        }
        for stride in [5, 8, 16] {
            assert!(!Span { stride, ..base }.is_contiguous_f32(), "stride {stride}");
        }
    }
}
