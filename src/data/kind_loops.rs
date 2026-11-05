//! The element loops, with the element kind constant.
//!
//! Each operation is chosen once, after its spans are checked. There are no intrinsics: `Span`'s
//! `get` and `set` dispatch on the kind per element, so these instantiate each loop once per kind,
//! which folds the dispatch away, and LLVM vectorizes what it can. `C` says whether the stride is
//! constant too. Contiguous loops (`C = true`) are compiled twice, for the baseline target and for
//! AVX2 behind a run-time check, so every x86-64 and AArch64 target keeps working unchanged.
//! Strided loops (`C = false`) stay scalar, compiled once, and lose only the dispatch, except that
//! `f32` reads at the common record sizes have the stride constant as well.
//!
//! The results are the plain loops', bit for bit, which the tests in `mod.rs` hold them to, by
//! these arguments:
//!
//! - Raw pointers and no references, so an overlap is defined and LLVM has to keep the sequential
//!   result, which it does with its own run-time overlap check and a scalar fallback.
//! - An `f32` element compared with an `f64` threshold has an exact `f32` equivalent, computed
//!   once ([`Comparison::narrow`]), which doubles the lanes against widening every element.
//! - A block test only decides whether a block needs looking at. A block that does runs the
//!   original rule in order, so first occurrences, ties and NaNs are unchanged.
//! - A write of each element in the same order as a read-then-write of each element.

use super::{
    Answer, Comparison, Indices, Kind, Reduction, ReductionOp, Span, add_elements, clamp_elements, fill_elements,
    key_order, scale_elements,
};

/// Elements a block-wise scan tests before it looks at the answer. Long enough that the test is
/// nearly all vector code; short enough that a hit costs little.
const SCAN_BLOCK: usize = 64;
/// The first block of a scan that can stop early, doubling up to [`SCAN_BLOCK`], so that a span
/// decided by its first few elements pays for a few elements.
const FIRST_SCAN_BLOCK: usize = 8;
/// Elements counted into 32-bit lanes before they move to the running total. A `u32` count packs
/// twice the lanes of a `usize` one and cannot overflow in a block this size.
const COUNT_BLOCK: usize = 1 << 20;
/// Bit 22, which makes an `f32` NaN quiet.
const QUIET: u32 = 1 << 22;
/// From this many keys a radix sort beats a merge, histograms and all.
const RADIX_FROM: usize = 512;

/// `pub(super) fn $name`, which runs `$body` compiled for AVX2 on a CPU that has it and for the
/// baseline otherwise. `$body` must be `#[inline(always)]`: inlining is how it gets compiled twice.
macro_rules! with_best_vectors {
    ($(fn $name:ident($($argument:ident: $type:ty),*) $(-> $result:ty)? = $body:path;)*) => {$(
        #[inline(never)]
        pub(super) fn $name($($argument: $type),*) $(-> $result)? {
            #[cfg(target_arch = "x86_64")]
            {
                #[target_feature(enable = "avx2")]
                fn avx2($($argument: $type),*) $(-> $result)? {
                    $body($($argument),*)
                }
                if std::arch::is_x86_feature_detected!("avx2") {
                    // SAFETY: the CPU was just found to have the one feature `avx2` requires.
                    return unsafe { avx2($($argument),*) };
                }
            }
            $body($($argument),*)
        }
    )*};
}

/// `pub(super) fn $name`, the loop `$body` compiled once.
macro_rules! once {
    ($(fn $name:ident($($argument:ident: $type:ty),*) $(-> $result:ty)? = $body:path;)*) => {$(
        #[inline(never)]
        pub(super) fn $name($($argument: $type),*) $(-> $result)? {
            $body($($argument),*)
        }
    )*};
}

with_best_vectors! {
    fn add(left: Span, right: Span, out: Span) = add_loop::<true>;
    fn scale(source: Span, factor: f64, out: Span) = scale_loop::<true>;
    fn clamp(source: Span, low: f64, high: f64, out: Span) = clamp_loop::<true>;
    fn fill(span: Span, value: f64) = fill_loop::<true>;
    fn extreme(span: Span, greatest: bool) -> Option<usize> = extreme_loop::<true>;
    fn extreme_where(span: Span, greatest: bool, comparison: Comparison, threshold: f64) -> Option<usize> = extreme_where_loop::<true>;
    fn count(span: Span, comparison: Comparison, threshold: f64) -> usize = count_loop::<true>;
    fn find(span: Span, comparison: Comparison, threshold: f64, wanted: bool) -> bool = find_loop::<true>;
    fn select(span: Span, comparison: Comparison, threshold: f64, words: &mut [u64]) = select_loop::<true>;
    fn sum_where(span: Span, comparison: Comparison, threshold: f64) -> f64 = sum_where_loop::<true>;
    fn gather(source: Span, indices: &Indices, destination: Span) = gather_loop::<true>;
    fn scatter(source: Span, indices: &Indices, destination: Span) = scatter_loop::<true>;
    fn partition(keys: Span, comparison: Comparison, threshold: f64, out: &Indices) -> usize = partition_loop::<true>;
}

