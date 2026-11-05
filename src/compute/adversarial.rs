//! Row-stage and composition tests against oracles that share no code or formula with the kernels.
#![allow(clippy::many_single_char_names, clippy::cast_precision_loss, clippy::float_cmp)]
use crate::compute::*;
use std::sync::Arc;

pub(crate) fn bytes_of(values: &[f32]) -> Vec<u8> {
    values.iter().flat_map(|v| v.to_le_bytes()).collect()
}
pub(crate) fn floats(bytes: &[u8]) -> Vec<f32> {
    bytes.as_chunks::<4>().0.iter().map(|b| f32::from_le_bytes(*b)).collect()
}

/// A tensor `shape` over storage holding `values` at every `spacing`-th element, read through a
/// transposed walk when `transposed`: the same logical values in a very different memory layout.
pub(crate) fn laid_out(values: &[f32], rows: usize, cols: usize, spacing: usize, transposed: bool) -> Tensor {
    let mut storage = vec![f32::NAN; (rows * cols).max(1) * spacing];
    for row in 0..rows {
        for col in 0..cols {
            let at = if transposed { col * rows + row } else { row * cols + col };
            storage[at * spacing] = values[row * cols + col];
        }
    }
    let strides = if transposed { [spacing * 4, rows * spacing * 4] } else { [cols * spacing * 4, spacing * 4] };
    Tensor::from_storage(Arc::new(Vector(bytes_of(&storage))), Shape::Matrix(rows, cols), 0, strides).unwrap()
}
#[derive(Debug)]
pub(crate) struct Vector(pub(crate) Vec<u8>);
impl Storage for Vector {
    fn bytes(&self) -> &[u8] {
        &self.0
    }
}

#[test]
fn transpose_copies_bits_across_layouts_and_composes() {
    let budget = Budget::new(1 << 24);
    let domain = Domain::new(1024, 1024);
    for (rows, cols) in [(0, 3), (3, 0), (1, 1), (1, 7), (7, 1), (3, 5), (64, 33)] {
        let values: Vec<f32> =
            (0..rows * cols).map(|i| if i == 2 { f32::NAN } else { (i as f32 - 3.0) * -0.5 }).collect();
        let program =
            Expr::slot(0, Shape::Matrix(rows, cols), domain.clone()).unwrap().transpose().unwrap().compile().unwrap();
        assert_eq!(program.output_shape(), Shape::Matrix(cols, rows));
        for (spacing, transposed) in [(1, false), (2, false), (1, true), (3, true)] {
            let input = laid_out(&values, rows, cols, spacing, transposed);
            let result = floats(&program.run(&[&input], &budget).unwrap().readback().unwrap());
            for row in 0..rows {
                for col in 0..cols {
                    let (actual, expected) = (result[col * rows + row], values[row * cols + col]);
                    assert_eq!(actual.to_bits(), expected.to_bits(), "{rows}x{cols} ({row},{col})");
                }
            }
        }
    }
    // Twice is the identity, a product with its own transpose is symmetric, and neither needs more
    // than the planned temporaries.
    let values: Vec<f32> = (0..12).map(|i| i as f32 * 0.25 + 1.0).collect();
    let input = laid_out(&values, 3, 4, 1, false);
    let x = Expr::slot(0, Shape::Matrix(3, 4), domain).unwrap();
    let twice = x.transpose().unwrap().transpose().unwrap().compile().unwrap();
    assert_eq!(floats(&twice.run(&[&input], &budget).unwrap().readback().unwrap()), values);
    let gram = x.matmul(&x.transpose().unwrap()).unwrap().compile().unwrap();
    let product = floats(&gram.run(&[&input], &budget).unwrap().readback().unwrap());
    for i in 0..3 {
        for j in 0..3 {
            assert_eq!(product[i * 3 + j], product[j * 3 + i]);
        }
    }
    assert!(gram.native_source().is_err(), "lowering refuses what it cannot express");
    assert_eq!(budget.used(), 0);
}

/// Double-double arithmetic: an oracle with about 100 bits, independent of every kernel here.
mod oracle {
    #[derive(Clone, Copy, Debug)]
    pub(crate) struct Dd(pub(crate) f64, pub(crate) f64);
    fn two_sum(a: f64, b: f64) -> Dd {
        let s = a + b;
        let v = s - a;
        Dd(s, (a - (s - v)) + (b - v))
    }
    fn quick_two_sum(a: f64, b: f64) -> Dd {
        let s = a + b;
        Dd(s, b - (s - a))
    }
    fn two_prod(a: f64, b: f64) -> Dd {
        let p = a * b;
        Dd(p, a.mul_add(b, -p))
    }
    impl Dd {
        pub(crate) fn from(value: f64) -> Self {
            Self(value, 0.0)
        }
        pub(crate) fn add(self, o: Self) -> Self {
            let s = two_sum(self.0, o.0);
            let t = two_sum(self.1, o.1);
            let r = quick_two_sum(s.0, s.1 + t.0);
            quick_two_sum(r.0, r.1 + t.1)
        }
        pub(crate) fn neg(self) -> Self {
            Self(-self.0, -self.1)
        }
        pub(crate) fn sub(self, o: Self) -> Self {
            self.add(o.neg())
        }
        pub(crate) fn mul(self, o: Self) -> Self {
            let p = two_prod(self.0, o.0);
            quick_two_sum(p.0, p.1 + self.0 * o.1 + self.1 * o.0)
        }
        pub(crate) fn div(self, o: Self) -> Self {
            let q1 = self.0 / o.0;
            let r = self.sub(o.mul(Self::from(q1)));
            let q2 = r.0 / o.0;
            let r = r.sub(o.mul(Self::from(q2)));
            let q3 = r.0 / o.0;
            let q = quick_two_sum(q1, q2);
            q.add(Self::from(q3))
        }
        pub(crate) fn sqrt(self) -> Self {
            if self.0 == 0.0 {
                return Self::from(0.0);
            }
            let x = 1.0 / self.0.sqrt();
            let approximate = self.0 * x;
            Self::from(approximate)
                .add(self.sub(Self::from(approximate).mul(Self::from(approximate))).mul(Self::from(x * 0.5)))
        }
        pub(crate) fn value(self) -> f64 {
            self.0 + self.1
        }
    }
    /// ln(2) to double-double precision.
    const LN2: Dd = Dd(std::f64::consts::LN_2, 2.319_046_813_846_299_6e-17);

