//! Row kernels: ordered sums, accurate softmax and accurate normalization. Nothing here allocates
//! or generates code, and every instantiation produces the same bits, because the kernels use only
//! IEEE addition, multiplication, division, square root and narrowing, in an order the source fixes.
//!
//! Sums over a row's f64 values accumulate into [`LANES`] independent totals by index modulo
//! [`LANES`], then add those in lane order. That order is part of the contract: it is what lets the
//! loops vectorize without reassociation, and it loses no accuracy to a single ordered sum.
// `inline(always)` is how a body gets compiled under each instantiation's target features.
#![allow(clippy::inline_always, clippy::cast_possible_truncation, clippy::cast_precision_loss)]
use crate::compute::kernels::{Element, Isa};

/// Independent f64 accumulators per row.
pub(crate) const LANES: usize = 4;
/// Below this `x`, [`exp_nonpositive`] returns +0. `exp(-104)` is under half the smallest f32
/// subnormal, and every softmax denominator is at least one, so such a probability rounds to +0
/// and a million such terms cannot move the denominator.
pub(crate) const EXP_CUTOFF: f64 = -104.0;

/// One element as a kernel sees it: an `f32`, or little-endian bytes at any alignment.
pub(crate) trait Lane: Copy {
    fn get(self) -> f32;
    fn put(value: f32) -> Self;
}
impl Lane for f32 {
    #[inline(always)]
    fn get(self) -> f32 {
        self
    }
    #[inline(always)]
    fn put(value: f32) -> Self {
        value
    }
}
impl Lane for Element {
    #[inline(always)]
    fn get(self) -> f32 {
        f32::from_le_bytes(self)
    }
    #[inline(always)]
    fn put(value: f32) -> Self {
        value.to_le_bytes()
    }
}

/// `e^x` for `x <= 0`, within about one f64 ulp on `[EXP_CUTOFF, 0]` and +0 below it.
///
/// Branchless, so it vectorizes at any width, and built only from IEEE operations, so unlike the
/// platform's `exp` it returns the same bits on every host. Cody-Waite reduction to
/// `|r| <= ln(2)/2`, then the degree-13 Taylor polynomial, whose truncation error there is about
/// 4e-18, then a scale by `2^k` assembled from the bits of the rounded quotient.
#[inline(always)]
#[must_use]
pub fn exp_nonpositive(x: f64) -> f64 {
    const INV_LN2: f64 = std::f64::consts::LOG2_E;
    const LN2_HI: f64 = 6.931_471_803_691_238e-1;
    const LN2_LO: f64 = 1.908_214_929_270_587_7e-10;
    // Adding 1.5 * 2^52 rounds to an integer, kept in the low mantissa bits. This is the one
    // float-to-integer step AVX2 can do across all lanes at once.
    const SHIFT: f64 = 6_755_399_441_055_744.0;
    let rounded = x * INV_LN2 + SHIFT;
    let k = rounded - SHIFT;
    let r = x - k * LN2_HI - k * LN2_LO;
    // Estrin's scheme, not Horner's: the terms group into a short dependency chain, so a pipeline
    // overlaps neighbouring vectors, and the leading `1 + r` is added last, where it keeps the
    // rounding of everything smaller to under an ulp of the result.
    let r2 = r * r;
    let r4 = r2 * r2;
    let low = (1.0 / 2.0 + r * (1.0 / 6.0)) + r2 * (1.0 / 24.0 + r * (1.0 / 120.0));
    let middle = (1.0 / 720.0 + r * (1.0 / 5_040.0)) + r2 * (1.0 / 40_320.0 + r * (1.0 / 362_880.0));
    let high =
        (1.0 / 3_628_800.0 + r * (1.0 / 39_916_800.0)) + r2 * (1.0 / 479_001_600.0 + r * (1.0 / 6_227_020_800.0));
    let p = 1.0 + (r + r2 * (low + r4 * (middle + r4 * high)));
    // Only the low twelve bits survive the shift, which hold `k + 1023` for every `x` in range.
    let scale = f64::from_bits(rounded.to_bits().wrapping_add(1023) << 52);
    if x < EXP_CUTOFF { 0.0 } else { p * scale }
}