once! {
    fn add_strided(left: Span, right: Span, out: Span) = add_loop::<false>;
    fn scale_strided(source: Span, factor: f64, out: Span) = scale_loop::<false>;
    fn clamp_strided(source: Span, low: f64, high: f64, out: Span) = clamp_loop::<false>;
    fn fill_strided(span: Span, value: f64) = fill_loop::<false>;
    fn extreme_any_stride(span: Span, greatest: bool) -> Option<usize> = extreme_loop::<false>;
    fn extreme_where_strided(span: Span, greatest: bool, comparison: Comparison, threshold: f64) -> Option<usize> = extreme_where_loop::<false>;
    fn count_any_stride(span: Span, comparison: Comparison, threshold: f64) -> usize = count_loop::<false>;
    fn find_any_stride(span: Span, comparison: Comparison, threshold: f64, wanted: bool) -> bool = find_loop::<false>;
    fn select_any_stride(span: Span, comparison: Comparison, threshold: f64, words: &mut [u64]) = select_loop::<false>;
    fn sum_where_strided(span: Span, comparison: Comparison, threshold: f64) -> f64 = sum_where_loop::<false>;
    fn gather_strided(source: Span, indices: &Indices, destination: Span) = gather_loop::<false>;
    fn scatter_strided(source: Span, indices: &Indices, destination: Span) = scatter_loop::<false>;
    fn partition_strided(keys: Span, comparison: Comparison, threshold: f64, out: &Indices) -> usize = partition_loop::<false>;
    fn sum(span: Span) -> f64 = sum_loop::<true>;
    fn sum_strided(span: Span) -> f64 = sum_loop::<false>;
    fn sort_keys(keys: Span, out: &Indices) = sort_keys_loop;
    fn sort_with_scratch(keys: Span, out: &Indices, scratch: &Indices) = sort_with_scratch_loop;
}

// A constant stride lets AVX2 read a field of packed records in vectors, so the read-only loops are
// compiled for `f32` at the record sizes that come up. Writes are left out on purpose: AVX2 has no
// scatter store, so a constant stride makes them slower than the plain loop.
with_best_vectors! {
    fn extreme_f32_8(span: Span, greatest: bool) -> Option<usize> = extreme_f32_at::<8>;
    fn extreme_f32_12(span: Span, greatest: bool) -> Option<usize> = extreme_f32_at::<12>;
    fn extreme_f32_16(span: Span, greatest: bool) -> Option<usize> = extreme_f32_at::<16>;
    fn count_f32_8(span: Span, comparison: Comparison, threshold: f64) -> usize = count_f32_at::<8>;
    fn count_f32_12(span: Span, comparison: Comparison, threshold: f64) -> usize = count_f32_at::<12>;
    fn count_f32_16(span: Span, comparison: Comparison, threshold: f64) -> usize = count_f32_at::<16>;
    fn find_f32_8(span: Span, comparison: Comparison, threshold: f64, wanted: bool) -> bool = find_f32_at::<8>;
    fn find_f32_12(span: Span, comparison: Comparison, threshold: f64, wanted: bool) -> bool = find_f32_at::<12>;
    fn find_f32_16(span: Span, comparison: Comparison, threshold: f64, wanted: bool) -> bool = find_f32_at::<16>;
    fn select_f32_8(span: Span, comparison: Comparison, threshold: f64, words: &mut [u64]) = select_f32_at::<8>;
    fn select_f32_12(span: Span, comparison: Comparison, threshold: f64, words: &mut [u64]) = select_f32_at::<12>;
    fn select_f32_16(span: Span, comparison: Comparison, threshold: f64, words: &mut [u64]) = select_f32_at::<16>;
}

/// `pub(super) fn $name`, which takes the loop for a known `f32` stride, else the run-time one.
macro_rules! by_stride {
    ($(fn $name:ident($span:ident $(, $argument:ident: $type:ty)*) $(-> $result:ty)? = $at_8:ident, $at_12:ident, $at_16:ident, $other:ident;)*) => {$(
        pub(super) fn $name($span: Span $(, $argument: $type)*) $(-> $result)? {
            match ($span.kind, $span.stride) {
                (Kind::F32, 8) => $at_8($span $(, $argument)*),
                (Kind::F32, 12) => $at_12($span $(, $argument)*),
                (Kind::F32, 16) => $at_16($span $(, $argument)*),
                _ => $other($span $(, $argument)*),
            }
        }
    )*};
}

by_stride! {
    fn extreme_strided(span, greatest: bool) -> Option<usize> = extreme_f32_8, extreme_f32_12, extreme_f32_16, extreme_any_stride;
    fn count_strided(span, comparison: Comparison, threshold: f64) -> usize = count_f32_8, count_f32_12, count_f32_16, count_any_stride;
    fn find_strided(span, comparison: Comparison, threshold: f64, wanted: bool) -> bool = find_f32_8, find_f32_12, find_f32_16, find_any_stride;
    fn select_strided(span, comparison: Comparison, threshold: f64, words: &mut [u64]) = select_f32_8, select_f32_12, select_f32_16, select_any_stride;
}