    /// `e^x`, by reduction and a Taylor series with exact double-double coefficients, for finite
    /// `x` in the f64 exponent range. Shares nothing with `exp_nonpositive` or the platform's.
    pub(crate) fn exp(x: Dd) -> Dd {
        #[allow(clippy::cast_possible_truncation)]
        let k = (x.0 / std::f64::consts::LN_2).round() as i32;
        let r = x.sub(LN2.mul(Dd::from(f64::from(k))));
        // A further reduction by 2^8 makes the series short and the squarings cheap.
        let scaled = r.mul(Dd::from(1.0 / 256.0));
        let mut term = Dd::from(1.0);
        let mut sum = Dd::from(1.0);
        for i in 1..=14 {
            term = term.mul(scaled).div(Dd::from(f64::from(i)));
            sum = sum.add(term);
        }
        for _ in 0..8 {
            sum = sum.mul(sum);
        }
        let scale = Dd::from(2.0f64.powi(k / 2)).mul(Dd::from(2.0f64.powi(k - k / 2)));
        sum.mul(scale)
    }
}
use oracle::{Dd, exp as dd_exp};

/// How many representable f32 values separate two numbers of any sign. NaN only matches NaN.
fn ulps32(a: f32, b: f32) -> u64 {
    if a.is_nan() || b.is_nan() {
        return if a.is_nan() && b.is_nan() { 0 } else { u64::MAX };
    }
    let key = |v: f32| {
        let bits = v.to_bits();
        if bits >> 31 == 0 { i64::from(bits) } else { -i64::from(bits & 0x7fff_ffff) }
    };
    key(a).abs_diff(key(b))
}
fn ulps64(a: f64, b: f64) -> u64 {
    let key = |v: f64| {
        let bits = v.to_bits();
        if bits >> 63 == 0 { i128::from(bits) } else { -i128::from(bits & 0x7fff_ffff_ffff_ffff) }
    };
    u64::try_from(key(a).abs_diff(key(b))).unwrap()
}

/// The exact softmax of a row of finite values, or of the row's special-value rule.
fn softmax_oracle(row: &[f32]) -> Vec<f32> {
    if row.is_empty() {
        return Vec::new();
    }
    if row.iter().any(|v| v.is_nan()) {
        return vec![f32::NAN; row.len()];
    }
    let infinities = row.iter().filter(|v| **v == f32::INFINITY).count();
    if infinities != 0 {
        return row.iter().map(|v| if *v == f32::INFINITY { 1.0 / infinities as f32 } else { 0.0 }).collect();
    }
    let max = row.iter().copied().fold(f32::NEG_INFINITY, f32::max);
    if max == f32::NEG_INFINITY {
        return vec![0.0; row.len()];
    }
    let exponents: Vec<Option<Dd>> = row
        .iter()
        .map(|v| {
            let difference = f64::from(*v) - f64::from(max);
            (difference > -700.0).then(|| dd_exp(Dd::from(difference)))
        })
        .collect();
    let total = exponents.iter().flatten().fold(Dd::from(0.0), |acc, e| acc.add(*e));
    #[allow(clippy::cast_possible_truncation)]
    exponents.iter().map(|e| e.map_or(0.0, |e| e.div(total).value() as f32)).collect()
}

/// The exact normalization of a row, by double-double statistics.
fn normalize_oracle(row: &[f32], epsilon: f64, center: bool) -> Vec<f32> {
    let n = Dd::from(row.len() as f64);
    let values: Vec<Dd> = row.iter().map(|v| Dd::from(f64::from(*v))).collect();
    let mean = if center { values.iter().fold(Dd::from(0.0), |a, v| a.add(*v)).div(n) } else { Dd::from(0.0) };
    let variance = values
        .iter()
        .fold(Dd::from(0.0), |a, v| {
            let d = v.sub(mean);
            a.add(d.mul(d))
        })
        .div(n);
    let divisor = variance.add(Dd::from(epsilon)).sqrt();
    #[allow(clippy::cast_possible_truncation)]
    values.iter().map(|v| v.sub(mean).div(divisor).value() as f32).collect()
}

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn unit(&mut self) -> f32 {
        (self.next() >> 40) as f32 / (1u64 << 24) as f32
    }
    /// A row drawn from one adversarial family, chosen by `kind`.
    fn row(&mut self, width: usize, kind: usize) -> Vec<f32> {
        (0..width)
            .map(|_| {
                let u = self.unit();
                match kind {
                    0 => u * 8.0 - 4.0,
                    1 => u * 600.0 - 300.0,
                    // Nearly constant: every pair differs in the last few bits.
                    2 => f32::from_bits(1.0f32.to_bits() + (self.next() & 7) as u32),
                    3 => (u - 0.5) * 2.0 * f32::MAX,
                    4 => f32::from_bits((self.next() & 0xff) as u32) * if u < 0.5 { 1.0 } else { -1.0 },
                    5 => {
                        if u < 0.5 {
                            0.0
                        } else {
                            -0.0
                        }
                    }
                    // A large common offset, where a premature f32 subtraction would lose bits.
                    6 => 1.0e6 + u * 16.0,
                    _ => {
                        // Around the exponential's cutoff.
                        -104.0 + u * 2.0 - if u < 0.1 { 0.0 } else { 1.0 }
                    }
                }
            })
            .collect()
    }
}

const WIDTHS: [usize; 18] = [1, 2, 3, 4, 5, 7, 8, 9, 15, 16, 17, 31, 32, 33, 63, 64, 65, 257];
const FAMILIES: usize = 8;

fn strict_isas() -> Vec<Isa> {
    [Isa::Baseline, Isa::Avx2, Isa::Fma].into_iter().filter(|isa| isa.available()).collect()
}

/// Runs `program` on `values` in every layout and instantiation, asserting all agree to the bit and
/// returning that result.
fn run_every_way(program: &Program, values: &[f32], rows: usize, cols: usize) -> Vec<f32> {
    let budget = Budget::new(1 << 26);
    let mut answer: Option<Vec<f32>> = None;
    for (spacing, transposed) in [(1, false), (2, false), (1, true), (3, true)] {
        let input = laid_out(values, rows, cols, spacing, transposed);
        for isa in strict_isas() {
            let mut out = vec![0x55u8; program.output_shape().elements().unwrap() * 4];
            program.run_with(&[input.borrow()], &mut out, &budget, isa, CHUNK).unwrap();
            let result = floats(&out);
            if let Some(answer) = &answer {
                for (a, b) in result.iter().zip(answer) {
                    assert!(a.to_bits() == b.to_bits() || (a.is_nan() && b.is_nan()), "{spacing} {transposed} {isa:?}");
                }
            } else {
                answer = Some(result);
            }
        }
    }
    assert_eq!(budget.used(), 0);
    answer.expect("at least one instantiation")
}

