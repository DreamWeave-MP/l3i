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
//! A contiguous span, of any kind, runs its own compiled loops (`contiguous`), chosen once per
//! operation after the spans are checked; a strided one runs the generic loops here. The results
//! are the same, bit for bit, with one exception nothing ever promised: given two different NaN
//! operands, which payload survives an operation is the optimizer's operand order, here as in the
//! generic loop, which already answers differently in debug and release.
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
        // A plain kind is nearly every call, and this is on every call: no searching for `+` or `@`.
        let plain = match name.as_bytes() {
            b"f32" => Some(Kind::F32),
            b"f64" => Some(Kind::F64),
            b"u8" => Some(Kind::U8),
            b"i8" => Some(Kind::I8),
            b"u16" => Some(Kind::U16),
            b"i16" => Some(Kind::I16),
            b"u32" => Some(Kind::U32),
            b"i32" => Some(Kind::I32),
            _ => None,
        };
        if let Some(kind) = plain {
            return Ok(Layout::contiguous(kind));
        }
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

    /// Elements packed with no gap, which run the fastest loops in [`kind_loops`].
    #[inline(always)]
    fn is_contiguous(&self) -> bool {
        self.stride == self.kind.size()
    }

    /// This span, safe from writes to `out`: its elements copied with no gaps if `out` overlaps it,
    /// the copy kept alive by the first value returned. Only the elements are copied, so a wide
    /// stride costs nothing extra.
    fn kept_from(self, out: &Indices) -> (Option<Vec<u8>>, Span) {
        let span = byte_range(self.base, self.count, self.stride, self.kind.size());
        if !overlaps(span, byte_range(out.base, out.count, 4, 4)) {
            return (None, self);
        }
        let size = self.kind.size();
        // The stride is at least the size, so these bytes fit inside the checked span.
        let mut copy = Vec::with_capacity(self.count * size);
        for index in 0..self.count {
            // SAFETY: the span was bounds-checked at construction, and nothing else touches the
            // buffer during this copy.
            copy.extend_from_slice(unsafe { std::slice::from_raw_parts(self.base.add(index * self.stride), size) });
        }
        let base = copy.as_mut_ptr();
        (Some(copy), Span { base, stride: size, ..self })
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
    /// These indices, safe from writes through `destination`. They have been checked, and a loop
    /// reads them as it writes, so an output over them could otherwise turn a checked index into an
    /// unchecked one and aim a read or write outside its span. Then they are copied; the copy, kept
    /// alive by the first value returned, is what the loop reads.
    fn kept_from(self, destination: &Span) -> (Option<Vec<u8>>, Indices) {
        let written = byte_range(destination.base, destination.count, destination.stride, destination.kind.size());
        if !overlaps(byte_range(self.base, self.count, 4, 4), written) {
            return (None, self);
        }
        // SAFETY: `count` indices at `base` were bounds-checked at construction, and nothing else
        // touches the buffer during this copy.
        let mut copy = unsafe { std::slice::from_raw_parts(self.base, self.count * 4) }.to_vec();
        let base = copy.as_mut_ptr();
        (Some(copy), Indices { base, count: self.count })
    }

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

mod kind_loops;

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
    if span.is_contiguous() {
        kind_loops::extreme(*span, greatest)
    } else {
        kind_loops::extreme_strided(*span, greatest)
    }
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
    let selected = if span.is_contiguous() {
        kind_loops::count(span, comparison, threshold)
    } else {
        kind_loops::count_strided(span, comparison, threshold)
    };
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
    let span = Span::new("data.select", &buffer, Layout::parse("data.select", kind)?, offset, count)?;
    let comparison = Comparison::parse("data.select", comparison)?;
    let target = Target::prepare(into, span.count);
    select_words(&span, comparison, threshold, &mut target.selection().bits.borrow_mut());
    target.finish(call, 7)
}