/// `$body` once per kind, with `$kind` rebound to a constant, so a loop in the body has no dispatch
/// left to do and `$kind == Kind::F32` in the body picks a branch at compile time.
macro_rules! constant_kind {
    ($kind:ident => $body:expr) => {
        match $kind {
            Kind::U8 => {
                let $kind = Kind::U8;
                $body
            }
            Kind::I8 => {
                let $kind = Kind::I8;
                $body
            }
            Kind::U16 => {
                let $kind = Kind::U16;
                $body
            }
            Kind::I16 => {
                let $kind = Kind::I16;
                $body
            }
            Kind::U32 => {
                let $kind = Kind::U32;
                $body
            }
            Kind::I32 => {
                let $kind = Kind::I32;
                $body
            }
            Kind::F32 => {
                let $kind = Kind::F32;
                $body
            }
            Kind::F64 => {
                let $kind = Kind::F64;
                $body
            }
        }
    };
}

/// `$run(count, test, ...)` where `test(i)` says whether element `i` passes the comparison, with
/// the kind constant. Contiguous `f32` compares narrowed and contiguous `f64` as itself, one loop
/// per comparison, where the extra lanes pay for the code. Contiguous integers have one loop per
/// kind, answering all six comparisons from a table. Strided spans do not vectorize.
macro_rules! with_element_test {
    ($span:ident, $comparison:ident, $threshold:ident => $run:ident($($extra:expr),*)) => {{
        let kind = $span.kind;
        constant_kind!(kind => {
            let $span = $span.with_kind::<C>(kind);
            if C && kind == Kind::F32 {
                constant_comparison!($comparison => {
                    let narrowed = $comparison.narrow($threshold);
                    $run($span.count, |i| $comparison.test_f32($span.get_f32(i), narrowed) $(, $extra)*)
                })
            } else if C && kind == Kind::F64 {
                constant_comparison!($comparison => {
                    $run($span.count, |i| $comparison.test($span.get(i), $threshold) $(, $extra)*)
                })
            } else if C {
                let table = OrderTable::of($comparison);
                $run($span.count, |i| table.test($span.get(i), $threshold) $(, $extra)*)
            } else {
                // No vectors to pay for three compares an element: one, picked by a branch that
                // always goes the same way.
                $run($span.count, |i| $comparison.test($span.get(i), $threshold) $(, $extra)*)
            }
        })
    }};
}

/// A comparison as what it answers for an element below, equal to and above the threshold, and
/// when either is NaN and so none of those: only `Ne` is true then.
#[derive(Clone, Copy)]
struct OrderTable {
    below: bool,
    equal: bool,
    above: bool,
    unordered: bool,
}

impl OrderTable {
    fn of(comparison: Comparison) -> OrderTable {
        let (below, equal, above) = match comparison {
            Comparison::Eq => (false, true, false),
            Comparison::Ne => (true, false, true),
            Comparison::Lt => (true, false, false),
            Comparison::Le => (true, true, false),
            Comparison::Gt => (false, false, true),
            Comparison::Ge => (false, true, true),
        };
        OrderTable { below, equal, above, unordered: comparison == Comparison::Ne }
    }

    /// Branchless, so it vectorizes: no element picks a branch, each lane gets the same compares
    /// and a mask. That is why these are not the lazy operators.
    #[inline(always)]
    #[allow(clippy::needless_bitwise_bool)]
    fn test(self, value: f64, threshold: f64) -> bool {
        (self.below & (value < threshold))
            | (self.equal & (value == threshold))
            | (self.above & (value > threshold))
            | (self.unordered & (value.is_nan() | threshold.is_nan()))
    }
}

impl Kind {
    #[inline(always)]
    fn is_float(self) -> bool {
        matches!(self, Kind::F32 | Kind::F64)
    }
}

impl Span {
    /// The same span with this kind as a constant, and with its stride as one when `C` says the
    /// span is contiguous. Inlined with a constant, a loop over `get` and `set` loses the
    /// per-element dispatch in `Kind::read` and `Kind::write`.
    #[inline(always)]
    fn with_kind<const C: bool>(self, kind: Kind) -> Span {
        debug_assert!(self.kind == kind && (!C || self.is_contiguous()));
        Span { kind, stride: if C { kind.size() } else { self.stride }, ..self }
    }

    /// Element `index` of an `f32` span as stored. Comparing these needs no widening; arithmetic
    /// on them would skip the quieting a widening read does, so only comparisons use it.
    #[inline(always)]
    fn get_f32(&self, index: usize) -> f32 {
        debug_assert!(self.kind == Kind::F32 && index < self.count);
        // SAFETY: `index < count`, and the span was bounds-checked at construction.
        unsafe { self.base.add(index * self.stride).cast::<f32>().read_unaligned() }
    }

    /// Stores `bits` as element `index`, so that no floating-point operation touches a NaN here.
    #[inline(always)]
    fn set_f32_bits(&self, index: usize, bits: u32) {
        debug_assert!(self.kind == Kind::F32 && index < self.count);
        // SAFETY: as `get_f32`; buffers are writable storage.
        unsafe { self.base.add(index * self.stride).cast::<u32>().write_unaligned(bits) }
    }

    /// Element `index` of an `f32` span as a plain read and write would carry it: bit for bit,
    /// except that a NaN leaves quiet, as the widening read in `get` makes it.
    #[inline(always)]
    fn quieted_f32_bits(&self, index: usize) -> u32 {
        let value = self.get_f32(index);
        if value.is_nan() { value.to_bits() | QUIET } else { value.to_bits() }
    }
}