#[test]
fn deterministic_exponential_is_within_one_ulp_of_an_independent_oracle() {
    use crate::compute::reduction::{EXP_CUTOFF, exp_nonpositive};
    let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
    let mut worst = 0;
    let mut check = |x: f64| {
        let actual = exp_nonpositive(x);
        let expected = dd_exp(Dd::from(x)).value();
        let distance = ulps64(actual, expected);
        worst = worst.max(distance);
        assert!(distance <= 1, "{x}: {actual:e} vs {expected:e}, {distance} ulps");
        assert!(ulps64(actual, x.exp()) <= 3, "{x}: {actual:e} vs the platform's {:e}", x.exp());
    };
    for _ in 0..200_000 {
        check(-f64::from(rng.unit()) * 104.0);
    }
    // Every reduction boundary, and just either side of it.
    for k in 0..=150 {
        let boundary = -(f64::from(k) + 0.5) * std::f64::consts::LN_2;
        for x in [boundary, boundary + 1e-12, boundary - 1e-12] {
            if (EXP_CUTOFF..=0.0).contains(&x) {
                check(x);
            }
        }
    }
    for x in [EXP_CUTOFF, -1e-300, -f64::from(f32::from_bits(1)), -1.0, -0.5] {
        check(x);
    }
    assert_eq!(exp_nonpositive(0.0).to_bits(), 1.0f64.to_bits());
    assert_eq!(exp_nonpositive(-0.0).to_bits(), 1.0f64.to_bits());
    for x in [EXP_CUTOFF - 1e-9, -1000.0, -1e300, f64::NEG_INFINITY] {
        assert_eq!(exp_nonpositive(x).to_bits(), 0, "{x}");
    }
    // The cutoff's claim itself: below half the smallest subnormal, for any denominator of at least 1.
    let last = dd_exp(Dd::from(EXP_CUTOFF)).value();
    assert!(last < f64::from(f32::from_bits(1)) / 2.0, "{last:e}");
    println!("worst exponential distance: {worst} ulps");
}

#[test]
fn softmax_stays_within_one_f32_ulp_of_the_oracle_in_every_layout() {
    let domain = Domain::new(1 << 12, 1 << 12);
    let mut rng = Rng(0x0123_4567_89ab_cdef);
    let (mut elements, mut inexact) = (0usize, 0usize);
    for width in WIDTHS {
        for rows in [1, 3, 8] {
            for kind in 0..FAMILIES {
                let values: Vec<f32> = (0..rows).flat_map(|_| rng.row(width, kind)).collect();
                let shape = Shape::Matrix(rows, width);
                let program = Expr::slot(0, shape, domain.clone()).unwrap().softmax().unwrap().compile().unwrap();
                let actual = run_every_way(&program, &values, rows, width);
                for (row, (input, result)) in values.chunks(width).zip(actual.chunks(width)).enumerate() {
                    let expected = softmax_oracle(input);
                    for (column, (a, e)) in result.iter().zip(&expected).enumerate() {
                        let distance = ulps32(*a, *e);
                        assert!(distance <= 1, "width {width} kind {kind} ({row},{column}): {a:e} vs {e:e}");
                        elements += 1;
                        inexact += usize::from(distance != 0);
                    }
                    let total: f64 = result.iter().map(|v| f64::from(*v)).sum();
                    assert!((total - 1.0).abs() < width as f64 * 1e-7, "total {total}");
                    // Probability follows its logit.
                    for i in 1..width {
                        assert!(!(input[i - 1] < input[i] && result[i - 1] > result[i]), "{input:?}");
                    }
                }
            }
        }
    }
    // Almost every result is the correctly rounded one, not merely near it.
    assert!(inexact * 1000 < elements, "{inexact} of {elements} differ from the correctly rounded result");
}

#[test]
fn softmax_special_rows_share_a_matrix_without_contaminating_each_other() {
    let domain = Domain::new(1 << 12, 1 << 12);
    let (nan, inf, ninf, small) = (f32::NAN, f32::INFINITY, f32::NEG_INFINITY, f32::from_bits(1));
    let rows: [[f32; 5]; 8] = [
        [1.0, 2.0, 3.0, 4.0, 5.0],
        [nan, 2.0, 3.0, 4.0, 5.0],
        [1.0, 2.0, 3.0, 4.0, nan],
        [inf, 2.0, inf, 4.0, inf],
        [inf, nan, 1.0, ninf, 0.0],
        [ninf, ninf, ninf, ninf, ninf],
        [ninf, 1.0, ninf, ninf, ninf],
        [f32::MAX, -f32::MAX, small, -small, 0.0],
    ];
    let values: Vec<f32> = rows.iter().flatten().copied().collect();
    let shape = Shape::Matrix(8, 5);
    let program = Expr::slot(0, shape, domain).unwrap().softmax().unwrap().compile().unwrap();
    let actual = run_every_way(&program, &values, 8, 5);
    for (input, result) in rows.iter().zip(actual.chunks(5)) {
        for (a, e) in result.iter().zip(softmax_oracle(input)) {
            assert_eq!(ulps32(*a, e), 0, "{input:?}: {a:e} vs {e:e}");
        }
    }
}

