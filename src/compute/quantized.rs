//! Experimental symmetric int8 and packed int4 matrix-vector arithmetic. Quantization policy belongs
//! to the host. Weights and activations are signed, zero point zero. Accumulation is exact: a partial
//! sum of 2^16 products of int8 fits i32 even for -128 * -128, and totals are i64. Output applies
//! explicit positive finite f64 scales and rounds once to f32. This is not strict-f32 tensor matmul
//! semantics, and how well a real model survives quantizing is the host's to establish.
//!
//! The kernels are plain loops under each instruction set's features, which is enough: LLVM gives
//! `vpmovsxbw` and `vpmaddwd` under AVX2, and `sdot` under the AArch64 dot-product extension.
#![allow(clippy::inline_always)] // It is how a body gets compiled under each set of target features.
use crate::compute::error;

/// Longest slice whose i64 total can't overflow, at 2^14 per element.
const MAX_ELEMENTS: usize = 1 << 48;
/// Products per i32 partial sum: at most 2^14 each, so 2^30 in all, and a horizontal sum this rarely.
const PARTIAL: usize = 1 << 16;

/// Which instantiation of the integer kernels runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IntegerIsa {
    Baseline,
    /// AVX2 on `x86_64`, the dot-product extension on `aarch64`.
    Native,
}
impl IntegerIsa {
    #[must_use]
    pub fn detect() -> Self {
        if Self::Native.available() { Self::Native } else { Self::Baseline }
    }
    #[must_use]
    pub fn available(self) -> bool {
        match self {
            Self::Baseline => true,
            #[cfg(target_arch = "x86_64")]
            Self::Native => std::is_x86_feature_detected!("avx2"),
            #[cfg(target_arch = "aarch64")]
            Self::Native => std::arch::is_aarch64_feature_detected!("dotprod"),
            #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
            Self::Native => false,
        }
    }
}

#[inline(always)]
fn dot_body(left: &[i8], right: &[i8]) -> i64 {
    left.chunks(PARTIAL)
        .zip(right.chunks(PARTIAL))
        .map(|(a, b)| {
            let partial: i32 = a.iter().zip(b).map(|(x, y)| i32::from(*x) * i32::from(*y)).sum();
            i64::from(partial)
        })
        .sum()
}
/// One nibble's signed value, the low one when `high` is false.
#[inline(always)]
fn nibble<const HIGH: bool>(byte: u8) -> i8 {
    let shifted = if HIGH { byte } else { byte << 4 };
    i8::from_ne_bytes([shifted]) >> 4
}
/// Packed int4 weights, low nibble first, against int8 activations. A trailing odd activation reads
/// the last byte's low nibble.
#[inline(always)]
fn dot_i4_body(packed: &[u8], input: &[i8]) -> i64 {
    let (pairs, odd) = input.as_chunks::<2>();
    // Each product is at most 2^10, so these partials can't overflow either.
    let mut total: i64 = pairs
        .chunks(PARTIAL)
        .zip(packed.chunks(PARTIAL))
        .map(|(pairs, bytes)| {
            let partial: i32 = pairs
                .iter()
                .zip(bytes)
                .map(|(pair, byte)| {
                    i32::from(nibble::<false>(*byte)) * i32::from(pair[0])
                        + i32::from(nibble::<true>(*byte)) * i32::from(pair[1])
                })
                .sum();
            i64::from(partial)
        })
        .sum();
    if let [last] = odd {
        total += i64::from(nibble::<false>(packed[pairs.len()])) * i64::from(*last);
    }
    total
}
/// One output row: its exact sum scaled and rounded once. The scales multiply each other first, so
/// that scales only representable together stay representable.
#[inline(always)]
#[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]
fn scaled(accumulator: i64, scale: f64, input_scale: f64) -> f32 {
    // Exact in f64 up to 2^53, which takes 2^39 of the largest product.
    (accumulator as f64 * (scale * input_scale)) as f32
}
#[inline(always)]
fn matvec_body(weights: &[i8], input: &[i8], scales: &[f64], input_scale: f64, out: &mut [f32]) {
    if input.is_empty() {
        out.fill(0.0);
        return;
    }
    for ((row, out), scale) in weights.chunks_exact(input.len()).zip(out.iter_mut()).zip(scales) {
        *out = scaled(dot_body(row, input), *scale, input_scale);
    }
}
#[inline(always)]
fn matvec_i4_body(
    dot: impl Fn(&[u8], &[i8]) -> i64,
    weights: &[u8],
    input: &[i8],
    groups: Groups<'_>,
    input_scale: f64,
    out: &mut [f32],
) {
    if input.is_empty() {
        out.fill(0.0);
        return;
    }
    let stride = input.len().div_ceil(2);
    for (group, (rows, scale)) in weights.chunks(stride * groups.rows).zip(groups.scales).enumerate() {
        let first = group * groups.rows;
        for (row, out) in rows.chunks_exact(stride).zip(&mut out[first..]) {
            *out = scaled(dot(row, input), *scale, input_scale);
        }
    }
}