/// What a row's special values decide, before any arithmetic.
enum RowClass {
    Nan,
    Infinities(usize),
    AllMasked,
    Finite(f32),
}
#[inline(always)]
fn classify<L: Lane>(values: &[L]) -> RowClass {
    const WIDTH: usize = 8;
    // Separate loops, as in `softmax_body`. A NaN never compares greater, so `max` skips it.
    let any_nan = values.iter().fold(0u32, |any, value| any | u32::from(value.get().to_bits() << 1 > 0xff00_0000));
    let mut max = [f32::NEG_INFINITY; WIDTH];
    let (blocks, rest) = values.as_chunks::<WIDTH>();
    for block in blocks {
        for i in 0..WIDTH {
            let value = block[i].get();
            max[i] = if value > max[i] { value } else { max[i] };
        }
    }
    for (i, value) in rest.iter().enumerate() {
        let value = value.get();
        max[i] = if value > max[i] { value } else { max[i] };
    }
    if any_nan != 0 {
        RowClass::Nan
    } else if max.contains(&f32::INFINITY) {
        let count = values.iter().filter(|value| value.get() == f32::INFINITY).count();
        RowClass::Infinities(count)
    } else {
        let max = max.iter().copied().fold(f32::NEG_INFINITY, |a, b| if b > a { b } else { a });
        if max == f32::NEG_INFINITY { RowClass::AllMasked } else { RowClass::Finite(max) }
    }
}

/// The f64 elements of scratch one softmax row of `len` columns needs.
#[must_use]
pub const fn softmax_scratch(len: usize) -> usize {
    len.next_multiple_of(LANES)
}

/// The sum of `values` under the contract's accumulation order.
#[inline(always)]
fn lane_total(values: &[f64]) -> f64 {
    let mut totals = [0.0f64; LANES];
    let (blocks, rest) = values.as_chunks::<LANES>();
    for block in blocks {
        for lane in 0..LANES {
            totals[lane] += block[lane];
        }
    }
    for (lane, value) in rest.iter().enumerate() {
        totals[lane] += *value;
    }
    totals[0] + totals[1] + totals[2] + totals[3]
}

#[inline(always)]
fn softmax_body<L: Lane>(values: &[L], out: &mut [L], exponentials: &mut [f64]) {
    assert_eq!(values.len(), out.len());
    let max = match classify(values) {
        RowClass::Nan => return out.fill(L::put(f32::NAN)),
        RowClass::AllMasked => return out.fill(L::put(0.0)),
        RowClass::Infinities(count) => {
            #[allow(clippy::cast_precision_loss)] // Past 2^53 infinities, the share isn't a number f32 has either.
            let share = L::put((1.0 / count as f64) as f32);
            for (slot, value) in out.iter_mut().zip(values) {
                *slot = if value.get() == f32::INFINITY { share } else { L::put(0.0) };
            }
            return;
        }
        RowClass::Finite(max) => f64::from(max),
    };
    // Plain loops, because each one vectorizes alone and a loop doing all of it does not. Padding
    // to whole vectors means no element takes the scalar remainder, which short rows would feel.
    // Whatever the padding holds is exponentiated and never read.
    let padded = &mut exponentials[..softmax_scratch(values.len())];
    for (value, target) in values.iter().zip(padded.iter_mut()) {
        *target = f64::from(value.get()) - max;
    }
    for target in padded.iter_mut() {
        *target = exp_nonpositive(*target);
    }
    let exponentials = &padded[..values.len()];
    let total = lane_total(exponentials);
    // A total of at least one, so the reciprocal is exact to half an ulp and well away from
    // underflow. One more f64 rounding per element cannot reach the f32 result but rarely.
    let inverse = 1.0 / total;
    for (slot, exponential) in out.iter_mut().zip(exponentials) {
        *slot = L::put((*exponential * inverse) as f32);
    }
}