#[test]
fn zero_strides_and_gaps_repeat_exactly_what_they_address() {
    let domain = Domain::new(1 << 12, 1 << 12);
    let budget = Budget::new(1 << 20);
    let base: Vec<f32> = (0..12).map(|i| i as f32 * 0.5 - 2.0).collect();
    let owner = Tensor::from_storage(Arc::new(Vector(bytes_of(&base))), Shape::Matrix(3, 4), 0, [16, 4]).unwrap();
    // Every row the same row; every column the same column; one element repeated everywhere.
    let cases: [(Shape, usize, [usize; 2], Vec<f32>); 3] = [
        (Shape::Matrix(5, 4), 16, [0, 4], (0..5).flat_map(|_| base[4..8].to_vec()).collect()),
        (Shape::Matrix(3, 6), 4, [16, 0], (0..3).flat_map(|r| [base[r * 4 + 1]; 6]).collect()),
        (Shape::Matrix(3, 3), 8, [0, 0], vec![base[2]; 9]),
    ];
    for (shape, offset, strides, logical) in cases {
        let view = Tensor::from_storage(owner.owner.clone(), shape, offset, strides).unwrap();
        let (rows, cols) = shape.dims();
        let x = Expr::slot(0, shape, domain.clone()).unwrap();
        for isa in strict_isas() {
            let run = |program: &Program| {
                let mut out = vec![0u8; program.output_shape().elements().unwrap() * 4];
                program.run_with(&[view.borrow()], &mut out, &budget, isa, CHUNK).unwrap();
                floats(&out)
            };
            let softmax = run(&x.softmax().unwrap().compile().unwrap());
            let normalized = run(&x.normalize(1e-5, true).unwrap().compile().unwrap());
            let sums = run(&x.sum_rows().unwrap().compile().unwrap());
            for (row, input) in logical.chunks(cols).enumerate() {
                let (a, b) = (&softmax[row * cols..][..cols], &normalized[row * cols..][..cols]);
                for (x, y) in a.iter().zip(softmax_oracle(input)) {
                    assert!(ulps32(*x, y) <= 1);
                }
                for (x, y) in b.iter().zip(normalize_oracle(input, 1e-5, true)) {
                    assert!(ulps32(*x, y) <= 1);
                }
                assert_eq!(sums[row], input.iter().fold(0.0f32, |s, v| s + v));
            }
            assert_eq!(rows, logical.len() / cols);
        }
    }
    assert_eq!(budget.used(), 0);
}

#[test]
fn normalization_stays_near_the_oracle_in_every_layout() {
    let domain = Domain::new(1 << 12, 1 << 12);
    let mut rng = Rng(0xfeed_face_cafe_beef);
    for width in WIDTHS {
        for rows in [1, 3, 8] {
            for kind in 0..FAMILIES {
                for (epsilon, center) in [(1e-5, true), (1e-5, false), (1e-300, true), (4.0, false), (1e30, true)] {
                    let values: Vec<f32> = (0..rows).flat_map(|_| rng.row(width, kind)).collect();
                    let shape = Shape::Matrix(rows, width);
                    let program = Expr::slot(0, shape, domain.clone())
                        .unwrap()
                        .normalize(epsilon, center)
                        .unwrap()
                        .compile()
                        .unwrap();
                    let actual = run_every_way(&program, &values, rows, width);
                    for (input, result) in values.chunks(width).zip(actual.chunks(width)) {
                        let expected = normalize_oracle(input, epsilon, center);
                        for (a, e) in result.iter().zip(&expected) {
                            // f64 statistics: within an f32 ulp, or within f64 cancellation error
                            // of the row's scale where the true mean nearly vanished.
                            let close = (f64::from(*a) - f64::from(*e)).abs() < 1e-9;
                            assert!(
                                ulps32(*a, *e) <= 1 || close,
                                "width {width} kind {kind} eps {epsilon} center {center}: {a:e} vs {e:e}"
                            );
                            assert!(a.is_finite(), "bounded output for finite input: {input:?}");
                        }
                    }
                }
            }
        }
    }
}

#[test]
fn normalization_special_cases_are_exact() {
    let domain = Domain::new(1 << 12, 1 << 12);
    let run = |values: &[f32], rows: usize, cols: usize, epsilon: f64, center: bool| {
        let shape = Shape::Matrix(rows, cols);
        let program =
            Expr::slot(0, shape, domain.clone()).unwrap().normalize(epsilon, center).unwrap().compile().unwrap();
        run_every_way(&program, values, rows, cols)
    };
    // Zero variance: a centered constant is exactly zero, positive; RMS of zeros keeps zero's sign.
    assert_eq!(run(&[7.5; 9], 1, 9, 1e-5, true).iter().map(|v| v.to_bits()).sum::<u32>(), 0);
    assert_eq!(
        run(&[0.0, -0.0, 0.0], 1, 3, 1e-5, false).iter().map(|v| v.to_bits()).collect::<Vec<_>>(),
        [0, (-0.0f32).to_bits(), 0]
    );
    // Without centering, the epsilon alone bounds a tiny row, and the denominator is exactly this.
    let tiny = run(&[1e-30, -1e-30], 1, 2, 1.0, false);
    assert_eq!(tiny, [1e-30, -1e-30]);
    // IEEE propagation of nonfinite inputs.
    let centered = run(&[1.0, f32::INFINITY, 2.0], 1, 3, 1e-5, true);
    assert!(centered.iter().all(|v| v.is_nan()));
    let rms = run(&[1.0, f32::INFINITY, -2.0], 1, 3, 1e-5, false);
    assert_eq!(rms[0].to_bits(), 0);
    assert!(rms[1].is_nan());
    assert_eq!(rms[2].to_bits(), (-0.0f32).to_bits());
    assert!(run(&[1.0, f32::NAN, 2.0], 1, 3, 1e-5, true).iter().all(|v| v.is_nan()));
    // A NaN row does not reach the next row.
    let two = run(&[f32::NAN, 1.0, 3.0, 1.0], 2, 2, 3.0, true);
    assert!(two[0].is_nan() && two[1].is_nan());
    assert_eq!(&two[2..], [0.5, -0.5]);
    // Rejected before any graph exists.
    let x = Expr::slot(0, Shape::Matrix(1, 1), domain.clone()).unwrap();
    for epsilon in [0.0, -0.0, -1.0, f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
        assert!(x.normalize(epsilon, true).is_err(), "{epsilon}");
    }
    assert!(x.normalize(f64::MIN_POSITIVE / 8.0, true).is_ok(), "a subnormal epsilon is positive");
}

