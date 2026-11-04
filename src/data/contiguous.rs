//! The contiguous loops: what `dream.data` runs when every span's stride is its element size.
//!
//! Each operation is chosen once, after its spans are checked. There are no intrinsics: these are
//! ordinary loops that lose the per-element kind dispatch to constant propagation and that LLVM
//! then vectorizes where it can, compiled twice, for the baseline target and for AVX2 behind a
//! run-time check, so every x86-64 and AArch64 target keeps working unchanged. Strided spans stay
//! on the generic loops in `mod.rs`, which are what these are tested against.
//!
//! The results are the generic loops', bit for bit, by three arguments:
//!
//! - Raw pointers and no references, so an overlap is defined and LLVM has to keep the sequential
//!   result, which it does with its own run-time overlap check and a scalar fallback.
//! - An `f32` element compared with an `f64` threshold has an exact `f32` equivalent, computed
//!   once ([`Comparison::narrow`]), which doubles the lanes against widening every element.
//! - A block test only decides whether a block needs looking at. A block that does runs the
//!   original rule in order, so first occurrences, ties and NaNs are unchanged.

use super::{Answer, Comparison, Kind, Reduction, Span, add_elements, clamp_elements, fill_elements, scale_elements};

/// Elements a block-wise scan tests before it looks at the answer. Long enough that the test is
/// nearly all vector code; short enough that a hit costs little.
const SCAN_BLOCK: usize = 64;
/// Elements counted into 32-bit lanes before they move to the running total. A `u32` count packs
/// twice the lanes of a `usize` one and cannot overflow in a block this size.
const COUNT_BLOCK: usize = 1 << 20;
/// Bit 22, which makes an `f32` NaN quiet.
const QUIET: u32 = 1 << 22;