#[inline(always)]
fn normalize_body<L: Lane>(values: &[L], out: &mut [L], epsilon: f64, center: bool) {
    assert_eq!(values.len(), out.len());
    let count = values.len() as f64;
    let (blocks, rest) = values.as_chunks::<LANES>();
    let mean = if center {
        let mut totals = [0.0f64; LANES];
        for block in blocks {
            for lane in 0..LANES {
                totals[lane] += f64::from(block[lane].get());
            }
        }
        for (lane, value) in rest.iter().enumerate() {
            totals[lane] += f64::from(value.get());
        }
        (totals[0] + totals[1] + totals[2] + totals[3]) / count
    } else {
        0.0
    };
    let mut totals = [0.0f64; LANES];
    for block in blocks {
        for lane in 0..LANES {
            let deviation = f64::from(block[lane].get()) - mean;
            totals[lane] += deviation * deviation;
        }
    }
    for (lane, value) in rest.iter().enumerate() {
        let deviation = f64::from(value.get()) - mean;
        totals[lane] += deviation * deviation;
    }
    let variance = (totals[0] + totals[1] + totals[2] + totals[3]) / count;
    let divisor = (variance + epsilon).sqrt();
    for (slot, value) in out.iter_mut().zip(values) {
        *slot = L::put(((f64::from(value.get()) - mean) / divisor) as f32);
    }
}

/// Rows at once in [`sum_rows_body`]. Each sum keeps its order and its one rounding per addition,
/// but independent sums overlap in the pipeline where one sum's additions must wait for each other.
const SUM_ROWS: usize = 8;
/// Writes the ordered sum of each `cols`-wide row of `rows` to `out`, rows `stride` elements apart.
#[inline(always)]
fn sum_rows_body(rows: &[Element], stride: usize, cols: usize, out: &mut [Element]) {
    // No columns reads nothing, and an empty view's stride names rows that need not exist.
    if cols == 0 {
        out.fill(0.0f32.to_le_bytes());
        return;
    }
    let row = |index: usize| &rows[index * stride..][..cols];
    let (groups, rest) = out.as_chunks_mut::<SUM_ROWS>();
    let mut first = 0;
    for group in groups {
        let lanes: [&[Element]; SUM_ROWS] = std::array::from_fn(|lane| row(first + lane));
        let mut sums = [0.0f32; SUM_ROWS];
        #[allow(clippy::needless_range_loop)] // The same column of every lane, in increasing order.
        for col in 0..cols {
            for lane in 0..SUM_ROWS {
                sums[lane] += f32::from_le_bytes(lanes[lane][col]);
            }
        }
        for (slot, sum) in group.iter_mut().zip(sums) {
            *slot = sum.to_le_bytes();
        }
        first += SUM_ROWS;
    }
    for (slot, index) in rest.iter_mut().zip(first..) {
        *slot = ordered_sum(row(index)).to_le_bytes();
    }
}

/// Ordered f32 reduction, with one rounding after every addition. Starts at +0.
#[inline(always)]
pub(crate) fn ordered_sum<L: Lane>(values: &[L]) -> f32 {
    values.iter().fold(0.0, |acc, value| acc + value.get())
}

#[inline(never)]
fn softmax_baseline(values: &[Element], out: &mut [Element], exponentials: &mut [f64]) {
    softmax_body(values, out, exponentials);
}
#[inline(never)]
fn normalize_baseline(values: &[Element], out: &mut [Element], epsilon: f64, center: bool) {
    normalize_body(values, out, epsilon, center);
}
#[inline(never)]
fn sum_rows_baseline(rows: &[Element], stride: usize, cols: usize, out: &mut [Element]) {
    sum_rows_body(rows, stride, cols, out);
}
#[cfg(target_arch = "x86_64")]
mod x86 {
    use super::{Element, normalize_body, softmax_body, sum_rows_body};
    #[target_feature(enable = "avx2")]
    pub(super) fn softmax(values: &[Element], out: &mut [Element], exponentials: &mut [f64]) {
        softmax_body(values, out, exponentials);
    }
    #[target_feature(enable = "avx2")]
    pub(super) fn normalize(values: &[Element], out: &mut [Element], epsilon: f64, center: bool) {
        normalize_body(values, out, epsilon, center);
    }
    #[target_feature(enable = "avx2")]
    pub(super) fn sum_rows(rows: &[Element], stride: usize, cols: usize, out: &mut [Element]) {
        sum_rows_body(rows, stride, cols, out);
    }
}