#[test]
fn row_sums_are_ordered_across_group_boundaries_and_layouts() {
    let domain = Domain::new(1 << 12, 1 << 12);
    let mut rng = Rng(0x7777_1234_dead_0001);
    for rows in [1, 2, 7, 8, 9, 15, 16, 17, 64] {
        for width in [0, 1, 2, 3, 8, 33, 129] {
            for kind in 0..FAMILIES {
                let values: Vec<f32> = (0..rows).flat_map(|_| rng.row(width, kind)).collect();
                let shape = Shape::Matrix(rows, width);
                let program = Expr::slot(0, shape, domain.clone()).unwrap().sum_rows().unwrap().compile().unwrap();
                let actual = run_every_way(&program, &values, rows, width);
                for row in 0..rows {
                    let expected = (0..width).fold(0.0f32, |sum, col| sum + values[row * width + col]);
                    assert_eq!(actual[row].to_bits(), expected.to_bits(), "{rows}x{width} kind {kind} row {row}");
                }
            }
        }
    }
    // Order is observable: these only cancel when added left to right.
    let (big, one) = (16_777_216.0f32, 1.0f32);
    let values = [big, one, one, -big, -big, one, one, big];
    let program = Expr::slot(0, Shape::Matrix(2, 4), domain).unwrap().sum_rows().unwrap().compile().unwrap();
    assert_eq!(run_every_way(&program, &values, 2, 4), [0.0, 2.0]);
}

#[test]
fn zero_budget_runs_what_needs_no_scratch_and_refuses_what_does() {
    let domain = Domain::new(1 << 12, 1 << 12);
    let values: Vec<f32> = (0..64).map(|i| i as f32 * 0.25).collect();
    let shape = Shape::Matrix(8, 8);
    let x = Expr::slot(0, shape, domain).unwrap();
    let (contiguous, spread) = (laid_out(&values, 8, 8, 1, false), laid_out(&values, 8, 8, 2, false));
    let denied = Budget::new(0);
    let mut out = vec![0x55u8; 64 * 4];
    for center in [true, false] {
        let program = x.normalize(1e-5, center).unwrap().compile().unwrap();
        program.run_into(&[contiguous.borrow()], &mut out, &denied).unwrap();
        assert!(program.run_into(&[spread.borrow()], &mut out, &denied).is_err(), "a spread row must be copied");
    }
    let sums = x.sum_rows().unwrap().compile().unwrap();
    for input in [&contiguous, &spread] {
        sums.run_into(&[input.borrow()], &mut out[..32], &denied).unwrap();
    }
    assert_eq!(denied.peak(), 0);
    // Softmax's scratch is exactly what it charges, and one byte less is refused before any write.
    let softmax = x.softmax().unwrap().compile().unwrap();
    let needed = crate::compute::reduction::softmax_scratch(8) * 8;
    let mut untouched = vec![0x55u8; 64 * 4];
    assert!(softmax.run_into(&[contiguous.borrow()], &mut untouched, &Budget::new(needed - 1)).is_err());
    assert!(untouched.iter().all(|byte| *byte == 0x55));
    let enough = Budget::new(needed);
    softmax.run_into(&[contiguous.borrow()], &mut untouched, &enough).unwrap();
    assert_eq!((enough.peak(), enough.used()), (needed, 0));
    let spread_needed = needed + 8 * 4;
    assert!(softmax.run_into(&[spread.borrow()], &mut untouched, &Budget::new(spread_needed - 1)).is_err());
    softmax.run_into(&[spread.borrow()], &mut untouched, &Budget::new(spread_needed)).unwrap();
}

#[test]
fn a_nan_of_either_sign_and_any_payload_poisons_softmax() {
    let domain = Domain::new(16, 16);
    let program = Expr::slot(0, Shape::Matrix(1, 5), domain).unwrap().softmax().unwrap().compile().unwrap();
    for bits in [0x7fc0_0000u32, 0xffc0_0000, 0x7f80_0001, 0xff80_0001, 0x7fff_ffff, 0xffff_ffff] {
        let values = [1.0, f32::from_bits(bits), 3.0, 4.0, 5.0];
        assert!(run_every_way(&program, &values, 1, 5).iter().all(|v| v.is_nan()), "{bits:#x}");
    }
}

#[test]
fn softmax_scratch_pads_to_whole_vectors() {
    use crate::compute::reduction::softmax_scratch;
    assert_eq!([0, 1, 4, 5, 7, 8, 9].map(softmax_scratch), [0, 4, 4, 8, 8, 8, 12]);
}

/// Rows a whole number of bytes apart that is not a whole number of elements still address exactly
/// the bytes they say. Sums must not treat them as evenly spaced elements.
#[test]
fn row_strides_that_split_elements_read_the_addressed_bytes() {
    let domain = Domain::new(64, 64);
    let budget = Budget::new(1 << 16);
    let bytes: Vec<u8> = (0..64u8).map(|i| i.wrapping_mul(37) | 0x40).collect();
    let tensor = Tensor::from_storage(Arc::new(Vector(bytes.clone())), Shape::Matrix(5, 3), 1, [6, 4]).unwrap();
    let element = |row: usize, col: usize| {
        let at = 1 + row * 6 + col * 4;
        f32::from_le_bytes(bytes[at..at + 4].try_into().unwrap())
    };
    let x = Expr::slot(0, Shape::Matrix(5, 3), domain).unwrap();
    let sums = floats(&x.sum_rows().unwrap().compile().unwrap().run(&[&tensor], &budget).unwrap().readback().unwrap());
    let normalized = floats(
        &x.normalize(1e-5, true).unwrap().compile().unwrap().run(&[&tensor], &budget).unwrap().readback().unwrap(),
    );
    for row in 0..5 {
        let logical = [element(row, 0), element(row, 1), element(row, 2)];
        assert_eq!(sums[row].to_bits(), (0..3).fold(0.0f32, |s, c| s + logical[c]).to_bits(), "{row}");
        for (a, e) in normalized[row * 3..][..3].iter().zip(normalize_oracle(&logical, 1e-5, true)) {
            assert!(ulps32(*a, e) <= 1, "{row}: {a:e} vs {e:e}");
        }
    }
}

/// Scratch stays charged for as long as it lives. A guard dropped early would still pass every
/// single-threaded check, so this makes concurrent runs race for exactly one stage's worth.
#[test]
fn row_scratch_stays_charged_while_other_threads_contend_for_the_budget() {
    let width = 1 << 16;
    let domain = Domain::new(64, 64);
    let values: Vec<f32> = (0..width).map(|i| (i % 97) as f32 * 0.25).collect();
    let input = laid_out(&values, 1, width, 1, false);
    let program = Expr::slot(0, Shape::Matrix(1, width), domain).unwrap().softmax().unwrap().compile().unwrap();
    let budget = Budget::new(crate::compute::reduction::softmax_scratch(width) * 8);
    let barrier = std::sync::Barrier::new(4);
    std::thread::scope(|scope| {
        for _ in 0..4 {
            scope.spawn(|| {
                let mut out = vec![0u8; width * 4];
                barrier.wait();
                for _ in 0..200 {
                    // Either it fails cleanly, or it succeeded alone with the whole budget.
                    let _ = program.run_into(&[input.borrow()], &mut out, &budget);
                }
            });
        }
    });
    assert_eq!(budget.used(), 0);
    assert!(budget.denials() > 0, "four threads never overlapped on one stage's budget");
    assert!(budget.peak() <= crate::compute::reduction::softmax_scratch(width) * 8);
}

