//! Row kernels for future typed-plan reduction stages. No allocation or runtime code generation.
//! Strict sum starts at +0 and visits inputs in increasing index order.

/// Ordered f32 reduction, with one rounding after every addition.
#[must_use]
pub fn sum(values: &[f32]) -> f32 {
    values.iter().fold(0.0, |acc, value| acc + value)
}

/// Stable softmax. The caller supplies equally sized, nonoverlapping output storage.
/// Any NaN poisons the row. Positive infinities share probability equally; all negative
/// infinities (an all-masked row) produce zeros. Empty rows produce an empty result.
/// Finite rows use f64 exponentials and accumulation, then round probabilities to f32.
/// This accurate normalization contract is distinct from ordered strict-f32 sum.
pub fn softmax(values: &[f32], out: &mut [f32]) {
    assert_eq!(values.len(), out.len());
    if values.is_empty() {
        return;
    }
    if values.iter().any(|v| v.is_nan()) {
        out.fill(f32::NAN);
        return;
    }
    let infinities = values.iter().filter(|v| **v == f32::INFINITY).count();
    if infinities != 0 {
        #[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]
        let probability = (1.0 / infinities as f64) as f32;
        for (slot, value) in out.iter_mut().zip(values) {
            *slot = if *value == f32::INFINITY { probability } else { 0.0 };
        }
        return;
    }
    let max = values.iter().copied().fold(f32::NEG_INFINITY, f32::max);
    if max == f32::NEG_INFINITY {
        out.fill(0.0);
        return;
    }
    let max = f64::from(max);
    let total: f64 = values.iter().map(|v| (f64::from(*v) - max).exp()).sum();
    for (slot, value) in out.iter_mut().zip(values) {
        #[allow(clippy::cast_possible_truncation)]
        {
            *slot = ((f64::from(*value) - max).exp() / total) as f32;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        softmax(&[], &mut empty);
        let mut out = [0.0; 3];
        softmax(&[f32::NEG_INFINITY; 3], &mut out);
        assert!(out.iter().all(|v| v.to_bits() == 0));
        softmax(&[f32::INFINITY, 0.0, f32::INFINITY], &mut out);
        assert_eq!(out.map(f32::to_bits), [0.5f32.to_bits(), 0, 0.5f32.to_bits()]);
        softmax(&[0.0, f32::NAN, 1.0], &mut out);
        assert!(out.iter().all(|v| v.is_nan()));
        softmax(&[f32::MAX, 0.0, -f32::MAX], &mut out);
        assert_eq!(out.map(f32::to_bits), [1.0f32.to_bits(), 0, 0]);
    }

    #[test]
    fn softmax_matches_independent_unshifted_reference_and_translation() {
        for n in [1, 7, 16, 33, 4097] {
            #[allow(clippy::cast_precision_loss)]
            let input: Vec<f32> = (0..n).map(|i| (i % 31) as f32 * 0.25 - 4.0).collect();
            let denominator: f64 = input.iter().map(|v| f64::from(*v).exp()).sum();
            let mut out = vec![0.0; n];
            softmax(&input, &mut out);
            for (value, actual) in input.iter().zip(&out) {
                let expected = f64::from(*value).exp() / denominator;
                assert!((f64::from(*actual) - expected).abs() <= expected * 1e-7);
            }
            let shifted: Vec<f32> = input.iter().map(|v| v + 1024.0).collect();
            let mut translated = vec![0.0; n];
            softmax(&shifted, &mut translated);
            assert_eq!(out, translated);
        }
    }
}

/// Accurate row normalization, without affine weights. Uses f64 statistics and f32 outputs.
/// `center` selects population layer normalization; false selects RMS normalization.
/// Nonfinite inputs propagate through IEEE arithmetic. Empty rows are accepted.
/// Epsilon must be finite and strictly positive; validation happens before any output write.
pub fn normalize(values: &[f32], out: &mut [f32], epsilon: f64, center: bool) -> l3i::Result<()> {
    if values.len() != out.len() || !epsilon.is_finite() || epsilon <= 0.0 {
        return Err(crate::error("normalization requires matching lengths and finite positive epsilon"));
    }
    if values.is_empty() {
        return Ok(());
    }
    #[allow(clippy::cast_precision_loss)]
    let count = values.len() as f64;
    let mean = if center { values.iter().map(|v| f64::from(*v)).sum::<f64>() / count } else { 0.0 };
    let variance = values
        .iter()
        .map(|v| {
            let d = f64::from(*v) - mean;
            d * d
        })
        .sum::<f64>()
        / count;
    let divisor = (variance + epsilon).sqrt();
    for (slot, value) in out.iter_mut().zip(values) {
        #[allow(clippy::cast_possible_truncation)]
        {
            *slot = ((f64::from(*value) - mean) / divisor) as f32;
        }
    }
    Ok(())
}

#[cfg(test)]
mod normalization_tests {
    use super::normalize;

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