/// One softmax row on `isa`. FMA is never used here, so all instantiations agree bit for bit.
pub(crate) fn softmax_row(values: &[Element], out: &mut [Element], exponentials: &mut [f64], isa: Isa) {
    assert!(isa.available(), "{isa:?} is not available on this host");
    match isa {
        #[cfg(target_arch = "x86_64")]
        // SAFETY: availability was just asserted, and both x86 instantiations include AVX2.
        Isa::Avx2 | Isa::Fma => unsafe { x86::softmax(values, out, exponentials) },
        _ => softmax_baseline(values, out, exponentials),
    }
}
/// One normalization row on `isa`, with `epsilon` already validated. See [`softmax_row`].
pub(crate) fn normalize_row(values: &[Element], out: &mut [Element], epsilon: f64, center: bool, isa: Isa) {
    assert!(isa.available(), "{isa:?} is not available on this host");
    match isa {
        #[cfg(target_arch = "x86_64")]
        // SAFETY: as for `softmax_row`.
        Isa::Avx2 | Isa::Fma => unsafe { x86::normalize(values, out, epsilon, center) },
        _ => normalize_baseline(values, out, epsilon, center),
    }
}
/// Ordered sums of evenly spaced contiguous rows on `isa`. See [`softmax_row`].
pub(crate) fn sum_rows(rows: &[Element], stride: usize, cols: usize, out: &mut [Element], isa: Isa) {
    assert!(isa.available(), "{isa:?} is not available on this host");
    match isa {
        #[cfg(target_arch = "x86_64")]
        // SAFETY: as for `softmax_row`.
        Isa::Avx2 | Isa::Fma => unsafe { x86::sum_rows(rows, stride, cols, out) },
        _ => sum_rows_baseline(rows, stride, cols, out),
    }
}

/// Ordered f32 reduction, with one rounding after every addition. Starts at +0.
#[must_use]
pub fn sum(values: &[f32]) -> f32 {
    ordered_sum(values)
}

/// Stable softmax of one row. `exponentials` is scratch of at least [`softmax_scratch`] elements.
///
/// Any NaN poisons the row. Positive infinities share probability equally; all negative infinities
/// (an all-masked row) produce zeros. Empty rows produce an empty result. Finite rows subtract the
/// row's maximum in f64, take [`exp_nonpositive`] once per element, accumulate in f64 and round each
/// probability to f32. This accurate contract is distinct from strict-f32 arithmetic.
pub fn softmax(values: &[f32], out: &mut [f32], exponentials: &mut [f64]) {
    softmax_body(values, out, exponentials);
}