fn ordered_product(a: &[f32], b: &[f32], m: usize, k: usize, n: usize, fused: bool) -> Vec<f32> {
    let mut out = vec![0.0f32; m * n];
    for i in 0..m {
        for j in 0..n {
            let mut sum = 0.0f32;
            for p in 0..k {
                let (x, y) = (a[i * k + p], b[p * n + j]);
                sum = if fused { x.mul_add(y, sum) } else { sum + x * y };
            }
            out[i * n + j] = sum;
        }
    }
    out
}

/// Every boundary the kernels branch on, in each dimension: row groups, narrow and wide column
/// blocks, packing passes and column passes. Results must be the ordered triple loop's, to the bit.
#[test]
fn matmul_every_kernel_boundary_matches_the_ordered_triple_loop() {
    let domain = Domain::new(1 << 14, 1 << 14);
    let mut rng = Rng(0xabcd_ef01_2345_6789);
    let sign = |rng: &mut Rng| {
        let u = rng.unit();
        (u * 2.0 - 1.0) * 1.000_001
    };
    let shapes = [1usize, 2, 3, 4, 5, 6, 7, 11, 12, 13]
        .into_iter()
        .flat_map(|m| [1usize, 2, 3, 4, 5, 127, 128, 129].map(|k| (m, k)))
        .flat_map(|(m, k)| [2usize, 3, 7, 8, 9, 15, 16, 17, 24, 25, 31, 32, 33].map(|n| (m, k, n)))
        .chain([
            (7, 3, 255),
            (7, 3, 256),
            (7, 3, 257),
            (7, 130, 272),
            (7, 300, 513),
            (2, 3, 513),
            (1, 4097, 3),
            (1, 5, 257),
        ]);
    let budget = Budget::new(1 << 24);
    for (m, k, n) in shapes {
        let a: Vec<f32> = (0..m * k).map(|_| sign(&mut rng)).collect();
        let b: Vec<f32> = (0..k * n).map(|_| sign(&mut rng)).collect();
        let (ta, tb) = (laid_out(&a, m, k, 1, false), laid_out(&b, k, n, 1, false));
        let x = Expr::slot(0, ta.shape(), domain.clone()).unwrap();
        let y = Expr::slot(1, tb.shape(), domain.clone()).unwrap();
        let strict = ordered_product(&a, &b, m, k, n, false);
        let program = x.matmul(&y).unwrap().compile().unwrap();
        for isa in strict_isas() {
            let mut out = vec![0x55u8; m * n * 4];
            program.run_with(&[ta.borrow(), tb.borrow()], &mut out, &budget, isa, CHUNK).unwrap();
            let actual = floats(&out);
            for (i, (p, q)) in actual.iter().zip(&strict).enumerate() {
                assert_eq!(p.to_bits(), q.to_bits(), "{m}x{k}x{n} {isa:?} element {i}");
            }
        }
        if Isa::Fma.available() {
            let fused = ordered_product(&a, &b, m, k, n, true);
            let program = x.matmul(&y).unwrap().compile_with(Numeric::Fast).unwrap();
            let mut out = vec![0x55u8; m * n * 4];
            program.run_with(&[ta.borrow(), tb.borrow()], &mut out, &budget, Isa::Fma, CHUNK).unwrap();
            for (i, (p, q)) in floats(&out).iter().zip(&fused).enumerate() {
                assert_eq!(p.to_bits(), q.to_bits(), "fused {m}x{k}x{n} element {i}");
            }
        }
    }
    assert_eq!(budget.used(), 0);
}

/// Padding lanes hold zeros that meet whatever `a` holds, so a NaN or infinity in `a` makes NaN in
/// a discarded lane. It must stay discarded, and a stale output element must not survive.
#[test]
fn matmul_remainder_padding_never_reaches_a_real_element() {
    let domain = Domain::new(1 << 12, 1 << 12);
    let budget = Budget::new(1 << 20);
    for (m, k, n) in [(7, 5, 9), (7, 5, 3), (13, 3, 25), (2, 3, 3), (6, 129, 17), (3, 4, 2)] {
        let a = vec![f32::INFINITY; m * k];
        let b = vec![0.0f32; k * n];
        let finite = vec![1.0f32; m * k];
        let weights: Vec<f32> = (0..k * n).map(|i| i as f32).collect();
        let (ta, tb) = (laid_out(&a, m, k, 1, false), laid_out(&b, k, n, 1, false));
        let (fa, fb) = (laid_out(&finite, m, k, 1, false), laid_out(&weights, k, n, 1, false));
        let x = Expr::slot(0, ta.shape(), domain.clone()).unwrap();
        let p = x.matmul(&Expr::slot(1, tb.shape(), domain.clone()).unwrap()).unwrap().compile().unwrap();
        let mut out = vec![0x55u8; m * n * 4];
        // An infinity times a real zero is a real NaN.
        p.run_into(&[ta.borrow(), tb.borrow()], &mut out, &budget).unwrap();
        assert!(floats(&out).iter().all(|v| v.is_nan()), "{m}x{k}x{n}");
        // Infinite scales against nonzero weights never see the padding's zeros as output.
        let mut ones = vec![0x55u8; m * n * 4];
        p.run_into(&[fa.borrow(), fb.borrow()], &mut ones, &budget).unwrap();
        assert!(floats(&ones).iter().all(|v| v.is_finite()), "{m}x{k}x{n}");
    }
}