macro_rules! instantiate {
    ($dot:ident, $dot_i4:ident, $matvec:ident, $matvec_i4:ident $(, $features:literal)?) => {
        $(#[target_feature(enable = $features)])?
        fn $dot(left: &[i8], right: &[i8]) -> i64 {
            dot_body(left, right)
        }
        $(#[target_feature(enable = $features)])?
        fn $dot_i4(packed: &[u8], input: &[i8]) -> i64 {
            dot_i4_body(packed, input)
        }
        $(#[target_feature(enable = $features)])?
        fn $matvec(weights: &[i8], input: &[i8], scales: &[f64], input_scale: f64, out: &mut [f32]) {
            matvec_body(weights, input, scales, input_scale, out);
        }
        $(#[target_feature(enable = $features)])?
        fn $matvec_i4(weights: &[u8], input: &[i8], groups: Groups<'_>, input_scale: f64, out: &mut [f32]) {
            matvec_i4_body(|row, input| dot_i4_body(row, input), weights, input, groups, input_scale, out);
        }
    };
}
instantiate!(dot_baseline, dot_i4_baseline, matvec_baseline, matvec_i4_baseline);
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
#[allow(clippy::cast_ptr_alignment)] // Every load below tolerates any alignment.
fn dot_native(left: &[i8], right: &[i8]) -> i64 {
    use std::arch::x86_64::{
        __m128i, _mm_add_epi32, _mm_cvtsi128_si32, _mm_hadd_epi32, _mm_loadu_si128, _mm256_add_epi32,
        _mm256_castsi256_si128, _mm256_cvtepi8_epi16, _mm256_extracti128_si256, _mm256_madd_epi16,
        _mm256_setzero_si256,
    };
    let mut total = 0i64;
    for (a, b) in left.chunks(PARTIAL).zip(right.chunks(PARTIAL)) {
        let (a_blocks, a_tail) = a.as_chunks::<64>();
        let (b_blocks, b_tail) = b.as_chunks::<64>();
        // Four chains overlap in the pipeline. A chain gains 2^15 at most per 16 elements, and a
        // partial is 2^16 elements, so no lane comes near overflowing.
        let mut sums = [_mm256_setzero_si256(); 4];
        for (x, y) in a_blocks.iter().zip(b_blocks) {
            for (lane, sum) in sums.iter_mut().enumerate() {
                let at = lane * 16;
                // SAFETY: `at + 16 <= 64` bytes of each block; these loads tolerate any alignment.
                let (p, q) = unsafe {
                    (
                        _mm_loadu_si128(x[at..].as_ptr().cast::<__m128i>()),
                        _mm_loadu_si128(y[at..].as_ptr().cast::<__m128i>()),
                    )
                };
                *sum = _mm256_add_epi32(*sum, _mm256_madd_epi16(_mm256_cvtepi8_epi16(p), _mm256_cvtepi8_epi16(q)));
            }
        }
        let [s0, s1, s2, s3] = sums;
        let all = _mm256_add_epi32(_mm256_add_epi32(s0, s1), _mm256_add_epi32(s2, s3));
        let wide = _mm_add_epi32(_mm256_castsi256_si128(all), _mm256_extracti128_si256::<1>(all));
        let pairs = _mm_hadd_epi32(wide, wide);
        total += i64::from(_mm_cvtsi128_si32(_mm_hadd_epi32(pairs, pairs)));
        total += dot_body(a_tail, b_tail);
    }
    total
}
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
#[allow(clippy::cast_ptr_alignment)] // Every load below tolerates any alignment.
fn dot_i4_native(packed: &[u8], input: &[i8]) -> i64 {
    use std::arch::x86_64::{
        __m128i, _mm_add_epi32, _mm_and_si128, _mm_cvtsi128_si32, _mm_hadd_epi32, _mm_loadl_epi64, _mm_loadu_si128,
        _mm_set1_epi8, _mm_setr_epi8, _mm_shuffle_epi8, _mm_srli_epi16, _mm_unpacklo_epi8, _mm256_add_epi32,
        _mm256_castsi256_si128, _mm256_cvtepi8_epi16, _mm256_extracti128_si256, _mm256_madd_epi16,
        _mm256_setzero_si256,
    };
    let signed = _mm_setr_epi8(0, 1, 2, 3, 4, 5, 6, 7, -8, -7, -6, -5, -4, -3, -2, -1);
    let (values, value_tail) = input.as_chunks::<16>();
    let (bytes, byte_tail) = packed.split_at(values.len() * 8);
    let bytes = bytes.as_chunks::<8>().0;
    // A lane gains 2^11 at most per 16 elements, so the chain would need 2^20 blocks to overflow.
    let mut total = 0i64;
    for (bytes, values) in bytes.chunks(1 << 16).zip(values.chunks(1 << 16)) {
        let mut sum = _mm256_setzero_si256();
        for (packed, x) in bytes.iter().zip(values) {
            // SAFETY: 8 and 16 bytes of arrays that long; these loads tolerate any alignment.
            let (packed, x) = unsafe {
                (_mm_loadl_epi64(packed.as_ptr().cast::<__m128i>()), _mm_loadu_si128(x.as_ptr().cast::<__m128i>()))
            };
            let mask = _mm_set1_epi8(0x0f);
            let low = _mm_and_si128(packed, mask);
            let high = _mm_and_si128(_mm_srli_epi16::<4>(packed), mask);
            let weights = _mm_shuffle_epi8(signed, _mm_unpacklo_epi8(low, high));
            sum = _mm256_add_epi32(sum, _mm256_madd_epi16(_mm256_cvtepi8_epi16(weights), _mm256_cvtepi8_epi16(x)));
        }
        let wide = _mm_add_epi32(_mm256_castsi256_si128(sum), _mm256_extracti128_si256::<1>(sum));
        let pairs = _mm_hadd_epi32(wide, wide);
        total += i64::from(_mm_cvtsi128_si32(_mm_hadd_epi32(pairs, pairs)));
    }
    total + dot_i4_body(byte_tail, value_tail)
}
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
fn matvec_native(weights: &[i8], input: &[i8], scales: &[f64], input_scale: f64, out: &mut [f32]) {
    if input.is_empty() {
        out.fill(0.0);
        return;
    }
    for ((row, out), scale) in weights.chunks_exact(input.len()).zip(out.iter_mut()).zip(scales) {
        *out = scaled(dot_native(row, input), *scale, input_scale);
    }
}
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
fn matvec_i4_native(weights: &[u8], input: &[i8], groups: Groups<'_>, input_scale: f64, out: &mut [f32]) {
    matvec_i4_body(|row, input| dot_i4_native(row, input), weights, input, groups, input_scale, out);
}
#[cfg(target_arch = "aarch64")]
instantiate!(dot_native, dot_i4_native, matvec_native, matvec_i4_native, "dotprod");

/// `$native` when `$isa` is the native instantiation, which `$isa.available()` must already confirm.
macro_rules! dispatch {
    ($isa:expr, $native:expr, $baseline:expr) => {
        match $isa {
            #[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
            // SAFETY: every caller asserts this instantiation is available before dispatching.
            IntegerIsa::Native => unsafe { $native },
            _ => $baseline,
        }
    };
}

/// Exact integer dot product of equal lengths, with no temporary allocation.
#[must_use]
pub fn dot(left: &[i8], right: &[i8]) -> i64 {
    dot_with(left, right, IntegerIsa::detect())
}
/// [`dot`] on a chosen instantiation.
#[must_use]
pub fn dot_with(left: &[i8], right: &[i8], isa: IntegerIsa) -> i64 {
    assert_eq!(left.len(), right.len());
    assert!(left.len() <= MAX_ELEMENTS);
    assert!(isa.available());
    dispatch!(isa, dot_native(left, right), dot_baseline(left, right))
}
/// Exact product of packed int4 weights, low nibble first, with int8 activations. `packed` holds
/// `input.len().div_ceil(2)` bytes.
#[must_use]
pub fn dot_i4(packed: &[u8], input: &[i8]) -> i64 {
    dot_i4_with(packed, input, IntegerIsa::detect())
}
/// [`dot_i4`] on a chosen instantiation.
#[must_use]
pub fn dot_i4_with(packed: &[u8], input: &[i8], isa: IntegerIsa) -> i64 {
    assert_eq!(packed.len(), input.len().div_ceil(2));
    assert!(input.len() <= MAX_ELEMENTS);
    assert!(isa.available());
    dispatch!(isa, dot_i4_native(packed, input), dot_i4_baseline(packed, input))
}

fn valid_scale(scale: f64, input_scale: f64) -> bool {
    scale.is_finite() && scale > 0.0 && (scale * input_scale).is_finite()
}

/// Matrix-vector product with one scale per weight row and one activation scale.
/// Validates all dimensions and scales before writing output. No allocation or worker creation.
pub fn matvec(weights: &[i8], input: &[i8], scales: &[f64], input_scale: f64, out: &mut [f32]) -> crate::Result<()> {
    matvec_with(weights, input, scales, input_scale, out, IntegerIsa::detect())
}
/// [`matvec`] on a chosen instantiation.
pub fn matvec_with(
    weights: &[i8],
    input: &[i8],
    scales: &[f64],
    input_scale: f64,
    out: &mut [f32],
    isa: IntegerIsa,
) -> crate::Result<()> {
    if weights.len() > MAX_ELEMENTS
        || input.len() > MAX_ELEMENTS
        || out.len().checked_mul(input.len()) != Some(weights.len())
        || scales.len() != out.len()
        || !input_scale.is_finite()
        || input_scale <= 0.0
        || !scales.iter().all(|scale| valid_scale(*scale, input_scale))
    {
        return Err(error("invalid int8 matrix-vector dimensions or scales"));
    }
    assert!(isa.available());
    dispatch!(
        isa,
        matvec_native(weights, input, scales, input_scale, out),
        matvec_baseline(weights, input, scales, input_scale, out)
    );
    Ok(())
}

/// One scale per `rows` consecutive weight rows, the last group possibly shorter.
#[derive(Clone, Copy, Debug)]
pub struct Groups<'a> {
    pub rows: usize,
    pub scales: &'a [f64],
}

/// Packed int4 weights, `input.len().div_ceil(2)` bytes per row, low nibble first, times int8
/// activations: half the weight memory of int8, for one scale per group of rows.
pub fn matvec_i4(
    weights: &[u8],
    input: &[i8],
    groups: Groups<'_>,
    input_scale: f64,
    out: &mut [f32],
) -> crate::Result<()> {
    matvec_i4_with(weights, input, groups, input_scale, out, IntegerIsa::detect())
}
/// [`matvec_i4`] on a chosen instantiation.
pub fn matvec_i4_with(
    weights: &[u8],
    input: &[i8],
    groups: Groups<'_>,
    input_scale: f64,
    out: &mut [f32],
    isa: IntegerIsa,
) -> crate::Result<()> {
    let stride = input.len().div_ceil(2);
    let wanted_groups = if groups.rows == 0 { None } else { Some(out.len().div_ceil(groups.rows)) };
    if input.len() > MAX_ELEMENTS
        || out.len().checked_mul(stride) != Some(weights.len())
        || wanted_groups != Some(groups.scales.len())
        || !input_scale.is_finite()
        || input_scale <= 0.0
        || !groups.scales.iter().all(|scale| valid_scale(*scale, input_scale))
    {
        return Err(error("invalid int4 matrix-vector dimensions or scales"));
    }
    assert!(isa.available());
    // A group longer than the output is one whole group, however long it claims to be. Past here its
    // bytes are at most `weights.len()`, which the check above has shown is a real length, or one
    // stride when there are no rows at all, so they can't overflow or come to nothing.
    let groups = Groups { rows: groups.rows.min(out.len().max(1)), scales: groups.scales };
    dispatch!(
        isa,
        matvec_i4_native(weights, input, groups, input_scale, out),
        matvec_i4_baseline(weights, input, groups, input_scale, out)
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    /// Every instantiation this host has: a test that passes on one has said nothing about another.
    fn isas() -> Vec<IntegerIsa> {
        [IntegerIsa::Baseline, IntegerIsa::Native].into_iter().filter(|isa| isa.available()).collect()
    }
    fn oracle(left: &[i8], right: &[i8]) -> i128 {
        left.iter().zip(right).map(|(x, y)| i128::from(*x) * i128::from(*y)).sum()
    }
    fn generator(seed: u64) -> impl FnMut() -> u64 {
        let mut state = seed;
        move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        }
    }
    #[test]
    fn exact_accumulation_covers_blocks_and_signed_extremes() {
        for isa in isas() {
            // Past every vector width and either side of the partial's end.
            for len in
                [0, 1, 7, 8, 15, 16, 17, 31, 32, 33, 63, 64, 65, 4095, 4096, 4097, 65_535, 65_536, 65_537, 1_048_576]
            {
                let (lowest, highest) = (vec![-128i8; len], vec![127i8; len]);
                let count = i64::try_from(len).unwrap();
                assert_eq!(dot_with(&lowest, &lowest, isa), count * 16384, "{len} {isa:?}");
                assert_eq!(dot_with(&lowest, &highest, isa), count * -16256, "{len} {isa:?}");
                assert_eq!(dot_with(&highest, &highest, isa), count * 16129, "{len} {isa:?}");
                let alternating: Vec<i8> = (0..len).map(|i| if i % 2 == 0 { -128 } else { 127 }).collect();
                assert_eq!(i128::from(dot_with(&alternating, &lowest, isa)), oracle(&alternating, &lowest), "{len}");
            }
            let a: Vec<i8> = (-128..=127).cycle().take(8193).collect();
            let b: Vec<i8> = (-128..=127).rev().cycle().take(8193).collect();
            assert_eq!(i128::from(dot_with(&a, &b, isa)), oracle(&a, &b), "{isa:?}");
        }
    }
    /// More than one partial's worth of the largest product, so an overflowing partial shows.
    #[test]
    fn long_chains_of_the_largest_product_stay_exact() {
        for isa in isas() {
            let len = 3 * PARTIAL + 5;
            let lowest = vec![-128i8; len];
            assert_eq!(dot_with(&lowest, &lowest, isa), i64::try_from(len).unwrap() * 16384);
        }
    }
    #[test]
    fn deterministic_random_products_match_i128_oracle() {
        let mut next = generator(0x1234_5678_9abc_def1);
        let mut sample = move || i8::from_ne_bytes([next().to_ne_bytes()[3]]);
        for columns in [0, 1, 7, 16, 31, 128, 4097] {
            let input: Vec<i8> = (0..columns).map(|_| sample()).collect();
            let weights: Vec<i8> = (0..3 * columns).map(|_| sample()).collect();
            let scales = [0.125, 0.5, 2.0];
            for isa in isas() {
                let mut output = [f32::NAN; 3];
                matvec_with(&weights, &input, &scales, 0.25, &mut output, isa).unwrap();
                for row in 0..3 {
                    let exact: i128 = oracle(&weights[row * columns..(row + 1) * columns], &input);
                    #[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]
                    let expected = (exact as f64 * scales[row] * 0.25) as f32;
                    assert_eq!(output[row].to_bits(), expected.to_bits());
                }
            }
        }
    }
    /// The scales multiply each other first. Ordinary values can't show it, because an f64 rounding
    /// difference almost never survives conversion to f32, but scales that only fit once they have
    /// met each other can.
    #[test]
    fn the_scales_multiply_each_other_first() {
        let mut out = [f32::NAN];
        matvec(&[-128], &[-128], &[1e305], 1e-305, &mut out).unwrap();
        assert!((out[0] - 16384.0).abs() < 0.01, "applied one at a time this overflows: {}", out[0]);
        matvec(&[-128], &[-128], &[1e-305], 1e-20, &mut out).unwrap();
        assert_eq!(out[0], 0.0, "the product underflows to zero at f32, as it should");
    }
    /// Results a f32 can't tell from the exact product, for scales that aren't powers of two.
    #[test]
    #[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]
    fn results_stay_within_one_place_of_the_exact_dequantized_value() {
        let mut next = generator(0x0123_4567_89ab_cdef);
        for columns in [1usize, 15, 16, 17, 4097] {
            let weights: Vec<i8> = (0..columns).map(|_| i8::from_ne_bytes([next().to_ne_bytes()[1]])).collect();
            let input: Vec<i8> = (0..columns).map(|_| i8::from_ne_bytes([next().to_ne_bytes()[2]])).collect();
            let (numerator, denominator) = (next() % 9973 + 1, next() % 9967 + 1);
            let scale = numerator as f64 / denominator as f64;
            let exact = oracle(&weights, &input);
            let mut out = [f32::NAN];
            matvec(&weights, &input, &[scale], 1.0, &mut out).unwrap();
            // The exact accumulator, from i128 rather than the kernel's accumulation.
            let expected = exact as f64 * numerator as f64 / denominator as f64;
            let place = f64::from(f32::EPSILON) * expected.abs().max(f64::from(f32::MIN_POSITIVE));
            assert!((f64::from(out[0]) - expected).abs() <= place, "{columns}: {} vs {expected}", out[0]);
        }
    }
    #[test]
    fn scales_and_validation_are_explicit() {
        let mut out = [9.0; 2];
        matvec(&[1, 2, -3, 4], &[2, -1], &[0.5, 0.25], 0.5, &mut out).unwrap();
        assert_eq!(out.map(f32::to_bits), [0, (-1.25f32).to_bits()]);
        let before = out;
        assert!(matvec(&[1, 2, -3, 4], &[2, -1], &[0.5, f64::NAN], 0.5, &mut out).is_err());
        assert_eq!(out, before);
        assert!(matvec(&[1], &[1, 2], &[1.0; 2], 1.0, &mut out).is_err());
        matvec(&[], &[], &[1.0; 2], 1.0, &mut out).unwrap();
        assert_eq!(out.map(f32::to_bits), [0; 2]);
    }

    /// Weights in `-8..=7` packed as `dot_i4` reads them, with their int8 forms for an oracle.
    fn packed_i4(values: &[i8]) -> Vec<u8> {
        let mut packed = vec![0u8; values.len().div_ceil(2)];
        for (index, value) in values.iter().enumerate() {
            assert!((-8..=7).contains(value));
            let nibble = value.to_ne_bytes()[0] & 0x0f;
            packed[index / 2] |= if index % 2 == 0 { nibble } else { nibble << 4 };
        }
        packed
    }
    #[test]
    fn packed_int4_weights_mean_what_their_int8_forms_mean() {
        let mut next = generator(0x0dd5_eed5_0000_0001);
        let extremes: [(i8, i8); 4] = [(-8, -128), (7, 127), (-8, 127), (7, -128)];
        for len in [0usize, 1, 2, 3, 7, 8, 15, 16, 17, 31, 32, 33, 63, 64, 65, 255, 4097, 2 * PARTIAL + 3] {
            for isa in isas() {
                for (weight, activation) in extremes {
                    let (weights, input) = (vec![weight; len], vec![activation; len]);
                    assert_eq!(i128::from(dot_i4_with(&packed_i4(&weights), &input, isa)), oracle(&weights, &input));
                }
                let weights: Vec<i8> =
                    (0..len).map(|_| i8::from_ne_bytes([next().to_ne_bytes()[1] & 0xf]) - 8).collect();
                let input: Vec<i8> = (0..len).map(|_| i8::from_ne_bytes([next().to_ne_bytes()[2]])).collect();
                assert_eq!(
                    i128::from(dot_i4_with(&packed_i4(&weights), &input, isa)),
                    oracle(&weights, &input),
                    "{len} {isa:?}"
                );
            }
        }
    }
    #[test]
    #[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]
    fn grouped_scales_apply_to_their_own_rows_and_the_short_last_group() {
        let mut next = generator(0x5eed_0000_0000_0001);
        for (rows, group, columns) in [(1usize, 1usize, 1usize), (7, 3, 5), (8, 4, 33), (9, 2, 64), (3, 8, 17)] {
            let weights: Vec<i8> =
                (0..rows * columns).map(|_| i8::from_ne_bytes([next().to_ne_bytes()[1] & 0xf]) - 8).collect();
            let input: Vec<i8> = (0..columns).map(|_| i8::from_ne_bytes([next().to_ne_bytes()[2]])).collect();
            let packed: Vec<u8> = weights.chunks_exact(columns).flat_map(packed_i4).collect();
            let scales: Vec<f64> = (0..rows.div_ceil(group)).map(|g| 0.1 + g as f64 * 0.37).collect();
            for isa in isas() {
                let mut out = vec![f32::NAN; rows];
                let groups = Groups { rows: group, scales: &scales };
                matvec_i4_with(&packed, &input, groups, 0.3, &mut out, isa).unwrap();
                for (row, actual) in out.iter().enumerate() {
                    let exact = oracle(&weights[row * columns..(row + 1) * columns], &input);
                    let expected = (exact as f64 * (scales[row / group] * 0.3)) as f32;
                    assert_eq!(actual.to_bits(), expected.to_bits(), "{rows}x{columns}, row {row} {isa:?}");
                }
            }
        }
    }
    /// One scale for everything is a real request however large the group is said to be, and a size
    /// whose bytes overflow must not become a panic or a wrong group.
    #[test]
    fn a_group_longer_than_the_output_is_one_group() {
        let (weights, input) = ([0x21u8, 0x43, 0x65, 0x07], [1i8, 1, 1, 1]);
        for isa in isas() {
            for rows in [2, 3, 1 << 62, 1 << 63, usize::MAX] {
                let mut out = [f32::NAN; 2];
                let groups = Groups { rows, scales: &[0.5] };
                matvec_i4_with(&weights, &input, groups, 1.0, &mut out, isa).unwrap();
                assert_eq!(out, [5.0, 9.0], "{rows} {isa:?}");
            }
            // The size reported first: two bytes a row, wrapping to no bytes at all.
            let mut one = [f32::NAN];
            matvec_i4_with(&[0x11; 2], &input, Groups { rows: 1 << 63, scales: &[1.0] }, 1.0, &mut one, isa).unwrap();
            assert_eq!(one, [4.0]);
            // No rows need no scales, whatever size their groups would have been.
            for rows in [1, 1 << 63, usize::MAX] {
                let none = Groups { rows, scales: &[] };
                matvec_i4_with(&[], &input, none, 1.0, &mut [], isa).unwrap();
            }
            // A size can't excuse the wrong number of scales.
            let mut out = [f32::NAN; 2];
            let extra = Groups { rows: 1 << 63, scales: &[1.0, 1.0] };
            assert!(matvec_i4_with(&weights, &input, extra, 1.0, &mut out, isa).is_err());
            assert!(out.iter().all(|value| value.is_nan()));
        }
    }
    #[test]
    fn int4_validation_refuses_before_writing() {
        let mut out = [9.0f32; 2];
        let good = Groups { rows: 1, scales: &[1.0, 1.0] };
        assert!(matvec_i4(&[0; 2], &[1, 2], good, 1.0, &mut out).is_ok());
        out = [9.0; 2];
        assert!(matvec_i4(&[0; 3], &[1, 2], good, 1.0, &mut out).is_err(), "too many weights");
        assert!(matvec_i4(&[0; 1], &[1, 2], good, 1.0, &mut out).is_err(), "too few weights");
        assert!(matvec_i4(&[0; 2], &[1, 2], Groups { rows: 0, scales: &[] }, 1.0, &mut out).is_err(), "empty group");
        assert!(
            matvec_i4(&[0; 2], &[1, 2], Groups { rows: 1, scales: &[1.0] }, 1.0, &mut out).is_err(),
            "a scale short"
        );
        assert!(
            matvec_i4(&[0; 2], &[1, 2], Groups { rows: 1, scales: &[1.0, -1.0] }, 1.0, &mut out).is_err(),
            "negative scale"
        );
        assert!(matvec_i4(&[0; 2], &[1, 2], good, f64::NAN, &mut out).is_err(), "NaN activation scale");
        assert_eq!(out, [9.0; 2]);
    }
}