/// `pub(super) fn $name`, which runs `$body` compiled for AVX2 on a CPU that has it and for the
/// baseline otherwise. `$body` must be `#[inline(always)]`: inlining is how it gets compiled twice.
macro_rules! with_best_vectors {
    ($(fn $name:ident($($argument:ident: $type:ty),*) $(-> $result:ty)? = $body:ident;)*) => {$(
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

with_best_vectors! {
    fn add(left: Span, right: Span, out: Span) = add_loop;
    fn scale(source: Span, factor: f64, out: Span) = scale_loop;
    fn clamp(source: Span, low: f64, high: f64, out: Span) = clamp_loop;
    fn fill(span: Span, value: f64) = fill_loop;
    fn extreme(span: Span, greatest: bool) -> Option<usize> = extreme_loop;
    fn count(span: Span, comparison: Comparison, threshold: f64) -> usize = count_loop;
    fn find(span: Span, comparison: Comparison, threshold: f64, wanted: bool) -> bool = find_loop;
    fn select(span: Span, comparison: Comparison, threshold: f64, words: &mut [u64]) = select_loop;
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
/// the kind constant. `f32` compares narrowed and `f64` as itself, one loop per comparison, where
/// the extra lanes pay for the code. An integer element is exact as a Luau number and never NaN,
/// so one loop per kind answers all six comparisons from a table.
macro_rules! with_element_test {
    ($span:ident, $comparison:ident, $threshold:ident => $run:ident($($extra:expr),*)) => {{
        let kind = $span.kind;
        constant_kind!(kind => {
            let $span = $span.with_kind(kind);
            if kind == Kind::F32 {
                constant_comparison!($comparison => {
                    let narrowed = $comparison.narrow($threshold);
                    $run($span.count, |i| $comparison.test_f32($span.get_f32(i), narrowed) $(, $extra)*)
                })
            } else if kind == Kind::F64 {
                constant_comparison!($comparison => {
                    $run($span.count, |i| $comparison.test($span.get(i), $threshold) $(, $extra)*)
                })
            } else {
                let table = OrderTable::of($comparison, $threshold);
                $run($span.count, |i| table.test($span.get(i), $threshold) $(, $extra)*)
            }
        })
    }};
}

/// A comparison as what it answers for an element below, equal to and above the threshold, for
/// elements that cannot be NaN. A NaN threshold makes all three compares false, leaving `Ne`'s true
/// to `unordered`.
#[derive(Clone, Copy)]
struct OrderTable {
    below: bool,
    equal: bool,
    above: bool,
    unordered: bool,
}

impl OrderTable {
    fn of(comparison: Comparison, threshold: f64) -> OrderTable {
        let (below, equal, above) = match comparison {
            Comparison::Eq => (false, true, false),
            Comparison::Ne => (true, false, true),
            Comparison::Lt => (true, false, false),
            Comparison::Le => (true, true, false),
            Comparison::Gt => (false, false, true),
            Comparison::Ge => (false, true, true),
        };
        OrderTable { below, equal, above, unordered: comparison == Comparison::Ne && threshold.is_nan() }
    }

    /// Branchless, so it vectorizes: no element picks a branch, each lane gets the same three
    /// compares and a mask. That is why these are not the lazy operators.
    #[inline(always)]
    #[allow(clippy::needless_bitwise_bool)]
    fn test(self, value: f64, threshold: f64) -> bool {
        (self.below & (value < threshold))
            | (self.equal & (value == threshold))
            | (self.above & (value > threshold))
            | self.unordered
    }
}

impl Kind {
    #[inline(always)]
    fn is_float(self) -> bool {
        matches!(self, Kind::F32 | Kind::F64)
    }
}

impl Span {
    /// The same span with this kind and its stride as constants. Inlined with a constant, a loop
    /// over `get` and `set` loses the per-element dispatch in `Kind::read` and `Kind::write`.
    #[inline(always)]
    fn with_kind(self, kind: Kind) -> Span {
        debug_assert!(self.kind == kind && self.is_contiguous());
        Span { kind, stride: kind.size(), ..self }
    }

    /// Element `index` as stored. Comparing these needs no widening; arithmetic on them would skip
    /// the quieting a widening read does, so only comparisons use it.
    #[inline(always)]
    fn get_f32(&self, index: usize) -> f32 {
        debug_assert!(self.kind == Kind::F32 && self.is_contiguous() && index < self.count);
        // SAFETY: `index < count`, and the span was bounds-checked at construction; contiguous
        // `f32` puts element `index` at four bytes each.
        unsafe { self.base.add(index * 4).cast::<f32>().read_unaligned() }
    }

    /// Stores `bits` as element `index`, so that no floating-point operation touches a NaN here.
    #[inline(always)]
    fn set_f32_bits(&self, index: usize, bits: u32) {
        debug_assert!(self.kind == Kind::F32 && self.is_contiguous() && index < self.count);
        // SAFETY: as `get_f32`; buffers are writable storage.
        unsafe { self.base.add(index * 4).cast::<u32>().write_unaligned(bits) }
    }
}

impl Reduction {
    /// The generic reduction with its kind and stride constant. The operation and filter stay run
    /// time here: this is the path for what vectors cannot shorten anyway.
    pub(super) fn run_constant_kind(self, span: Span, threshold: f64) -> Answer {
        let kind = span.kind;
        constant_kind!(kind => self.run_elements(&span.with_kind(kind), threshold))
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

/// For `f32`, LLVM narrows this addition, which rounds the same: 53 bits is more than twice 24,
/// plus two.
#[inline(always)]
pub(super) fn add_loop(left: Span, right: Span, out: Span) {
    let kind = out.kind;
    constant_kind!(kind => {
        let (left, right, out) = (left.with_kind(kind), right.with_kind(kind), out.with_kind(kind));
        // In place is the ordinary case and an overlap check would refuse it. Reading and writing
        // through one pointer tells LLVM the distance is zero, so that loop vectorizes too. An
        // integer's saturating write stays scalar anyway, so its one loop serves every placement.
        if !kind.is_float() {
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
pub(super) fn scale_loop(source: Span, factor: f64, out: Span) {
    let kind = out.kind;
    constant_kind!(kind => {
        let (source, out) = (source.with_kind(kind), out.with_kind(kind));
        // As `add_loop`: one pointer for in place, for the kinds that vectorize.
        if kind.is_float() && source.base == out.base {
            scale_elements(out, factor, out);
        } else {
            scale_elements(source, factor, out);
        }
    });
}

#[inline(always)]
pub(super) fn clamp_loop(source: Span, low: f64, high: f64, out: Span) {
    let kind = out.kind;
    constant_kind!(kind => {
        let (source, out) = (source.with_kind(kind), out.with_kind(kind));
        if kind == Kind::F32 {
            clamp_narrowed(source, low, high, out);
        } else if kind.is_float() && source.base == out.base {
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
        // A NaN passes through, since neither bound compares, and leaves quiet: the generic loop
        // widens every element, which quiets it. Nothing here would, so set the bit.
        let passed = if value.is_nan() { value.to_bits() | QUIET } else { value.to_bits() };
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
pub(super) fn fill_loop(span: Span, value: f64) {
    let kind = span.kind;
    constant_kind!(kind => fill_elements(span.with_kind(kind), value));
}

/// The generic scan's result with a block test in front: a branch on every element is what keeps
/// that loop scalar and mispredicting, and almost no block holds an improvement. A block that does
/// is rescanned by the same replace-when-strictly-better rule, in order, so a tie keeps the first
/// and a NaN (which nothing betters, and which betters nothing) behaves as it does there.
#[inline(always)]
fn extreme_blocks<T: Copy>(count: usize, get: impl Fn(usize) -> T, better: impl Fn(T, T) -> bool) -> Option<usize> {
    if count == 0 {
        return None;
    }
    let (mut best, mut value, mut begin) = (0, get(0), 1);
    while begin < count {
        let end = (begin + SCAN_BLOCK).min(count);
        let mut improves = false;
        for i in begin..end {
            improves |= better(get(i), value);
        }
        if improves {
            for i in begin..end {
                let candidate = get(i);
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

#[inline(always)]
pub(super) fn extreme_loop(span: Span, greatest: bool) -> Option<usize> {
    let kind = span.kind;
    constant_kind!(kind => {
        let span = span.with_kind(kind);
        match (kind == Kind::F32, greatest) {
            (true, true) => extreme_blocks(span.count, |i| span.get_f32(i), |candidate, value| candidate > value),
            (true, false) => extreme_blocks(span.count, |i| span.get_f32(i), |candidate, value| candidate < value),
            (false, true) => extreme_blocks(span.count, |i| span.get(i), |candidate, value| candidate > value),
            (false, false) => extreme_blocks(span.count, |i| span.get(i), |candidate, value| candidate < value),
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

/// Whether any element gives `test` the answer `wanted`, a block at a time. Stopping at the first
/// deciding block instead of the first deciding element is not observable: the test reads and
/// nothing else. `any` wants true; `all` wants false and inverts the result.
#[inline(always)]
fn find_blocks(count: usize, test: impl Fn(usize) -> bool, wanted: bool) -> bool {
    let mut begin = 0;
    while begin < count {
        let end = (begin + SCAN_BLOCK).min(count);
        let mut found = false;
        for i in begin..end {
            found |= test(i) == wanted;
        }
        if found {
            return true;
        }
        begin = end;
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
pub(super) fn count_loop(span: Span, comparison: Comparison, threshold: f64) -> usize {
    with_element_test!(span, comparison, threshold => count_blocks())
}

#[inline(always)]
pub(super) fn find_loop(span: Span, comparison: Comparison, threshold: f64, wanted: bool) -> bool {
    with_element_test!(span, comparison, threshold => find_blocks(wanted))
}

#[inline(always)]
pub(super) fn select_loop(span: Span, comparison: Comparison, threshold: f64, words: &mut [u64]) {
    with_element_test!(span, comparison, threshold => select_blocks(words));
}