/// Writes the comparison's result for each element into `words`, which hold one bit per element.
fn select_words(span: &Span, comparison: Comparison, threshold: f64, words: &mut [u64]) {
    if span.is_contiguous() {
        kind_loops::select(*span, comparison, threshold, words);
    } else {
        kind_loops::select_strided(*span, comparison, threshold, words);
    }
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
    let (_kept, indices) = indices.kept_from(&destination);
    // Sequential semantics: each element is read then written before the next, so an aliased
    // destination observes earlier writes, deterministically.
    if source.is_contiguous() {
        kind_loops::gather(source, &indices, destination);
    } else {
        kind_loops::gather_strided(source, &indices, destination);
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
    let (_kept, indices) = indices.kept_from(&destination);
    if destination.is_contiguous() {
        kind_loops::scatter(source, &indices, destination);
    } else {
        kind_loops::scatter_strided(source, &indices, destination);
    }
    Ok(source.count as f64)
}

fn fill(buffer: BufferView<'_>, kind: &str, offset: Exact<i64>, count: Exact<i64>, value: f64) -> Result<()> {
    let span = Span::new("data.fill", &buffer, Layout::parse("data.fill", kind)?, offset, count)?;
    if span.is_contiguous() {
        kind_loops::fill(span, value);
    } else {
        kind_loops::fill_strided(span, value);
    }
    Ok(())
}

#[inline(always)]
fn fill_elements(span: Span, value: f64) {
    for i in 0..span.count {
        span.set(i, value);
    }
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
    if out.is_contiguous() {
        kind_loops::add(left, right, out);
    } else {
        kind_loops::add_strided(left, right, out);
    }
    Ok(())
}

#[inline(always)]
fn add_elements(left: Span, right: Span, out: Span) {
    for i in 0..out.count {
        out.set(i, left.get(i) + right.get(i));
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
    if out.is_contiguous() {
        kind_loops::scale(source, factor, out);
    } else {
        kind_loops::scale_strided(source, factor, out);
    }
    Ok(())
}

#[inline(always)]
fn scale_elements(source: Span, factor: f64, out: Span) {
    for i in 0..out.count {
        out.set(i, source.get(i) * factor);
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
    if out.is_contiguous() {
        kind_loops::clamp(source, low, high, out);
    } else {
        kind_loops::clamp_strided(source, low, high, out);
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
            kind_loops::sort_keys(keys, &out);
        }
        Some(scratch) => {
            // Steady state, no allocation: the index vector moves between `out` and the caller's
            // scratch, reading keys from the span as it goes. The keys are read throughout, so
            // neither index vector may overlap them, and the two may not overlap each other.
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
            kind_loops::sort_with_scratch(keys, &out, &scratch);
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
    // The keys are read twice, to count and then to place, so an output over them could change the
    // answer between the two and run its slots past the end. Then they are copied, read once as
    // the operations table promises; the copy is kept alive by `_kept`.
    let (_kept, keys) = keys.kept_from(&out);
    let accepted = if keys.is_contiguous() {
        kind_loops::partition(keys, comparison, threshold, &out)
    } else {
        kind_loops::partition_strided(keys, comparison, threshold, &out)
    };
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

#[cfg(test)]
thread_local! {
    /// How many receiver calls reached a bound method on this thread, so that `lowering`'s tests
    /// can tell a hand-over from a native loop.
    pub(crate) static BOUND_RECEIVER_CALLS: Cell<usize> = const { Cell::new(0) };
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
                    #[cfg(test)]
                    BOUND_RECEIVER_CALLS.set(BOUND_RECEIVER_CALLS.get() + 1);
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
                    #[cfg(test)]
                    BOUND_RECEIVER_CALLS.set(BOUND_RECEIVER_CALLS.get() + 1);
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

/// The index of the first least (or greatest) element by a plain scan: the oracle for
/// `kind_loops`' block scan.
#[cfg(test)]
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

    /// The bound semantics: what the native loops are held to.
    #[inline]
    fn run(self, span: &Span, threshold: f64) -> Answer {
        if span.is_contiguous() { self.run_contiguous(*span, threshold) } else { self.run_strided(*span, threshold) }
    }

    /// The reduction as plain loops with the kind dispatched per element, element by element in
    /// order: the oracle `kind_loops` is tested against.
    #[cfg(test)]
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

/// The compiled forms of each loop held to each other, byte for byte. The integration tests compare
/// the public functions with handwritten Luau; these compare what LLVM made of the constant-kind
/// loops with the generic ones. They only test anything optimized: run them with `--release` too.
#[cfg(test)]
mod loop_paths {
    use super::{
        Answer, Comparison, Indices, Kind, Reduction, ReductionOp, Span, add_elements, clamp_elements, fill_elements,
        key_order, kind_loops, scale_elements,
    };
    use std::hint::black_box;

    /// A wide stride must not cost a copy of everything between its elements.
    #[test]
    fn a_kept_span_holds_only_its_elements() {
        const STRIDE: usize = 4096;
        let mut storage = vec![0u8; STRIDE * 2 + 4];
        storage[..4].copy_from_slice(&7u32.to_le_bytes());
        storage[STRIDE..STRIDE + 4].copy_from_slice(&8u32.to_le_bytes());
        storage[STRIDE * 2..].copy_from_slice(&9u32.to_le_bytes());
        let base = storage.as_mut_ptr();
        let keys = Span { base, kind: Kind::U32, stride: STRIDE, count: 3 };
        let (copy, kept) = keys.kept_from(&Indices { base, count: 3 });
        // Held for the reads below: the span points into it.
        let copy = copy.expect("the output overlaps");
        assert_eq!(copy.len(), 12);
        assert_eq!(kept.base, copy.as_ptr().cast_mut());
        assert_eq!(kept.stride, 4);
        assert_eq!((kept.get(0), kept.get(1), kept.get(2)), (7.0, 8.0, 9.0));
    }

    // The oracle: plain loops whose kind stays a run-time value, so `get` and `set` really dispatch
    // per element. `black_box` keeps LLVM from specializing them on what a test passes.
    fn add_any(left: Span, right: Span, out: Span) {
        add_elements(black_box(left), black_box(right), black_box(out));
    }
    fn scale_any(source: Span, factor: f64, out: Span) {
        scale_elements(black_box(source), factor, black_box(out));
    }
    fn clamp_any(source: Span, low: f64, high: f64, out: Span) {
        clamp_elements(black_box(source), low, high, black_box(out));
    }
    fn fill_any(span: Span, value: f64) {
        fill_elements(black_box(span), value);
    }
    fn extreme_any(span: &Span, greatest: bool) -> Option<usize> {
        super::extreme_elements(&black_box(*span), greatest)
    }
    fn count_any(span: Span, comparison: Comparison, threshold: f64) -> usize {
        let span = black_box(span);
        (0..span.count).filter(|&i| comparison.test(span.get(i), threshold)).count()
    }
    fn select_any(span: &Span, comparison: Comparison, threshold: f64, words: &mut [u64]) {
        let span = black_box(*span);
        for (word, chunk) in words.iter_mut().zip((0..span.count).step_by(64)) {
            let mut mask = 0u64;
            for bit in 0..64.min(span.count - chunk) {
                mask |= u64::from(comparison.test(span.get(chunk + bit), threshold)) << bit;
            }
            *word = mask;
        }
    }
    fn gather_any(source: Span, indices: &Indices, destination: Span) {
        let (source, destination) = (black_box(source), black_box(destination));
        for i in 0..indices.count {
            destination.set(i, source.get(indices.get(i)));
        }
    }
    fn scatter_any(source: Span, indices: &Indices, destination: Span) {
        let (source, destination) = (black_box(source), black_box(destination));
        for i in 0..source.count {
            destination.set(indices.get(i), source.get(i));
        }
    }
    fn partition_any(keys: Span, comparison: Comparison, threshold: f64, out: &Indices) -> usize {
        let keys = black_box(keys);
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
        accepted
    }
    /// The stable permutation by `key_order`, from a stable sort.
    fn sorted_any(keys: Span) -> Vec<u32> {
        let keys = black_box(keys);
        let mut order: Vec<(f64, u32)> = (0..keys.count).map(|i| (keys.get(i), i as u32)).collect();
        order.sort_by(|a, b| key_order(a.0, b.0));
        order.into_iter().map(|(_, index)| index).collect()
    }
    impl Reduction {
        fn run_any(self, span: &Span, threshold: f64) -> Answer {
            self.run_elements(&black_box(*span), threshold)
        }
    }

    /// The compiled forms of a loop. `Best` is what runs: AVX2 where the CPU has it, which is
    /// `Baseline` everywhere else.
    #[derive(Clone, Copy)]
    enum Route {
        Generic,
        Baseline,
        Best,
    }

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

    fn span_of(kind: Kind, bytes: &mut [u8], at: usize, count: usize) -> Span {
        assert!(at + count * kind.size() <= bytes.len());
        // SAFETY: inside the slice, checked above; the slice outlives the loop that uses the span
        // and no reference to it is used while the loop runs.
        Span { base: unsafe { bytes.as_mut_ptr().add(at) }, kind, stride: kind.size(), count }
    }

    fn span(bytes: &mut [u8], at: usize, count: usize) -> Span {
        span_of(Kind::F32, bytes, at, count)
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

    /// Runs `operation` on a copy of `base` through each route and compares every byte.
    fn agree(what: &str, base: &[u8], operation: impl Fn(&mut [u8], Route)) {
        let mut generic = base.to_vec();
        operation(&mut generic, Route::Generic);
        for (name, route) in [("baseline", Route::Baseline), ("best", Route::Best)] {
            let mut fast = base.to_vec();
            operation(&mut fast, route);
            if let Some(at) = first_difference(&fast, &generic) {
                let word = at & !3;
                let read = |bytes: &[u8]| u32::from_ne_bytes(bytes[word..word + 4].try_into().unwrap());
                panic!("{what}: byte {at}: {name} {:#010x}, generic {:#010x}", read(&fast), read(&generic));
            }
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
                    agree(&format!("add count={count} shift={shift} {place}"), &base, |bytes, route| {
                        let (l, r, o) = (span(bytes, left, count), span(bytes, right, count), span(bytes, out, count));
                        match route {
                            Route::Generic => add_any(l, r, o),
                            Route::Baseline => kind_loops::add_loop::<true>(l, r, o),
                            Route::Best => kind_loops::add(l, r, o),
                        }
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
                    agree(&format!("scale count={count} factor={factor} shift={shift}"), &base, |bytes, route| {
                        let (s, o) = (span(bytes, MIDDLE, count), span(bytes, out, count));
                        match route {
                            Route::Generic => scale_any(s, factor, o),
                            Route::Baseline => kind_loops::scale_loop::<true>(s, factor, o),
                            Route::Best => kind_loops::scale(s, factor, o),
                        }
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
            (-1e40, -1e39),
            (-1e-50, 1e-50),
            (1e-50, 1e-49),
            (-1e-49, -1e-50),
            (f64::from(f32::MAX), f64::INFINITY),
            // A large finite high bound is not an absent one: infinity and values above it clamp.
            (-1.0, f64::from(f32::MAX)),
            (-1.0, 1e35),
            (-1.0, 1e39),
            (0.0, f64::INFINITY),
            (f64::NEG_INFINITY, f64::from(f32::MIN)),
        ];
        for count in LENGTHS {
            let size = size(count) / 4 * 4;
            let base = words.corpus(size / 4);
            for (low, high) in bounds {
                for shift in SHIFTS {
                    let out = MIDDLE.wrapping_add_signed(shift);
                    agree(&format!("clamp count={count} [{low}, {high}] shift={shift}"), &base, |bytes, route| {
                        let (s, o) = (span(bytes, MIDDLE, count), span(bytes, out, count));
                        match route {
                            Route::Generic => clamp_any(s, low, high, o),
                            Route::Baseline => kind_loops::clamp_loop::<true>(s, low, high, o),
                            Route::Best => kind_loops::clamp(s, low, high, o),
                        }
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
                    agree(&format!("fill count={count} at={at} value={value}"), &base, |bytes, route| {
                        let s = span(bytes, at, count);
                        match route {
                            Route::Generic => fill_any(s, value),
                            Route::Baseline => kind_loops::fill_loop::<true>(s, value),
                            Route::Best => kind_loops::fill(s, value),
                        }
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

    /// Every reduction member, comparison and threshold through every route, on `bytes`.
    fn reductions_agree(what: &str, bytes: &mut [u8], at: usize, count: usize) {
        reductions_agree_for(Kind::F32, what, bytes, at, count);
    }

    fn reductions_agree_for(kind: Kind, what: &str, bytes: &mut [u8], at: usize, count: usize) {
        let s = span_of(kind, bytes, at, count);
        for greatest in [false, true] {
            let generic = extreme_any(&s, greatest);
            assert_eq!(kind_loops::extreme(s, greatest), generic, "extreme {greatest} {what}");
            assert_eq!(kind_loops::extreme_loop::<true>(s, greatest), generic, "baseline extreme {greatest} {what}");
        }
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
            let (fast, plain) = (reduction.run_contiguous(s, 0.0), reduction.run_any(&s, 0.0));
            reductions_match(kind, op, &fast, &plain, &format!("{op:?} {what}"));
        }
        for comparison in Comparison::ALL {
            for threshold in THRESHOLDS {
                let label = format!("{comparison:?} {threshold} {what}");
                for op in ops {
                    let reduction = Reduction::new(op, Some(comparison));
                    let (fast, generic) = (reduction.run_contiguous(s, threshold), reduction.run_any(&s, threshold));
                    reductions_match(kind, op, &fast, &generic, &format!("{op:?} {label}"));
                }
                let selected = count_any(s, comparison, threshold);
                assert_eq!(kind_loops::count(s, comparison, threshold), selected, "count {label}");
                assert_eq!(
                    kind_loops::count_loop::<true>(s, comparison, threshold),
                    selected,
                    "baseline count {label}"
                );
                let any = Reduction::new(ReductionOp::Any, Some(comparison)).run_any(&s, threshold);
                let all = Reduction::new(ReductionOp::All, Some(comparison)).run_any(&s, threshold);
                let (found, missed) = (
                    kind_loops::find_loop::<true>(s, comparison, threshold, true),
                    kind_loops::find_loop::<true>(s, comparison, threshold, false),
                );
                assert_eq!((2, u64::from(found)), answer(&any), "baseline any {label}");
                assert_eq!((2, u64::from(!missed)), answer(&all), "baseline all {label}");
                let mut generic = vec![0; count.div_ceil(64)];
                select_any(&s, comparison, threshold, &mut generic);
                let mut fast = vec![u64::MAX; count.div_ceil(64)];
                kind_loops::select(s, comparison, threshold, &mut fast);
                assert_eq!(fast, generic, "select {label}");
                fast.fill(u64::MAX);
                kind_loops::select_loop::<true>(s, comparison, threshold, &mut fast);
                assert_eq!(fast, generic, "baseline select {label}");
            }
        }
    }

    /// The narrowed threshold against the comparison it replaces, around every `f32` and `f64`
    /// boundary that could tell them apart: the elements themselves, their neighbours in both
    /// widths, the midpoints between adjacent `f32` values, and everything past the `f32` range.
    #[test]
    fn narrowed_thresholds_answer_as_the_wide_comparison() {
        let mut elements: Vec<f32> = PATTERNS.iter().map(|bits| f32::from_bits(*bits)).collect();
        let mut words = Words(0x7e57_ab1e);
        elements.extend((0..200).map(|_| f32::from_bits(words.next())));
        elements.extend(elements.clone().iter().flat_map(|value| [value.next_up(), value.next_down()]));
        let mut thresholds = vec![
            f64::INFINITY,
            f64::NEG_INFINITY,
            f64::NAN,
            -f64::NAN,
            f64::MAX,
            f64::MIN,
            f64::MIN_POSITIVE,
            -f64::MIN_POSITIVE,
            1e-320,
            -1e-320,
            1e-50,
            -1e-50,
            1e39,
            -1e39,
        ];
        for value in &elements {
            let wide = f64::from(*value);
            let midpoint = f64::midpoint(wide, f64::from(value.next_up()));
            thresholds.extend([
                wide,
                wide.next_up(),
                wide.next_down(),
                midpoint,
                midpoint.next_up(),
                midpoint.next_down(),
            ]);
        }
        for comparison in Comparison::ALL {
            for threshold in &thresholds {
                let narrow = comparison.narrow(*threshold);
                for value in &elements {
                    assert_eq!(
                        comparison.test_f32(*value, narrow),
                        comparison.test(f64::from(*value), *threshold),
                        "{value:e} {comparison:?} {threshold:e} narrowed to {narrow:e}",
                    );
                }
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
            // Both ends of each block, where a scan that miscounts would miss it.
            for position in (0..count).filter(|position| !(4..60).contains(&(position % 64))) {
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

    /// Every kind other than `f32`. Integer writes wrap and truncate, so the corpus of raw words
    /// and these factors and bounds reach every conversion rule: NaN writes zero, values past 2^63
    /// saturate before the wrap, fractions truncate toward zero.
    const OTHER_KINDS: [Kind; 7] = [Kind::U8, Kind::I8, Kind::U16, Kind::I16, Kind::U32, Kind::I32, Kind::F64];
    const OTHER_LENGTHS: [usize; 9] = [0, 1, 3, 8, 17, 33, 64, 65, 257];

    /// As [`finite`], for `f64` words.
    fn finite_f64(bytes: &mut [u8], at: usize, count: usize) {
        for word in bytes[at..at + count * 8].as_chunks_mut::<8>().0 {
            let bits = u64::from_ne_bytes(*word);
            if bits & 0x7ff0_0000_0000_0000 == 0x7ff0_0000_0000_0000 {
                *word = (bits & 0xbfff_ffff_ffff_ffff).to_ne_bytes();
            }
        }
    }

    /// The word count of a corpus holding `bytes` bytes, rounded up.
    fn corpus_of(words: &mut Words, bytes: usize) -> Vec<u8> {
        let mut base = words.corpus(bytes.div_ceil(4));
        base.truncate(bytes);
        base
    }

    #[test]
    fn other_kinds_agree_at_every_placement() {
        let mut words = Words(0x51ed_270b);
        for kind in OTHER_KINDS {
            let width = kind.size();
            for count in OTHER_LENGTHS {
                // Past anything the output can reach, the other operand when only one overlaps.
                let total = MIDDLE * 2 + (2 * count + 40) * width;
                let mut base = corpus_of(&mut words, total);
                let far = total - (count + 1) * width;
                if kind == Kind::F64 {
                    finite_f64(&mut base, far, count);
                }
                let what = format!("{kind:?} count={count}");
                for shift in SHIFTS {
                    let out = MIDDLE.wrapping_add_signed(shift);
                    for (left, right) in [(MIDDLE, far), (far, MIDDLE), (MIDDLE, MIDDLE)] {
                        agree(&format!("add {what} shift={shift} {left} {right}"), &base, |bytes, route| {
                            let (l, r, o) = (
                                span_of(kind, bytes, left, count),
                                span_of(kind, bytes, right, count),
                                span_of(kind, bytes, out, count),
                            );
                            match route {
                                Route::Generic => add_any(l, r, o),
                                Route::Baseline => kind_loops::add_loop::<true>(l, r, o),
                                Route::Best => kind_loops::add(l, r, o),
                            }
                        });
                    }
                    for factor in [1.0 / 3.0, -1.9, 3.0, 0.0, 1e300, -1e300, f64::INFINITY] {
                        agree(&format!("scale {what} shift={shift} factor={factor}"), &base, |bytes, route| {
                            let (s, o) = (span_of(kind, bytes, MIDDLE, count), span_of(kind, bytes, out, count));
                            match route {
                                Route::Generic => scale_any(s, factor, o),
                                Route::Baseline => kind_loops::scale_loop::<true>(s, factor, o),
                                Route::Best => kind_loops::scale(s, factor, o),
                            }
                        });
                    }
                    for (low, high) in [(-5.0, 5.0), (0.5, 0.5), (-1.9, 1.9), (0.0, f64::INFINITY), (1e300, 1e301)] {
                        agree(&format!("clamp {what} shift={shift} [{low}, {high}]"), &base, |bytes, route| {
                            let (s, o) = (span_of(kind, bytes, MIDDLE, count), span_of(kind, bytes, out, count));
                            match route {
                                Route::Generic => clamp_any(s, low, high, o),
                                Route::Baseline => kind_loops::clamp_loop::<true>(s, low, high, o),
                                Route::Best => kind_loops::clamp(s, low, high, o),
                            }
                        });
                    }
                }
                for value in [1.5, -1.9, 300.0, -300.0, 70000.5, 5e9, 1e300, f64::NAN, f64::INFINITY, -0.0] {
                    agree(&format!("fill {what} value={value}"), &base, |bytes, route| {
                        let s = span_of(kind, bytes, 3, count);
                        match route {
                            Route::Generic => fill_any(s, value),
                            Route::Baseline => kind_loops::fill_loop::<true>(s, value),
                            Route::Best => kind_loops::fill(s, value),
                        }
                    });
                }
            }
        }
    }

    #[test]
    fn other_kinds_reductions_agree_on_the_corpus() {
        let mut words = Words(0x2f6e_2b1d);
        for kind in OTHER_KINDS {
            for count in OTHER_LENGTHS.into_iter().chain([63, 127, 128, 129, 193]) {
                for at in [0, 1] {
                    let mut bytes = corpus_of(&mut words, at + count * kind.size());
                    reductions_agree_for(kind, &format!("{kind:?} count={count} at={at}"), &mut bytes, at, count);
                }
            }
        }
    }

    fn strided_of(kind: Kind, bytes: &mut [u8], at: usize, count: usize, stride: usize) -> Span {
        if count > 0 {
            assert!(at + (count - 1) * stride + kind.size() <= bytes.len());
        }
        // SAFETY: as `span_of`, with the checked extent above.
        Span { base: unsafe { bytes.as_mut_ptr().add(at) }, kind, stride, count }
    }

    /// Strided layouts of every kind, each operation against the plain loop, including outputs that
    /// overlap their source either way. Fields overlap their neighbours at a stride below 2 sizes.
    #[test]
    fn strided_layouts_agree_with_the_plain_loops() {
        let mut words = Words(0x6c07_8965);
        for kind in super::KINDS {
            let size = kind.size();
            // Off the common record sizes, and on the ones `f32` reads have constant loops for.
            for stride in [size + 1, size * 2 + 3, 8.max(size + 1), 12, 16] {
                for count in [0usize, 1, 3, 8, 33, 65, 129] {
                    let total = 64 + (2 * count + 8) * stride;
                    let mut base = corpus_of(&mut words, total);
                    let far = total - (count + 1) * stride;
                    if kind == Kind::F32 {
                        for i in 0..count {
                            finite(&mut base, far + i * stride, 1);
                        }
                    } else if kind == Kind::F64 {
                        for i in 0..count {
                            finite_f64(&mut base, far + i * stride, 1);
                        }
                    }
                    let what = format!("{kind:?}@{stride} count={count}");
                    for shift in [-16isize, -1, 0, 1, stride as isize, 16] {
                        let out = 24usize.wrapping_add_signed(shift);
                        let span = |bytes: &mut [u8], at| strided_of(kind, bytes, at, count, stride);
                        agree(&format!("add {what} shift={shift}"), &base, |bytes, route| {
                            let (l, r, o) = (span(bytes, 24), span(bytes, far), span(bytes, out));
                            if matches!(route, Route::Generic) {
                                add_any(l, r, o);
                            } else {
                                kind_loops::add_strided(l, r, o);
                            }
                        });
                        for factor in [1.0 / 3.0, -1.9, 3.0, 1e300] {
                            agree(&format!("scale {what} shift={shift} {factor}"), &base, |bytes, route| {
                                let (s, o) = (span(bytes, 24), span(bytes, out));
                                if matches!(route, Route::Generic) {
                                    scale_any(s, factor, o);
                                } else {
                                    kind_loops::scale_strided(s, factor, o);
                                }
                            });
                        }
                        for (low, high) in [(-5.0, 5.0), (0.5, 0.5), (0.0, f64::INFINITY), (1e39, 1e40)] {
                            agree(&format!("clamp {what} shift={shift} [{low}, {high}]"), &base, |bytes, route| {
                                let (s, o) = (span(bytes, 24), span(bytes, out));
                                if matches!(route, Route::Generic) {
                                    clamp_any(s, low, high, o);
                                } else {
                                    kind_loops::clamp_strided(s, low, high, o);
                                }
                            });
                        }
                    }
                    for value in [1.5, -1.9, 300.0, 1e300, f64::NAN] {
                        agree(&format!("fill {what} {value}"), &base, |bytes, route| {
                            let s = strided_of(kind, bytes, 3, count, stride);
                            if matches!(route, Route::Generic) {
                                fill_any(s, value);
                            } else {
                                kind_loops::fill_strided(s, value);
                            }
                        });
                    }
                    let s = strided_of(kind, &mut base, 5, count, stride);
                    for greatest in [false, true] {
                        assert_eq!(
                            kind_loops::extreme_strided(s, greatest),
                            extreme_any(&s, greatest),
                            "extreme {what}"
                        );
                    }
                    for op in [ReductionOp::Sum, ReductionOp::Min, ReductionOp::Max] {
                        let reduction = Reduction::new(op, None);
                        let (fast, plain) = (reduction.run_strided(s, 0.0), reduction.run_any(&s, 0.0));
                        reductions_match(kind, op, &fast, &plain, &format!("{op:?} {what}"));
                    }
                    for comparison in Comparison::ALL {
                        for threshold in THRESHOLDS {
                            let label = format!("{comparison:?} {threshold} {what}");
                            for op in [
                                ReductionOp::Sum,
                                ReductionOp::Min,
                                ReductionOp::Max,
                                ReductionOp::Count,
                                ReductionOp::Any,
                                ReductionOp::All,
                            ] {
                                let reduction = Reduction::new(op, Some(comparison));
                                let (fast, plain) =
                                    (reduction.run_strided(s, threshold), reduction.run_any(&s, threshold));
                                reductions_match(kind, op, &fast, &plain, &format!("{op:?} {label}"));
                            }
                            let mut fast = vec![u64::MAX; count.div_ceil(64)];
                            let mut plain = vec![0; count.div_ceil(64)];
                            kind_loops::select_strided(s, comparison, threshold, &mut fast);
                            select_any(&s, comparison, threshold, &mut plain);
                            assert_eq!(fast, plain, "select {label}");
                        }
                    }
                }
            }
        }
    }

    /// Answers must be equal bit for bit, except a sum of NaNs: which NaN survives is unspecified.
    fn reductions_match(kind: Kind, op: ReductionOp, fast: &Answer, plain: &Answer, what: &str) {
        if let (ReductionOp::Sum, Answer::Number(a), Answer::Number(b)) = (op, fast, plain)
            && a.is_nan()
            && b.is_nan()
        {
            return;
        }
        assert_eq!(answer(fast), answer(plain), "{kind:?} {what}");
    }

    /// Filtered sums, minima and maxima on contiguous spans, where the block tests are.
    #[test]
    fn filtered_reductions_agree_with_a_sparse_or_dense_filter() {
        let mut words = Words(0x3c6e_f372);
        for kind in super::KINDS {
            for count in [0usize, 1, 7, 8, 9, 63, 64, 65, 129, 300] {
                let mut bytes = corpus_of(&mut words, count * kind.size());
                let s = span_of(kind, &mut bytes, 0, count);
                for comparison in Comparison::ALL {
                    // Thresholds at each kind's extremes make a filter accept almost nothing or almost all.
                    for threshold in [0.0, 1.0, -1.0, 100.0, 250.0, 1e9, -1e9, 1e30, -1e30, f64::NAN] {
                        for op in [ReductionOp::Sum, ReductionOp::Min, ReductionOp::Max] {
                            let reduction = Reduction::new(op, Some(comparison));
                            let (fast, plain) =
                                (reduction.run_contiguous(s, threshold), reduction.run_any(&s, threshold));
                            reductions_match(
                                kind,
                                op,
                                &fast,
                                &plain,
                                &format!("{op:?} {comparison:?} {threshold} {count}"),
                            );
                        }
                    }
                }
            }
        }
    }

    fn index_bytes(values: impl Iterator<Item = u32>) -> Vec<u8> {
        values.flat_map(u32::to_ne_bytes).collect()
    }

    fn indices_over(bytes: &mut [u8], at: usize, count: usize) -> Indices {
        assert!(at + count * 4 <= bytes.len());
        // SAFETY: inside the slice, checked above.
        Indices { base: unsafe { bytes.as_mut_ptr().add(at) }, count }
    }

    #[test]
    fn gather_and_scatter_agree_including_overlaps_and_signaling_nans() {
        let mut words = Words(0x1f83_d9ab);
        for kind in super::KINDS {
            let size = kind.size();
            for stride in [size, size + 1, size * 2 + 3] {
                for count in [0usize, 1, 3, 8, 17, 64, 65] {
                    let total = 128 + (2 * count + 8) * stride;
                    let base = corpus_of(&mut words, total);
                    let mut indices = index_bytes((0..count).map(|_| words.next() % count.max(1) as u32));
                    let what = format!("{kind:?}@{stride} count={count}");
                    for shift in [-8isize, -1, 0, 1, 8, 64] {
                        let destination = 24usize.wrapping_add_signed(shift);
                        agree(&format!("gather {what} shift={shift}"), &base, |bytes, route| {
                            let source = strided_of(kind, bytes, 24, count, stride);
                            let out = span_of(kind, bytes, destination, count);
                            let mut copy = indices.clone();
                            let order = indices_over(&mut copy, 0, count);
                            match route {
                                Route::Generic => gather_any(source, &order, out),
                                _ if stride == size => kind_loops::gather(source, &order, out),
                                _ => kind_loops::gather_strided(source, &order, out),
                            }
                        });
                        agree(&format!("scatter {what} shift={shift}"), &base, |bytes, route| {
                            let source = span_of(kind, bytes, 24, count);
                            let out = strided_of(kind, bytes, destination, count, stride);
                            let mut copy = indices.clone();
                            let order = indices_over(&mut copy, 0, count);
                            match route {
                                Route::Generic => scatter_any(source, &order, out),
                                _ if stride == size => kind_loops::scatter(source, &order, out),
                                _ => kind_loops::scatter_strided(source, &order, out),
                            }
                        });
                    }
                    indices.clear();
                }
            }
        }
    }

    #[test]
    fn partition_agrees_including_output_over_the_keys() {
        let mut words = Words(0x5be0_cd19);
        for kind in super::KINDS {
            let size = kind.size();
            for stride in [size, size + 1, size * 2 + 3] {
                for count in [0usize, 1, 3, 8, 17, 64, 65, 129] {
                    let total = 64 + (count + 8) * stride.max(4) + count * 4;
                    let base = corpus_of(&mut words, total);
                    for comparison in Comparison::ALL {
                        for threshold in [0.0, 1.0, -1.0, 100.0, f64::NAN] {
                            // Past the keys: `partition` copies any keys its output overlaps, and a
                            // test through Luau holds that case to the answer on separate buffers.
                            {
                                let out_at = total - count * 4;
                                let what = format!("{kind:?}@{stride} {count} {comparison:?} {threshold}");
                                agree(&what, &base, |bytes, route| {
                                    let keys = strided_of(kind, bytes, 16, count, stride);
                                    let out = indices_over(bytes, out_at, count);
                                    let accepted = match route {
                                        Route::Generic => partition_any(keys, comparison, threshold, &out),
                                        _ if stride == size => kind_loops::partition(keys, comparison, threshold, &out),
                                        _ => kind_loops::partition_strided(keys, comparison, threshold, &out),
                                    };
                                    let accepted = accepted as u32;
                                    bytes[..4].copy_from_slice(&accepted.to_ne_bytes());
                                });
                            }
                        }
                    }
                }
            }
        }
    }

    /// Both zeros tie, every NaN ties and comes last, and ties keep their order, on every path:
    /// the allocating sort, the merge below the radix length, and the radix sort from it.
    #[test]
    fn argsort_agrees_with_a_stable_sort() {
        let mut words = Words(0x9b05_688c);
        for kind in super::KINDS {
            let size = kind.size();
            for stride in [size, size + 3] {
                for count in [0usize, 1, 2, 3, 7, 64, 511, 512, 513, 2000] {
                    // Raw words for every bit pattern, then a small alphabet for long runs of ties.
                    for mut bytes in [
                        corpus_of(&mut words, 8 + count * stride + size),
                        (0..8 + count * stride + size).map(|_| (words.next() >> 28) as u8).collect::<Vec<u8>>(),
                    ] {
                        let keys = strided_of(kind, &mut bytes, 8, count, stride);
                        let expected = sorted_any(keys);
                        let mut out = vec![0u8; count * 4];
                        let mut scratch = vec![0u8; count * 4];
                        let (order, spare) = (indices_over(&mut out, 0, count), indices_over(&mut scratch, 0, count));
                        kind_loops::sort_keys(keys, &order);
                        let got: Vec<u32> = (0..count).map(|i| order.get(i) as u32).collect();
                        assert_eq!(got, expected, "sort_keys {kind:?}@{stride} count={count}");
                        kind_loops::sort_with_scratch(keys, &order, &spare);
                        let got: Vec<u32> = (0..count).map(|i| order.get(i) as u32).collect();
                        assert_eq!(got, expected, "sort_with_scratch {kind:?}@{stride} count={count}");
                    }
                }
            }
        }
    }

    #[test]
    fn eligibility_is_exactly_stride_equal_to_size() {
        let mut bytes = vec![0u8; 64];
        for kind in super::KINDS {
            let base = span_of(kind, &mut bytes, 0, 2);
            assert!(base.is_contiguous(), "{kind:?}");
            for extra in [1, 3, 8] {
                let stride = kind.size() + extra;
                assert!(!Span { stride, ..base }.is_contiguous(), "{kind:?} at stride {stride}");
            }
        }
    }
}