/// The elements of a strided parent are the only ones its views may reach. Byte intervals would allow
/// every rejection below.
#[test]
fn views_are_element_capabilities_not_byte_intervals() {
    let base: Vec<f32> = (0..24).map(|i| i as f32).collect();
    let owner: Arc<dyn Storage> = Arc::new(Vector(bytes_of(&base)));
    // Every second element of a 2x12 block: rows 48 bytes apart, columns 8 apart, 6 columns.
    let gapped = Tensor::from_storage(owner.clone(), Shape::Matrix(2, 6), 0, [48, 8]).unwrap();
    let matrix = |r, c| Shape::Matrix(r, c);

    // Inside the parent's elements: subsets, reorderings, and repeats of them.
    for (shape, offset, strides, expected) in [
        (matrix(1, 3), 8, [0, 8], vec![2.0, 4.0, 6.0]),
        (matrix(2, 2), 0, [48, 16], vec![0.0, 4.0, 12.0, 16.0]),
        (matrix(3, 2), 0, [8, 48], vec![0.0, 12.0, 2.0, 14.0, 4.0, 16.0]),
        (matrix(2, 2), 8, [0, 0], vec![2.0; 4]),
        (matrix(4, 1), 48, [8, 0], vec![12.0, 14.0, 16.0, 18.0]),
    ] {
        let view = gapped.view(shape, offset, strides).unwrap();
        assert_eq!(floats(&view.readback().unwrap()), expected, "{shape:?} {offset} {strides:?}");
    }
    // Refused though every byte is inside the parent's interval: gaps, split elements, widening.
    for (shape, offset, strides, what) in [
        (matrix(1, 3), 0, [0, 4], "adjacent elements span a gap"),
        (matrix(1, 1), 4, [0, 0], "a single element in a gap"),
        (matrix(1, 1), 2, [0, 0], "bytes across two elements"),
        (matrix(1, 1), 48 + 4, [0, 0], "a gap in the second row"),
        (matrix(2, 1), 0, [44, 0], "a row stride landing in a gap"),
        (matrix(1, 2), 88, [0, 8], "running past the last element"),
        (matrix(2, 1), 40, [4, 0], "a row stride from the last element of a row into a gap"),
    ] {
        assert!(gapped.view(shape, offset, strides).is_err(), "{what}");
    }

    // A child can never recover what its own parent gave up.
    let row = gapped.view(matrix(1, 3), 8, [0, 8]).unwrap();
    assert!(row.view(matrix(1, 1), 24, [0, 0]).is_err(), "past the row");
    assert!(row.view(matrix(1, 2), 0, [0, 4]).is_err(), "into a gap");
    assert_eq!(floats(&row.view(matrix(1, 1), 16, [0, 0]).unwrap().readback().unwrap()), [6.0]);

    // A contiguous parent: any whole-element walk inside it, but not across its elements.
    let whole = Tensor::from_storage(owner.clone(), matrix(4, 6), 0, [24, 4]).unwrap();
    assert!(whole.view(matrix(2, 6), 24, [24, 4]).is_ok());
    assert!(whole.view(matrix(6, 4), 0, [4, 24]).is_ok(), "transposed");
    assert!(whole.view(matrix(1, 1), 1, [0, 0]).is_err(), "unaligned");
    assert!(whole.view(matrix(2, 2), 4, [6, 4]).is_err(), "row stride splits elements");
    assert!(whole.view(matrix(1, 2), 92, [0, 4]).is_err(), "past the end");

    // A repeated row addresses only that row.
    let repeated = Tensor::from_storage(owner.clone(), matrix(5, 3), 0, [0, 4]).unwrap();
    assert!(repeated.view(matrix(2, 3), 0, [0, 4]).is_ok());
    assert!(repeated.view(matrix(1, 1), 12, [0, 0]).is_err(), "the next row was never granted");
    // A transposed parent's elements, addressed along its own walk.
    let transposed = Tensor::from_storage(owner.clone(), matrix(3, 2), 0, [8, 48]).unwrap();
    assert!(transposed.view(matrix(1, 2), 8, [0, 48]).is_ok());
    assert!(transposed.view(matrix(1, 2), 4, [0, 48]).is_err());

    // Steps that collide have no one reading of an address: only the parent itself re-derives.
    let colliding = Tensor::from_storage(owner.clone(), matrix(3, 3), 0, [8, 4]).unwrap();
    assert!(colliding.view(matrix(3, 3), 0, [8, 4]).is_ok());
    assert!(colliding.view(matrix(1, 1), 0, [0, 0]).is_err());
    let overlapping = Tensor::from_storage(owner.clone(), matrix(1, 3), 0, [0, 2]).unwrap();
    assert!(overlapping.view(matrix(1, 1), 2, [0, 0]).is_err());

    // An empty view grants nothing, wherever it says it is, as long as it stays in bounds.
    assert!(gapped.view(matrix(0, 3), 4, [0, 4]).is_ok());
    let empty = gapped.view(matrix(0, 3), 4, [0, 4]).unwrap();
    assert!(empty.view(matrix(1, 1), 0, [0, 0]).is_err(), "an empty parent has no elements");
    // The host's entry stays free to describe any bytes: it is the trusted root.
    assert!(Tensor::from_storage(owner, matrix(1, 1), 2, [0, 0]).is_ok());
}

fn prepared_shapes() -> Vec<(usize, usize, usize)> {
    let mut shapes = vec![(7, 3, 2), (7, 129, 17), (13, 300, 33), (7, 130, 257), (64, 128, 128), (9, 5, 513)];
    for m in [2, 3, 8, 15, 16, 17, 31, 32, 33, 40, 48, 49] {
        for k in [1, 2, 127, 128, 129, 300] {
            shapes.push((m, k, 1));
        }
    }
    for m in [2, 6, 7, 12, 13] {
        for n in [2, 8, 9, 16, 17, 25] {
            shapes.push((m, 129, n));
        }
    }
    shapes
}

