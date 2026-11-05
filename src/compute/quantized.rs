//! Experimental symmetric int8 matrix-vector arithmetic. Quantization policy belongs to the host.
//! Row-major weights and activations are signed int8, zero point zero. Accumulation is exact i64;
//! each 4096-element partial fits i32 even for -128 * -128. Output applies explicit positive
//! finite f64 scales and rounds once to f32. This is not strict-f32 tensor matmul semantics.
use crate::compute::{MAX_ELEMENTS, error};

/// Exact integer dot product. Equal lengths required; no temporary allocation.
#[must_use]
pub fn dot(left: &[i8], right: &[i8]) -> i64 {
    assert_eq!(left.len(), right.len());
    assert!(left.len() <= MAX_ELEMENTS);
    left.chunks(4096)
        .zip(right.chunks(4096))
        .map(|(a, b)| {
            let partial: i32 = a.iter().zip(b).map(|(x, y)| i32::from(*x) * i32::from(*y)).sum();
            i64::from(partial)
        })
        .sum()
}

/// Matrix-vector product with one scale per weight row and one activation scale.
/// Validates all dimensions and scales before writing output. No allocation or worker creation.
pub fn matvec(weights: &[i8], input: &[i8], scales: &[f64], input_scale: f64, out: &mut [f32]) -> crate::Result<()> {
    if weights.len() > MAX_ELEMENTS
        || input.len() > MAX_ELEMENTS
        || out.len() > MAX_ELEMENTS
        || out.len().checked_mul(input.len()) != Some(weights.len())
        || scales.len() != out.len()
        || !input_scale.is_finite()
        || input_scale <= 0.0
        || scales.iter().any(|s| !s.is_finite() || *s <= 0.0 || !(*s * input_scale).is_finite())
    {
        return Err(error("invalid int8 matrix-vector dimensions or scales"));
    }
    for (row, (out, scale)) in out.iter_mut().zip(scales).enumerate() {
        let start = row * input.len();
        let accumulator = dot(&weights[start..start + input.len()], input);
        // The maximum permitted dot is below 2^35, so conversion to f64 is exact.
        #[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]
        {
            *out = (accumulator as f64 * (*scale * input_scale)) as f32;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn exact_accumulation_covers_blocks_and_signed_extremes() {
        for len in [0, 1, 15, 16, 17, 4095, 4096, 4097, 1_048_576] {
            let a = vec![-128i8; len];
            let b = vec![-128i8; len];
            assert_eq!(dot(&a, &b), i64::try_from(len).unwrap() * 16384);
        }
        let a: Vec<i8> = (-128..=127).cycle().take(8193).collect();
        let b: Vec<i8> = (-128..=127).rev().cycle().take(8193).collect();
        let oracle: i128 = a.iter().zip(&b).map(|(x, y)| i128::from(*x) * i128::from(*y)).sum();
        assert_eq!(i128::from(dot(&a, &b)), oracle);
    }
    #[test]
    fn deterministic_random_products_match_i128_oracle() {
        let mut state = 0x1234_5678_u32;
        let mut sample = || {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            i8::from_ne_bytes([state.to_ne_bytes()[0]])
        };
        for columns in [0, 1, 7, 16, 31, 128, 4097] {
            let input: Vec<i8> = (0..columns).map(|_| sample()).collect();
            let weights: Vec<i8> = (0..3 * columns).map(|_| sample()).collect();
            let scales = [0.125, 0.5, 2.0];
            let mut output = [f32::NAN; 3];
            matvec(&weights, &input, &scales, 0.25, &mut output).unwrap();
            for row in 0..3 {
                let exact: i128 =
                    (0..columns).map(|i| i128::from(weights[row * columns + i]) * i128::from(input[i])).sum();
                #[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]
                let expected = (exact as f64 * scales[row] * 0.25) as f32;
                assert_eq!(output[row].to_bits(), expected.to_bits());
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
        let mut state = 0x0123_4567_89ab_cdef_u64;
        let mut next = || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        for columns in [1usize, 15, 16, 17, 4097] {
            let weights: Vec<i8> = (0..columns).map(|_| i8::from_ne_bytes([next().to_ne_bytes()[1]])).collect();
            let input: Vec<i8> = (0..columns).map(|_| i8::from_ne_bytes([next().to_ne_bytes()[2]])).collect();
            let (numerator, denominator) = (next() % 9973 + 1, next() % 9967 + 1);
            let scale = numerator as f64 / denominator as f64;
            let exact: i128 = weights.iter().zip(&input).map(|(x, y)| i128::from(*x) * i128::from(*y)).sum();
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
}