impl Reduction {
    pub(super) fn run_contiguous(self, span: Span, threshold: f64) -> Answer {
        match (self.op, self.filter) {
            (ReductionOp::Sum, None) => Answer::Number(sum(span)),
            (ReductionOp::Sum, Some(comparison)) => Answer::Number(sum_where(span, comparison, threshold)),
            (ReductionOp::Min | ReductionOp::Max, filter) => {
                let greatest = self.op == ReductionOp::Max;
                let found = match filter {
                    None => extreme(span, greatest),
                    Some(comparison) => extreme_where(span, greatest, comparison, threshold),
                };
                found.map_or(Answer::Nil, |i| Answer::Number(span.get(i)))
            }
            (ReductionOp::Count, Some(comparison)) => Answer::Number(count(span, comparison, threshold) as f64),
            (ReductionOp::Any, Some(comparison)) => Answer::Bool(find(span, comparison, threshold, true)),
            (ReductionOp::All, Some(comparison)) => Answer::Bool(!find(span, comparison, threshold, false)),
            (ReductionOp::Count, None) => Answer::Number(span.count as f64),
            (ReductionOp::Any, None) => Answer::Bool(span.count > 0),
            (ReductionOp::All, None) => Answer::Bool(true),
        }
    }

    pub(super) fn run_strided(self, span: Span, threshold: f64) -> Answer {
        match (self.op, self.filter) {
            (ReductionOp::Sum, None) => Answer::Number(sum_strided(span)),
            (ReductionOp::Sum, Some(comparison)) => Answer::Number(sum_where_strided(span, comparison, threshold)),
            (ReductionOp::Min | ReductionOp::Max, filter) => {
                let greatest = self.op == ReductionOp::Max;
                let found = match filter {
                    None => extreme_strided(span, greatest),
                    Some(comparison) => extreme_where_strided(span, greatest, comparison, threshold),
                };
                found.map_or(Answer::Nil, |i| Answer::Number(span.get(i)))
            }
            (ReductionOp::Count, Some(comparison)) => Answer::Number(count_strided(span, comparison, threshold) as f64),
            (ReductionOp::Any, Some(comparison)) => Answer::Bool(find_strided(span, comparison, threshold, true)),
            (ReductionOp::All, Some(comparison)) => Answer::Bool(!find_strided(span, comparison, threshold, false)),
            (ReductionOp::Count, None) => Answer::Number(span.count as f64),
            (ReductionOp::Any, None) => Answer::Bool(span.count > 0),
            (ReductionOp::All, None) => Answer::Bool(true),
        }
    }
}

impl Comparison {
    /// The `f32` threshold under which comparing an `f32` element gives the answer comparing both
    /// as `f64` gives. Between two adjacent `f32` values no element can land, so an inexact
    /// threshold moves to whichever neighbour keeps the side it is on: the greatest `f32` not
    /// above it for `>` and `<=`, the least not below it for `<` and `>=`. No `f32` equals a
    /// threshold that has no exact `f32`, which is how NaN compares, so `==` and `!=` use NaN.
    pub(super) fn narrow(self, threshold: f64) -> f32 {
        let rounded = threshold as f32;
        let wide = f64::from(rounded);
        match self {
            Comparison::Eq | Comparison::Ne if wide == threshold => rounded,
            Comparison::Eq | Comparison::Ne => f32::NAN,
            Comparison::Gt | Comparison::Le if wide > threshold => rounded.next_down(),
            Comparison::Lt | Comparison::Ge if wide < threshold => rounded.next_up(),
            _ => rounded,
        }
    }