/// Packed weights are an optimization and nothing else: the same bits, in both modes, on every
/// instantiation and from any layout of the weights.
#[test]
fn prepared_weights_change_no_bit_in_any_mode_or_layout() {
    let domain = Domain::new(1 << 14, 1 << 14);
    let mut rng = Rng(0x1357_9bdf_0246_8ace);
    let mut value = move || (rng.unit() * 2.0 - 1.0) * 1.000_001;
    let budget = Budget::new(1 << 26);
    let isas: Vec<Isa> = strict_isas();
    for (m, k, n) in prepared_shapes() {
        let a: Vec<f32> = (0..m * k).map(|_| value()).collect();
        let b: Vec<f32> = (0..k * n).map(|_| value()).collect();
        let (x, y) = (
            Expr::slot(0, Shape::Matrix(m, k), domain.clone()).unwrap(),
            Expr::slot(1, Shape::Matrix(k, n), domain.clone()).unwrap(),
        );
        let (strict, fused) = (ordered_product(&a, &b, m, k, n, false), ordered_product(&a, &b, m, k, n, true));
        for (spacing, transposed) in [(1, false), (2, true)] {
            let (ta, tb) = (laid_out(&a, m, k, spacing, transposed), laid_out(&b, k, n, spacing, transposed));
            let (pa, pb) = if n == 1 {
                (ta.prepare_left(&budget).unwrap(), tb.clone())
            } else {
                (ta.clone(), tb.prepare_right(&budget).unwrap())
            };
            assert!(pa.prepared_bytes() + pb.prepared_bytes() > 0);
            assert!(ta.shares_storage(&pa) && tb.shares_storage(&pb), "no copy of the elements");
            let views = [pa.borrow(), pb.borrow()];
            let program = x.matmul(&y).unwrap().compile().unwrap();
            for isa in &isas {
                let denied = Budget::new(if spacing == 1 { 0 } else { (m * k + k * n) * 4 });
                let mut out = vec![0x55u8; m * n * 4];
                program.run_with(&views, &mut out, &denied, *isa, CHUNK).unwrap();
                for (i, (p, q)) in floats(&out).iter().zip(&strict).enumerate() {
                    assert_eq!(p.to_bits(), q.to_bits(), "{m}x{k}x{n} {spacing} {isa:?} element {i}");
                }
            }
            if Isa::Fma.available() {
                let program = x.matmul(&y).unwrap().compile_with(Numeric::Fast).unwrap();
                let mut out = vec![0x55u8; m * n * 4];
                program.run_with(&views, &mut out, &budget, Isa::Fma, CHUNK).unwrap();
                for (i, (p, q)) in floats(&out).iter().zip(&fused).enumerate() {
                    assert_eq!(p.to_bits(), q.to_bits(), "fused {m}x{k}x{n} {spacing} element {i}");
                }
            }
        }
    }
    assert_eq!(budget.used(), 0, "the last tensor sharing a packing releases its charge");
}

#[test]
fn prepared_weights_are_charged_while_they_live_and_refused_past_the_budget() {
    let domain = Domain::new(64, 64);
    let setup = Budget::new(1 << 24);
    let (m, k, n) = (16, 300, 20);
    let bytes: Vec<u8> = (0..k * n).flat_map(|i| (i as f32).to_le_bytes()).collect();
    let weights = Tensor::import(&bytes, Shape::Matrix(k, n), &setup).unwrap();
    let left = Tensor::import(&vec![1u8; m * k * 4], Shape::Matrix(m, k), &setup).unwrap();
    let held = setup.used();

    // The exact size is refused one byte short, cleanly, and the charge is exact when it fits.
    let size = crate::compute::kernels::prepared_right_elements(k, n) * 4;
    let tight = Budget::new(size - 1);
    assert!(weights.prepare_right(&tight).is_err());
    assert_eq!(tight.used(), 0);
    let budget = Budget::new(size);
    let prepared = weights.prepare_right(&budget).unwrap();
    assert_eq!((budget.used(), prepared.prepared_bytes()), (size, size));

    // Packing's scratch is what preparation saves: the product needs no budget beyond it, while
    // the same product without it does. Derived views give the packing up and pay again.
    let program = Expr::slot(0, left.shape(), domain.clone())
        .unwrap()
        .matmul(&Expr::slot(1, weights.shape(), domain).unwrap())
        .unwrap()
        .compile()
        .unwrap();
    let mut out = vec![0u8; m * n * 4];
    program.run_into(&[left.borrow(), prepared.borrow()], &mut out, &Budget::new(0)).unwrap();
    assert!(program.run_into(&[left.borrow(), weights.borrow()], &mut out, &Budget::new(0)).is_err());
    let derived = prepared.view(prepared.shape(), 0, [n * 4, 4]).unwrap();
    assert_eq!(derived.prepared_bytes(), 0);
    assert!(program.run_into(&[left.borrow(), derived.borrow()], &mut out, &Budget::new(0)).is_err());

    // A clone shares the packing; it stays charged until the last holder drops, and no longer.
    let clone = prepared.clone();
    drop(prepared);
    assert_eq!(budget.used(), size);
    program.run_into(&[left.borrow(), clone.borrow()], &mut out, &Budget::new(0)).unwrap();
    drop(clone);
    drop(derived);
    assert_eq!(budget.used(), 0);

    // A packing for the other side, or a product with a shape it doesn't serve, costs what a plain
    // operand costs and gives the same bits.
    let wrong_side = weights.prepare_left(&setup).unwrap();
    let mut plain = vec![0u8; m * n * 4];
    program.run_into(&[left.borrow(), weights.borrow()], &mut plain, &setup).unwrap();
    program.run_into(&[left.borrow(), wrong_side.borrow()], &mut out, &setup).unwrap();
    assert_eq!(out, plain);
    drop(wrong_side);
    assert_eq!(setup.used(), held);
}

/// One program and one packing shared by threads, under one budget, with every result checked.
#[test]
fn prepared_weights_serve_concurrent_runs() {
    let domain = Domain::new(64, 64);
    let budget = Budget::new(1 << 24);
    let (m, k) = (48, 129);
    let a: Vec<f32> = (0..m * k).map(|i| (i % 19) as f32 * 0.25 - 2.0).collect();
    let b: Vec<f32> = (0..k).map(|i| (i % 7) as f32 * 0.5 - 1.5).collect();
    let (ta, tb) = (laid_out(&a, m, k, 1, false), laid_out(&b, k, 1, 1, false));
    let prepared = ta.prepare_left(&budget).unwrap();
    let program = Expr::slot(0, ta.shape(), domain.clone())
        .unwrap()
        .matmul(&Expr::slot(1, tb.shape(), domain).unwrap())
        .unwrap()
        .compile()
        .unwrap();
    let expected = ordered_product(&a, &b, m, k, 1, false);
    std::thread::scope(|scope| {
        for _ in 0..4 {
            scope.spawn(|| {
                for _ in 0..200 {
                    let mut out = vec![0x55u8; m * 4];
                    program.run_into(&[prepared.borrow(), tb.borrow()], &mut out, &budget).unwrap();
                    assert_eq!(floats(&out), expected);
                }
            });
        }
    });
    drop(prepared);
    assert_eq!(budget.used(), 0);
}