/// Accurate row normalization, without affine weights. Uses f64 statistics and f32 outputs.
/// `center` selects population layer normalization; false selects RMS normalization.
/// Nonfinite inputs propagate through IEEE arithmetic. Empty rows are accepted.
/// Epsilon must be finite and strictly positive; validation happens before any output write.
pub fn normalize(values: &[f32], out: &mut [f32], epsilon: f64, center: bool) -> crate::Result<()> {
    if values.len() != out.len() || !epsilon.is_finite() || epsilon <= 0.0 {
        return Err(crate::compute::error("normalization requires matching lengths and finite positive epsilon"));
    }
    if !values.is_empty() {
        normalize_body(values, out, epsilon, center);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn softmax_alloc(values: &[f32], out: &mut [f32]) {
        softmax(values, out, &mut vec![0.0; softmax_scratch(values.len())]);
    }

    #[test]
    fn ordered_sum_keeps_cancellation_and_zero_contract() {
        assert_eq!(sum(&[]).to_bits(), 0);
        assert_eq!(sum(&[-0.0]).to_bits(), 0);
        assert_eq!(sum(&[16_777_216.0, 1.0, -16_777_216.0]).to_bits(), 0);
        assert!(sum(&[f32::INFINITY, f32::NEG_INFINITY]).is_nan());
        assert_eq!(sum(&[f32::from_bits(1), f32::from_bits(1)]).to_bits(), 2);
    }

    #[test]
    fn softmax_degenerate_and_extreme_rows() {
        let mut empty = [];
        softmax_alloc(&[], &mut empty);
        let mut out = [0.0; 3];
        softmax_alloc(&[f32::NEG_INFINITY; 3], &mut out);
        assert!(out.iter().all(|v| v.to_bits() == 0));
        softmax_alloc(&[f32::INFINITY, 0.0, f32::INFINITY], &mut out);
        assert_eq!(out.map(f32::to_bits), [0.5f32.to_bits(), 0, 0.5f32.to_bits()]);
        softmax_alloc(&[0.0, f32::NAN, 1.0], &mut out);
        assert!(out.iter().all(|v| v.is_nan()));
        softmax_alloc(&[f32::MAX, 0.0, -f32::MAX], &mut out);
        assert_eq!(out.map(f32::to_bits), [1.0f32.to_bits(), 0, 0]);
    }

    #[test]
    fn softmax_matches_independent_unshifted_reference_and_translation() {
        for n in [1, 7, 16, 33, 4097] {
            let input: Vec<f32> = (0..n).map(|i| (i % 31) as f32 * 0.25 - 4.0).collect();
            let denominator: f64 = input.iter().map(|v| f64::from(*v).exp()).sum();
            let mut out = vec![0.0; n];
            softmax_alloc(&input, &mut out);
            for (value, actual) in input.iter().zip(&out) {
                let expected = f64::from(*value).exp() / denominator;
                assert!((f64::from(*actual) - expected).abs() <= expected * 1e-7);
            }
            let shifted: Vec<f32> = input.iter().map(|v| v + 1024.0).collect();
            let mut translated = vec![0.0; n];
            softmax_alloc(&shifted, &mut translated);
            assert_eq!(out, translated);
        }
    }

    #[test]
    fn normalization_validates_before_writing_and_handles_extremes() {
        let mut out = [42.0; 2];
        for epsilon in [0.0, -1.0, f64::NAN, f64::INFINITY] {
            assert!(normalize(&[1.0, 2.0], &mut out, epsilon, true).is_err());
            assert_eq!(out.map(f32::to_bits), [42.0f32.to_bits(); 2]);
        }
        normalize(&[f32::MAX, -f32::MAX], &mut out, 1e-5, true).unwrap();
        assert_eq!(out.map(f32::to_bits), [1.0f32.to_bits(), (-1.0f32).to_bits()]);
        normalize(&[2.0, 2.0], &mut out, 1e-5, true).unwrap();
        assert_eq!(out.map(f32::to_bits), [0; 2]);
        normalize(&[], &mut [], 1e-5, false).unwrap();
    }

    #[test]
    fn normalization_matches_analytic_rows() {
        let mut out = [0.0; 2];
        normalize(&[3.0, 4.0], &mut out, 0.5, false).unwrap();
        for (actual, numerator) in out.into_iter().zip([3.0, 4.0]) {
            assert!((f64::from(actual) - numerator / 13.0f64.sqrt()).abs() < 1e-7);
        }
        normalize(&[1.0, 3.0], &mut out, 3.0, true).unwrap();
        assert_eq!(out.map(f32::to_bits), [(-0.5f32).to_bits(), 0.5f32.to_bits()]);
    }
}