    #[inline(always)]
    pub(super) fn test_f32(self, value: f32, threshold: f32) -> bool {
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

/// For contiguous `f32`, LLVM narrows this addition, which rounds the same: 53 bits is more than
/// twice 24, plus two.
#[inline(always)]
pub(super) fn add_loop<const C: bool>(left: Span, right: Span, out: Span) {
    let kind = out.kind;
    constant_kind!(kind => {
        let (left, right, out) = (left.with_kind::<C>(kind), right.with_kind::<C>(kind), out.with_kind::<C>(kind));
        // In place is the ordinary case and an overlap check would refuse it. Reading and writing
        // through one pointer tells LLVM the distance is zero, so that loop vectorizes too. Only
        // contiguous floats vectorize, so everything else has one loop for every placement.
        if !(C && kind.is_float()) {
            add_elements(left, right, out);
        } else if left.base == out.base {
            add_elements(out, right, out);
        } else if right.base == out.base {
            add_elements(left, out, out);
        } else {
            add_elements(left, right, out);
        }
    });
}

/// The factor is a double that an `f32` often cannot hold, so this one stays widened.
#[inline(always)]
pub(super) fn scale_loop<const C: bool>(source: Span, factor: f64, out: Span) {
    let kind = out.kind;
    constant_kind!(kind => {
        let (source, out) = (source.with_kind::<C>(kind), out.with_kind::<C>(kind));
        // As `add_loop`: one pointer for in place, for the kinds that vectorize.
        if C && kind.is_float() && source.base == out.base {
            scale_elements(out, factor, out);
        } else {
            scale_elements(source, factor, out);
        }
    });
}

#[inline(always)]
pub(super) fn clamp_loop<const C: bool>(source: Span, low: f64, high: f64, out: Span) {
    let kind = out.kind;
    constant_kind!(kind => {
        let (source, out) = (source.with_kind::<C>(kind), out.with_kind::<C>(kind));
        if C && kind == Kind::F32 {
            clamp_narrowed(source, low, high, out);
        } else if C && kind.is_float() && source.base == out.base {
            // As `add_loop`: one pointer for in place, for the kinds that vectorize.
            clamp_elements(out, low, high, out);
        } else {
            clamp_elements(source, low, high, out);
        }
    });
}

#[inline(always)]
fn clamp_narrowed(source: Span, low: f64, high: f64, out: Span) {
    let (below, above) = (Comparison::Lt.narrow(low), Comparison::Gt.narrow(high));
    // The bounds round as any write rounds.
    let (low, high) = ((low as f32).to_bits(), (high as f32).to_bits());
    // No element is above infinity, so a constant one folds that compare away: `math.huge` as the
    // high bound is a rectifier, common enough for a loop of its own.
    match (source.base == out.base, above == f32::INFINITY) {
        // As `add_loop`: one pointer for in place.
        (true, true) => clamp_narrowed_elements(out, out, below, f32::INFINITY, low, high),
        (true, false) => clamp_narrowed_elements(out, out, below, above, low, high),
        (false, true) => clamp_narrowed_elements(source, out, below, f32::INFINITY, low, high),
        (false, false) => clamp_narrowed_elements(source, out, below, above, low, high),
    }
}

#[inline(always)]
fn clamp_narrowed_elements(source: Span, out: Span, below: f32, above: f32, low: u32, high: u32) {
    for i in 0..out.count {
        let value = source.get_f32(i);
        // A NaN passes through, since neither bound compares, and leaves quiet: a plain loop
        // widens every element, which quiets it. Nothing here would, so set the bit.
        let passed = source.quieted_f32_bits(i);
        let bits = if value < below {
            low
        } else if value > above {
            high
        } else {
            passed
        };
        out.set_f32_bits(i, bits);
    }
}

#[inline(always)]
pub(super) fn fill_loop<const C: bool>(span: Span, value: f64) {
    let kind = span.kind;
    constant_kind!(kind => fill_elements(span.with_kind::<C>(kind), value));
}

/// In element order, because that is the answer: a chain of additions no vector can shorten.
#[inline(always)]
fn sum_loop<const C: bool>(span: Span) -> f64 {
    let kind = span.kind;
    constant_kind!(kind => {
        let span = span.with_kind::<C>(kind);
        let mut total = 0.0;
        for i in 0..span.count {
            total += span.get(i);
        }
        total
    })
}

/// The elements `test` accepts, added in element order. A block with none costs one vector test.
/// In a block with some, a rejected element adds `+0.0`, which changes nothing and avoids the
/// branch: the total starts at `+0.0` and so can never be `-0.0`, the one value it would change.
#[inline(always)]
fn sum_where_blocks(count: usize, test: impl Fn(usize) -> bool, span: Span) -> f64 {
    let (mut total, mut begin) = (0.0, 0);
    while begin < count {
        let end = (begin + SCAN_BLOCK).min(count);
        let mut found = false;
        for i in begin..end {
            found |= test(i);
        }
        if found {
            for i in begin..end {
                let value = span.get(i);
                total += if test(i) { value } else { 0.0 };
            }
        }
        begin = end;
    }
    total
}

#[inline(always)]
pub(super) fn sum_where_loop<const C: bool>(span: Span, comparison: Comparison, threshold: f64) -> f64 {
    with_element_test!(span, comparison, threshold => sum_where_blocks(span))
}

/// The first element `accept` takes, then the replace-when-strictly-better rule in order, with a
/// block test in front: a branch on every element is what keeps a plain loop scalar and
/// mispredicting, and almost no block holds an improvement. A block that does is rescanned in
/// order, so a tie keeps the first and a NaN (which nothing betters, and which betters nothing)
/// behaves as it does there.
#[inline(always)]
#[allow(clippy::needless_bitwise_bool)]
fn extreme_blocks<T: Copy>(
    count: usize,
    accept: impl Fn(usize) -> bool,
    get: impl Fn(usize) -> T,
    better: impl Fn(T, T) -> bool,
) -> Option<usize> {
    let first = first_blocks(count, &accept)?;
    let (mut best, mut value, mut begin) = (first, get(first), first + 1);
    while begin < count {
        let end = (begin + SCAN_BLOCK).min(count);
        let mut improves = false;
        for i in begin..end {
            improves |= accept(i) & better(get(i), value);
        }
        if improves {
            for i in begin..end {
                let candidate = get(i);
                if accept(i) && better(candidate, value) {
                    best = i;
                    value = candidate;
                }
            }
        }
        begin = end;
    }
    Some(best)
}

#[inline(always)]
pub(super) fn extreme_loop<const C: bool>(span: Span, greatest: bool) -> Option<usize> {
    let kind = span.kind;
    constant_kind!(kind => {
        let span = span.with_kind::<C>(kind);
        match (C && kind == Kind::F32, greatest) {
            (true, true) => extreme_blocks(span.count, |_| true, |i| span.get_f32(i), |candidate, value| candidate > value),
            (true, false) => extreme_blocks(span.count, |_| true, |i| span.get_f32(i), |candidate, value| candidate < value),
            (false, true) => extreme_blocks(span.count, |_| true, |i| span.get(i), |candidate, value| candidate > value),
            (false, false) => extreme_blocks(span.count, |_| true, |i| span.get(i), |candidate, value| candidate < value),
        }
    })
}

#[inline(always)]
fn extreme_where_blocks(count: usize, test: impl Fn(usize) -> bool, span: Span, greatest: bool) -> Option<usize> {
    if greatest {
        extreme_blocks(count, test, |i| span.get(i), |candidate, value| candidate > value)
    } else {
        extreme_blocks(count, test, |i| span.get(i), |candidate, value| candidate < value)
    }
}

#[inline(always)]
pub(super) fn extreme_where_loop<const C: bool>(
    span: Span,
    greatest: bool,
    comparison: Comparison,
    threshold: f64,
) -> Option<usize> {
    // Contiguous `f32` gets the constant loops. A loop per kind and comparison on top of that took
    // more code than every other operation together, so the rest share one plain loop.
    if !(C && span.kind == Kind::F32) {
        return extreme_where_blocks(span.count, |i| comparison.test(span.get(i), threshold), span, greatest);
    }
    let span = f32_at::<4>(span);
    let narrowed = comparison.narrow(threshold);
    constant_comparison!(comparison => {
        let accept = |i| comparison.test_f32(span.get_f32(i), narrowed);
        if greatest {
            extreme_blocks(span.count, accept, |i| span.get_f32(i), |candidate, value| candidate > value)
        } else {
            extreme_blocks(span.count, accept, |i| span.get_f32(i), |candidate, value| candidate < value)
        }
    })
}

#[inline(always)]
fn count_blocks(count: usize, test: impl Fn(usize) -> bool) -> usize {
    let (mut selected, mut begin) = (0, 0);
    while begin < count {
        let end = (begin + COUNT_BLOCK).min(count);
        let mut hits = 0u32;
        for i in begin..end {
            hits += u32::from(test(i));
        }
        selected += hits as usize;
        begin = end;
    }
    selected
}

/// The first element that passes `test`, a block at a time, starting small and doubling so that a
/// span decided early pays little. Stopping at a deciding block rather than a deciding element is
/// not observable: the test reads and nothing else.
#[inline(always)]
fn first_blocks(count: usize, test: impl Fn(usize) -> bool) -> Option<usize> {
    let (mut begin, mut block) = (0, FIRST_SCAN_BLOCK);
    while begin < count {
        let end = (begin + block).min(count);
        let mut found = false;
        for i in begin..end {
            found |= test(i);
        }
        if found {
            return (begin..end).find(|&i| test(i));
        }
        begin = end;
        block = (block * 2).min(SCAN_BLOCK);
    }
    None
}

/// Whether any element gives `test` the answer `wanted`: `any` wants true; `all` wants false and
/// inverts the result.
#[inline(always)]
fn find_blocks(count: usize, test: impl Fn(usize) -> bool, wanted: bool) -> bool {
    let (mut begin, mut block) = (0, FIRST_SCAN_BLOCK);
    while begin < count {
        let end = (begin + block).min(count);
        let mut found = false;
        for i in begin..end {
            found |= test(i) == wanted;
        }
        if found {
            return true;
        }
        begin = end;
        block = (block * 2).min(SCAN_BLOCK);
    }
    false
}

#[inline(always)]
fn select_blocks(count: usize, test: impl Fn(usize) -> bool, words: &mut [u64]) {
    for (word, chunk) in words.iter_mut().zip((0..count).step_by(64)) {
        let mut mask = 0u64;
        for bit in 0..64.min(count - chunk) {
            mask |= u64::from(test(chunk + bit)) << bit;
        }
        *word = mask;
    }
}

#[inline(always)]
pub(super) fn count_loop<const C: bool>(span: Span, comparison: Comparison, threshold: f64) -> usize {
    with_element_test!(span, comparison, threshold => count_blocks())
}

#[inline(always)]
pub(super) fn find_loop<const C: bool>(span: Span, comparison: Comparison, threshold: f64, wanted: bool) -> bool {
    with_element_test!(span, comparison, threshold => find_blocks(wanted))
}

#[inline(always)]
pub(super) fn select_loop<const C: bool>(span: Span, comparison: Comparison, threshold: f64, words: &mut [u64]) {
    with_element_test!(span, comparison, threshold => select_blocks(words));
}

/// An `f32` span with its stride as the constant `S`: the caller has seen it is.
#[inline(always)]
fn f32_at<const S: usize>(span: Span) -> Span {
    debug_assert!(span.kind == Kind::F32 && span.stride == S);
    Span { kind: Kind::F32, stride: S, ..span }
}

#[inline(always)]
fn extreme_f32_at<const S: usize>(span: Span, greatest: bool) -> Option<usize> {
    let span = f32_at::<S>(span);
    if greatest {
        extreme_blocks(span.count, |_| true, |i| span.get_f32(i), |candidate, value| candidate > value)
    } else {
        extreme_blocks(span.count, |_| true, |i| span.get_f32(i), |candidate, value| candidate < value)
    }
}

#[inline(always)]
fn count_f32_at<const S: usize>(span: Span, comparison: Comparison, threshold: f64) -> usize {
    let span = f32_at::<S>(span);
    let narrowed = comparison.narrow(threshold);
    constant_comparison!(comparison => count_blocks(span.count, |i| comparison.test_f32(span.get_f32(i), narrowed)))
}

#[inline(always)]
fn find_f32_at<const S: usize>(span: Span, comparison: Comparison, threshold: f64, wanted: bool) -> bool {
    let span = f32_at::<S>(span);
    let narrowed = comparison.narrow(threshold);
    constant_comparison!(comparison => find_blocks(span.count, |i| comparison.test_f32(span.get_f32(i), narrowed), wanted))
}

#[inline(always)]
fn select_f32_at<const S: usize>(span: Span, comparison: Comparison, threshold: f64, words: &mut [u64]) {
    let span = f32_at::<S>(span);
    let narrowed = comparison.narrow(threshold);
    constant_comparison!(comparison => select_blocks(span.count, |i| comparison.test_f32(span.get_f32(i), narrowed), words));
}

/// `destination[i] = source[indices[i]]`, in index order, read then write. `C` is the source's;
/// the destination is always contiguous. An `f32` goes by its bits, since a constant kind would
/// let LLVM fold the widening that a plain loop's read and write quiet a NaN with.
#[inline(always)]
pub(super) fn gather_loop<const C: bool>(source: Span, indices: &Indices, destination: Span) {
    let kind = source.kind;
    constant_kind!(kind => {
        let (source, destination) = (source.with_kind::<C>(kind), destination.with_kind::<true>(kind));
        for i in 0..indices.count {
            if kind == Kind::F32 {
                destination.set_f32_bits(i, source.quieted_f32_bits(indices.get(i)));
            } else {
                destination.set(i, source.get(indices.get(i)));
            }
        }
    });
}

/// `destination[indices[i]] = source[i]`, in index order. `C` is the destination's; the source is
/// always contiguous.
#[inline(always)]
pub(super) fn scatter_loop<const C: bool>(source: Span, indices: &Indices, destination: Span) {
    let kind = source.kind;
    constant_kind!(kind => {
        let (source, destination) = (source.with_kind::<true>(kind), destination.with_kind::<C>(kind));
        for i in 0..source.count {
            if kind == Kind::F32 {
                destination.set_f32_bits(indices.get(i), source.quieted_f32_bits(i));
            } else {
                destination.set(indices.get(i), source.get(i));
            }
        }
    });
}

/// Satisfying positions first, then the rest, each in order: one write per element, to a slot
/// chosen without a branch, which is the one a plain loop's branch would choose, in the same order.
#[inline(always)]
fn partition_blocks(count: usize, test: impl Fn(usize) -> bool, out: &Indices) -> usize {
    let accepted = count_blocks(count, &test);
    let (mut front, mut back) = (0, accepted);
    for i in 0..count {
        let hit = test(i);
        out.set(if hit { front } else { back }, i);
        front += usize::from(hit);
        back += usize::from(!hit);
    }
    accepted
}

#[inline(always)]
pub(super) fn partition_loop<const C: bool>(
    span: Span,
    comparison: Comparison,
    threshold: f64,
    out: &Indices,
) -> usize {
    with_element_test!(span, comparison, threshold => partition_blocks(out))
}

/// An element as a key whose unsigned order is [`key_order`]'s: ascending, with both zeros equal
/// and every NaN equal and last. Two equal keys are exactly two elements `key_order` calls equal.
#[inline(always)]
fn order_key(kind: Kind, span: &Span, index: usize) -> u64 {
    if kind == Kind::F32 {
        let value = span.get_f32(index);
        if value.is_nan() {
            return u64::from(u32::MAX);
        }
        let bits = (value + 0.0).to_bits();
        u64::from(if bits >> 31 == 0 { bits | 1 << 31 } else { !bits })
    } else if kind == Kind::F64 {
        let value = span.get(index);
        if value.is_nan() {
            return u64::MAX;
        }
        let bits = (value + 0.0).to_bits();
        if bits >> 63 == 0 { bits | 1 << 63 } else { !bits }
    } else {
        // Exact in a double. A signed kind is offset by half its range, which keeps it in its own
        // width with negatives below.
        let offset = match kind {
            Kind::I8 => 1 << 7,
            Kind::I16 => 1 << 15,
            Kind::I32 => 1 << 31,
            _ => 0,
        };
        (span.get(index) as i64 + offset).cast_unsigned()
    }
}

/// The permutation that orders the keys, stable, read once before anything is written, so `out`
/// may overlap them. Each key of at most four bytes is packed with its position, which makes every
/// pair distinct, so an unstable sort gives the one stable order; from [`RADIX_FROM`] keys, a radix
/// sort of those pairs.
#[inline(always)]
fn sort_keys_loop(keys: Span, out: &Indices) {
    let kind = keys.kind;
    constant_kind!(kind => {
        let keys = keys.with_kind::<false>(kind);
        if kind == Kind::F64 {
            // Eight radix passes lose to a stable sort of integer keys.
            let mut order: Vec<(u64, u32)> = (0..keys.count).map(|i| (order_key(kind, &keys, i), i as u32)).collect();
            order.sort_by_key(|pair| pair.0);
            for (slot, (_, index)) in order.iter().enumerate() {
                out.set(slot, *index as usize);
            }
        } else if keys.count >= RADIX_FROM {
            let order: Vec<u64> = (0..keys.count).map(|i| order_key(kind, &keys, i) << 32 | i as u64).collect();
            radix_sort_packed(order, kind.size(), out);
        } else {
            // Every other key fits 32 bits, so a key and its position pack into one word.
            let mut order: Vec<u64> = (0..keys.count).map(|i| order_key(kind, &keys, i) << 32 | i as u64).collect();
            order.sort_unstable();
            for (slot, packed) in order.iter().enumerate() {
                out.set(slot, (*packed & 0xffff_ffff) as usize);
            }
        }
    });
}

/// The same permutation with no allocation: the positions move between `out` and the caller's
/// scratch, a least-significant-digit radix sort from [`RADIX_FROM`] keys, a merge below it. Both
/// stable. The caller has checked that none of the three overlap.
#[inline(always)]
fn sort_with_scratch_loop(keys: Span, out: &Indices, scratch: &Indices) {
    // Below the radix length the keys are few, and one merge for every kind is plenty.
    if keys.count < RADIX_FROM {
        merge_sort(keys.count, |i, j| key_order(keys.get(i), keys.get(j)), out, scratch);
        return;
    }
    let kind = keys.kind;
    constant_kind!(kind => {
        let keys = keys.with_kind::<false>(kind);
        radix_sort(keys.count, kind.size(), |i| order_key(kind, &keys, i), out, scratch);
    });
}

#[inline(always)]
fn radix_sort(total: usize, bytes: usize, key: impl Fn(usize) -> u64, out: &Indices, scratch: &Indices) {
    let mut histograms = [[0u32; 256]; 8];
    for position in 0..total {
        let value = key(position);
        for (byte, histogram) in histograms.iter_mut().enumerate().take(bytes) {
            histogram[(value >> (byte * 8)) as usize & 0xff] += 1;
        }
        out.set(position, position);
    }
    let (mut from, mut to) = (out, scratch);
    for (byte, histogram) in histograms.iter().enumerate().take(bytes) {
        // Every key shares this digit: the pass would copy, so skip it.
        if histogram.contains(&(total as u32)) {
            continue;
        }
        let mut offsets = [0u32; 256];
        let mut offset = 0;
        for (slot, tally) in offsets.iter_mut().zip(histogram) {
            *slot = offset;
            offset += tally;
        }
        for slot in 0..total {
            let position = from.get(slot);
            let digit = (key(position) >> (byte * 8)) as usize & 0xff;
            to.set(offsets[digit] as usize, position);
            offsets[digit] += 1;
        }
        std::mem::swap(&mut from, &mut to);
    }
    if !std::ptr::eq(from, out) {
        for position in 0..total {
            out.set(position, from.get(position));
        }
    }
}

/// A least-significant-digit radix sort of keys packed above their positions, which `out` receives.
/// Each pass reads and writes in sequence, where looking a key up by position would miss the cache
/// on nearly every element once the keys outgrow it.
#[inline(always)]
fn radix_sort_packed(mut from: Vec<u64>, bytes: usize, out: &Indices) {
    let total = from.len();
    let mut histograms = [[0u32; 256]; 4];
    for packed in &from {
        for (byte, histogram) in histograms.iter_mut().enumerate().take(bytes) {
            histogram[(packed >> (32 + byte * 8)) as usize & 0xff] += 1;
        }
    }
    let mut to = vec![0u64; total];
    for (byte, histogram) in histograms.iter().enumerate().take(bytes) {
        // Every key shares this digit: the pass would copy, so skip it.
        if histogram.contains(&(total as u32)) {
            continue;
        }
        let mut offsets = [0u32; 256];
        let mut offset = 0;
        for (slot, tally) in offsets.iter_mut().zip(histogram) {
            *slot = offset;
            offset += tally;
        }
        for packed in &from {
            let digit = (packed >> (32 + byte * 8)) as usize & 0xff;
            to[offsets[digit] as usize] = *packed;
            offsets[digit] += 1;
        }
        std::mem::swap(&mut from, &mut to);
    }
    for (slot, packed) in from.iter().enumerate() {
        out.set(slot, (*packed & 0xffff_ffff) as usize);
    }
}

/// A bottom-up stable merge of the positions, alternating between `out` and `scratch`.
#[inline(always)]
fn merge_sort(total: usize, compare: impl Fn(usize, usize) -> std::cmp::Ordering, out: &Indices, scratch: &Indices) {
    for position in 0..total {
        out.set(position, position);
    }
    let (mut from, mut to) = (out, scratch);
    let mut width = 1;
    while width < total {
        let mut left = 0;
        while left < total {
            let mid = (left + width).min(total);
            let right = (left + 2 * width).min(total);
            let (mut lower, mut upper, mut slot) = (left, mid, left);
            while lower < mid && upper < right {
                let (first, second) = (from.get(lower), from.get(upper));
                // Strictly less moves the upper run's element first; ties keep the lower
                // (earlier) run's element: stable.
                if compare(second, first) == std::cmp::Ordering::Less {
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
    if !std::ptr::eq(from, out) {
        for position in 0..total {
            out.set(position, from.get(position));
        }
    }
}
