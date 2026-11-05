//! The engine's promises under deliberate failure and at memory's edges: `Err` leaves caller storage
//! alone, charges cover what they account for, kernels stay inside what they were given, and a Fast
//! dot product honours its error bound against exact arithmetic.
#![allow(clippy::many_single_char_names, clippy::cast_precision_loss, clippy::cast_possible_wrap)]
use super::adversarial::{Vector, bytes_of, floats, laid_out};
use crate::compute::kernels::Inline;
use crate::compute::*;
use std::sync::Arc;

/// Every shape of failure a program can meet needs one of these: elementwise with walked sources,
/// each row stage, a product packing `b`, a strided product packing its operands, and several stages.
fn programs() -> Vec<(&'static str, Program, Vec<Tensor>)> {
    let domain = Domain::new(1 << 12, 1 << 12);
    let values: Vec<f32> = (0..144).map(|i| (i % 23) as f32 * 0.25 - 2.0).collect();
    let slot = |id, rows, cols| Expr::slot(id, Shape::Matrix(rows, cols), domain.clone()).unwrap();
    let square = slot(0, 12, 12);
    let walked = laid_out(&values, 12, 12, 2, true);
    let plain = laid_out(&values, 12, 12, 1, false);
    vec![
        (
            "walked elementwise",
            square.add(&slot(1, 12, 12)).unwrap().relu().unwrap().compile().unwrap(),
            vec![walked.clone(), plain.clone()],
        ),
        ("softmax with spread columns", square.softmax().unwrap().compile().unwrap(), vec![walked.clone()]),
        (
            "normalize with spread columns",
            square.normalize(1e-5, true).unwrap().compile().unwrap(),
            vec![walked.clone()],
        ),
        (
            "packed product",
            square.matmul(&slot(1, 12, 12)).unwrap().compile().unwrap(),
            vec![plain.clone(), plain.clone()],
        ),
        (
            "strided product",
            square.matmul(&slot(1, 12, 12)).unwrap().compile().unwrap(),
            vec![walked.clone(), walked.clone()],
        ),
        (
            "stages",
            square
                .matmul(&slot(1, 12, 12))
                .unwrap()
                .add(&square)
                .unwrap()
                .transpose()
                .unwrap()
                .softmax()
                .unwrap()
                .compile_with(Numeric::Fast)
                .unwrap(),
            vec![walked, plain],
        ),
    ]
}

/// A budget too small at any size refuses, with the caller's output untouched and everything returned.
#[test]
fn a_refused_budget_leaves_the_output_untouched_at_every_limit() {
    for (name, program, inputs) in programs() {
        let views: Vec<View<'_>> = inputs.iter().map(Tensor::borrow).collect();
        let size = program.output_shape().elements().unwrap() * 4;
        let (mut want, roomy) = (vec![0u8; size], Budget::new(1 << 24));
        program.run_into(&views, &mut want, &roomy).unwrap();
        assert_eq!(roomy.used(), 0, "{name}");
        let peak = roomy.peak();
        assert!(peak > 0, "{name} never asks for anything, so this test proves nothing");
        for limit in (0..=peak).step_by(4).chain([peak]) {
            let budget = Budget::new(limit);
            let mut out = vec![0xa5u8; size];
            match program.run_into(&views, &mut out, &budget) {
                Ok(()) => assert!(out == want, "{name} at {limit} of {peak}: a wrong result"),
                Err(_) => {
                    assert!(out.iter().all(|byte| *byte == 0xa5), "{name} at {limit} of {peak}: wrote before failing");
                    assert!(budget.denials() > 0, "{name} at {limit} of {peak}");
                }
            }
            assert_eq!(budget.used(), 0, "{name} at {limit} of {peak}: kept a charge");
        }
        let mut out = vec![0xa5u8; size];
        assert!(program.run_into(&views, &mut out, &Budget::new(peak)).is_ok(), "{name} needs more than its peak");
        assert!(out == want, "{name}");
    }
}

/// The allocator itself refusing the nth request behaves the same way, for every n.
#[test]
fn a_refused_allocation_leaves_the_output_untouched_wherever_it_happens() {
    for (name, program, inputs) in programs() {
        let views: Vec<View<'_>> = inputs.iter().map(Tensor::borrow).collect();
        let size = program.output_shape().elements().unwrap() * 4;
        let mut want = vec![0u8; size];
        program.run_into(&views, &mut want, &Budget::new(1 << 24)).unwrap();
        let mut failures = 0;
        for fail_at in 0.. {
            let budget = Budget::new(1 << 24);
            let mut out = vec![0xa5u8; size];
            let mut seen = 0;
            let result = with_allocation_hook(
                |_| {
                    seen += 1;
                    seen - 1 != fail_at
                },
                || program.run_into(&views, &mut out, &budget),
            );
            assert_eq!(budget.used(), 0, "{name}, request {fail_at}: kept a charge");
            if result.is_ok() {
                assert!(out == want, "{name}");
                break;
            }
            failures += 1;
            assert!(out.iter().all(|byte| *byte == 0xa5), "{name}, request {fail_at}: wrote before failing");
        }
        assert!(failures > 0, "{name} allocated nothing, so this test proves nothing");
    }
}

/// `run` keeps nothing of a failed attempt either, and a later attempt starts clean.
#[test]
fn a_failed_run_returns_every_charge_and_the_next_one_works() {
    for (name, program, inputs) in programs() {
        let refs: Vec<&Tensor> = inputs.iter().collect();
        let budget = Budget::new(1 << 24);
        let want = program.run(&refs, &budget).unwrap().readback().unwrap();
        let after_first = budget.used();
        assert_eq!(after_first, 0, "{name}");
        let mut seen = 0;
        let refused = with_allocation_hook(
            |_| {
                seen += 1;
                false
            },
            || program.run(&refs, &budget),
        );
        assert!(refused.is_err(), "{name}");
        assert_eq!(budget.used(), 0, "{name}");
        assert_eq!(program.run(&refs, &budget).unwrap().readback().unwrap(), want, "{name}");
    }
}

/// Every piece of scratch and temporary memory is under its charge by the time it exists.
#[test]
fn every_allocation_during_a_run_is_already_charged() {
    for (name, program, inputs) in programs() {
        let views: Vec<View<'_>> = inputs.iter().map(Tensor::borrow).collect();
        let mut out = vec![0u8; program.output_shape().elements().unwrap() * 4];
        let budget = Budget::new(1 << 24);
        let (mut requests, mut largest) = (0, 0usize);
        with_allocation_hook(
            |bytes| {
                let used = budget.used();
                assert!(used >= bytes, "{name}: {bytes} bytes allocated with {used} charged");
                requests += usize::from(bytes > 0);
                largest = largest.max(bytes);
                true
            },
            || program.run_into(&views, &mut out, &budget),
        )
        .unwrap();
        assert!(requests > 0, "{name} allocated nothing, so this test proves nothing");
        assert!(largest <= budget.peak(), "{name}");
        assert_eq!(budget.used(), 0, "{name}");
    }
}

/// What no host policy can permit: sizes this crate's arithmetic couldn't represent are refused
/// before anything is allocated, and a huge graph that compiles costs nothing until it is run.
#[test]
fn no_policy_lifts_what_the_arithmetic_can_represent() {
    let anything = Limits { elements: usize::MAX, work: usize::MAX };
    let domain = Domain::with_limits(64, 8, anything);
    let huge = |rows, cols| Expr::slot(0, Shape::Matrix(rows, cols), domain.clone());
    assert!(Shape::Matrix(usize::MAX, 2).elements().is_err());
    assert!(Shape::Matrix(1 << 40, 1 << 40).elements().is_err(), "the product overflows");
    assert!(Shape::Matrix(usize::MAX, 0).elements().is_err(), "a zero doesn't excuse the other side");
    assert!(huge(1 << 58, 1).is_err(), "past what bytes can address");
    assert!(huge(1 << 30, 1 << 30).is_err());
    let budget = Budget::with_elements(1 << 20, usize::MAX);
    assert!(Tensor::import(&[], Shape::Matrix(1 << 58, 1), &budget).is_err());
    // Work that overflows when counted.
    let a = huge(1 << 28, 1 << 28).unwrap();
    let b = huge(1 << 28, 1 << 28).unwrap();
    assert!(a.matmul(&b).is_err());
    // Work the count can hold: compiling allocates nothing, and running asks the budget first.
    let big = huge(1 << 28, 1 << 20).unwrap().relu().unwrap().compile().unwrap();
    assert_eq!(budget.used(), 0);
    let empty: [u8; 0] = [];
    let input = View::contiguous(&empty, Shape::Matrix(0, 0)).unwrap();
    assert!(big.run_into(&[input], &mut [], &budget).is_err(), "the wrong shape, caught before any size");
    let out = big.run(&[], &budget);
    assert!(out.is_err(), "no input");
    assert_eq!(budget.peak(), 0);
    // Under a host's limits, the same sizes are refused sooner.
    let cautious = Domain::new(64, 8);
    assert!(Expr::slot(0, Shape::Matrix(1 << 20, 2), cautious.clone()).is_err());
    assert!(Expr::slot(0, Shape::Matrix(1 << 21, 0), cautious).is_err());
}

/// The work a plan counts reaches the policy for every kind of stage, whichever limit it comes from.
#[test]
fn the_work_limit_counts_what_each_stage_executes() {
    let shape = Shape::Matrix(64, 64);
    let limited = |work| Domain::with_limits(64, 8, Limits { elements: 1 << 20, work });
    let elementwise = |domain: Arc<Domain>| {
        let x = Expr::slot(0, shape, domain).unwrap();
        x.add(&x).unwrap().relu().unwrap().compile().is_ok()
    };
    assert!(elementwise(limited(64 * 64 * 2)));
    assert!(!elementwise(limited(64 * 64 * 2 - 1)));
    let product = |domain: Arc<Domain>| {
        let x = Expr::slot(0, shape, domain).unwrap();
        x.matmul(&x).map(|_| ()).is_ok()
    };
    assert!(product(limited(64 * 64 * 64)));
    assert!(!product(limited(64 * 64 * 64 - 1)));
    let rows = |domain: Arc<Domain>| Expr::slot(0, shape, domain).unwrap().softmax().unwrap().compile().is_ok();
    assert!(rows(limited(64 * 64 * 6)));
    assert!(!rows(limited(64 * 64 * 6 - 1)));
}

/// Everything the engine allocates starts on a cache line, at any size and when empty.
#[test]
fn engine_buffers_start_on_a_cache_line() {
    let budget = Budget::new(1 << 24);
    for bytes in [0usize, 1, 4, 63, 64, 65, 4096, 1 << 17, 1 << 20] {
        let allocation = budget.allocate(bytes).unwrap();
        assert!(allocation.bytes.as_ptr().addr().is_multiple_of(64), "{bytes} bytes");
        assert_eq!(allocation.bytes.len(), bytes);
        assert!(allocation.bytes.iter().all(|byte| *byte == 0), "{bytes} bytes aren't zeroed");
    }
    let mut seen = None;
    crate::compute::kernels::with_scratch(1024, |scratch| seen = Some(scratch.as_ptr().addr())).unwrap();
    assert!(seen.unwrap().is_multiple_of(64));
}

/// Bytes that must survive a kernel, around a region it may write.
const GUARD: usize = 64;
struct Guarded {
    bytes: Vec<u8>,
    start: usize,
    len: usize,
}
impl Guarded {
    fn new(len: usize, misalign: usize) -> Self {
        Self { bytes: vec![0xa5; GUARD + misalign + len + GUARD], start: GUARD + misalign, len }
    }
    fn out(&mut self) -> &mut [u8] {
        &mut self.bytes[self.start..self.start + self.len]
    }
    fn assert_guards(&self, label: &str) {
        let (before, rest) = self.bytes.split_at(self.start);
        let after = &rest[self.len..];
        assert!(before.iter().chain(after).all(|byte| *byte == 0xa5), "{label}: wrote outside its output");
    }
}

/// Exactly the shapes either side of every block boundary, at every misalignment of operands and
/// output, under each instantiation: bit-identical to a textbook loop and never outside the output.
#[test]
fn products_stay_inside_misaligned_outputs_and_match_the_textbook_at_every_boundary() {
    let domain = Domain::new(1 << 12, 1 << 12);
    let budget = Budget::new(1 << 26);
    let mut state = 0x6a09_e667_f3bc_c908_u64;
    let mut unit = move || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        (state >> 40) as f32 / (1u64 << 24) as f32 * 4.0 - 2.0
    };
    for &m in &[1usize, 2, 5, 6, 7, 11, 12, 13, 18, 37] {
        for &k in &[1usize, 2, 63, 64, 127, 128, 129, 255, 257] {
            for &n in &[1usize, 7, 8, 9, 15, 16, 17, 31, 32, 33, 255, 256, 257] {
                let misalign = (m + k + n) % 4;
                let a: Vec<f32> = (0..m * k).map(|_| unit()).collect();
                let b: Vec<f32> = (0..k * n).map(|_| unit()).collect();
                // Storage with the operands starting at a byte offset no f32 could sit at.
                let tensor = |values: &[f32], rows, cols, shift: usize| {
                    let mut bytes = vec![0u8; shift];
                    bytes.extend_from_slice(&bytes_of(values));
                    let strides = [cols * 4, 4];
                    Tensor::from_storage(Arc::new(Vector(bytes)), Shape::Matrix(rows, cols), shift, strides).unwrap()
                };
                let (ta, tb) = (tensor(&a, m, k, 3 - misalign), tensor(&b, k, n, misalign));
                let product = Expr::slot(0, Shape::Matrix(m, k), domain.clone())
                    .unwrap()
                    .matmul(&Expr::slot(1, Shape::Matrix(k, n), domain.clone()).unwrap())
                    .unwrap();
                let oracle = |fused: bool| {
                    let mut sums = vec![0.0f32; m * n];
                    for i in 0..m {
                        for j in 0..n {
                            for p in 0..k {
                                let (x, y) = (a[i * k + p], b[p * n + j]);
                                let sum = sums[i * n + j];
                                sums[i * n + j] = if fused { x.mul_add(y, sum) } else { sum + x * y };
                            }
                        }
                    }
                    sums
                };
                let prepared = [
                    (ta.clone(), tb.clone()),
                    (ta.clone(), tb.prepare_right(&budget).unwrap()),
                    (ta.prepare_left(&budget).unwrap(), tb.clone()),
                ];
                let strict = product.compile().unwrap();
                let fast = product.compile_with(Numeric::Fast).unwrap();
                let (strict_want, fused_want) = (oracle(false), oracle(true));
                for (left, right) in &prepared {
                    let views = [left.borrow(), right.borrow()];
                    for isa in [Isa::Baseline, Isa::Avx2, Isa::Fma].into_iter().filter(|isa| isa.available()) {
                        let label = format!("{m}x{k}x{n} {isa:?}");
                        let mut out = Guarded::new(m * n * 4, misalign);
                        strict.run_with(&views, out.out(), &budget, isa, CHUNK).unwrap();
                        out.assert_guards(&label);
                        let bits = |values: &[f32]| values.iter().map(|v| v.to_bits()).collect::<Vec<_>>();
                        assert_eq!(bits(&floats(out.out())), bits(&strict_want), "strict {label}");
                        // Dot products long enough to reorder are `relaxed_dot`'s tests' business.
                        let mut out = Guarded::new(m * n * 4, misalign);
                        fast.run_with(&views, out.out(), &budget, isa, CHUNK).unwrap();
                        out.assert_guards(&label);
                        if isa == Isa::Fma && (n > 1 || k < 64 || left.prepared_bytes() > 0 && m > 1) {
                            assert_eq!(bits(&floats(out.out())), bits(&fused_want), "fast {label}");
                        } else if isa != Isa::Fma {
                            assert_eq!(bits(&floats(out.out())), bits(&strict_want), "fast {label}");
                        }
                    }
                }
            }
        }
    }
}

/// The explicit kernels at the smallest products that reach them, small enough for Miri to run:
/// `MIRIFLAGS=-Zmiri-disable-isolation RUSTFLAGS='-Ctarget-feature=+avx2,+fma' cargo +nightly miri test`.
#[test]
fn the_full_size_kernels_at_their_smallest_stay_inside_what_they_were_given() {
    // Under those flags Miri must really be walking the explicit kernels, not the baseline.
    #[cfg(all(miri, target_feature = "fma"))]
    assert!(Isa::Fma.available() && Isa::Avx2.available());
    let domain = Domain::new(16, 16);
    let budget = Budget::new(1 << 20);
    for (m, k, n) in [(6, 3, 16), (7, 3, 16), (12, 2, 17), (6, 1, 32), (13, 2, 33)] {
        let a: Vec<f32> = (0..m * k).map(|i| i as f32 * 0.5 - 2.0).collect();
        let b: Vec<f32> = (0..k * n).map(|i| i as f32 * 0.25 - 1.0).collect();
        let shift = |values: &[f32], rows, cols| {
            let mut bytes = vec![0u8; 1];
            bytes.extend_from_slice(&bytes_of(values));
            Tensor::from_storage(Arc::new(Vector(bytes)), Shape::Matrix(rows, cols), 1, [cols * 4, 4]).unwrap()
        };
        let (ta, tb) = (shift(&a, m, k), shift(&b, k, n));
        let prepared = tb.prepare_right(&budget).unwrap();
        let product = Expr::slot(0, Shape::Matrix(m, k), domain.clone())
            .unwrap()
            .matmul(&Expr::slot(1, Shape::Matrix(k, n), domain.clone()).unwrap())
            .unwrap();
        for numeric in [Numeric::Strict, Numeric::Fast] {
            let program = product.compile_with(numeric).unwrap();
            for right in [&tb, &prepared] {
                let mut out = Guarded::new(m * n * 4, 1);
                program.run_into(&[ta.borrow(), right.borrow()], out.out(), &budget).unwrap();
                out.assert_guards("smallest");
                for i in 0..m {
                    for j in 0..n {
                        let want: f32 = (0..k).map(|p| a[i * k + p] * b[p * n + j]).sum();
                        let got = floats(out.out())[i * n + j];
                        assert!((got - want).abs() <= 1e-3, "{m}x{k}x{n}: {got} against {want}");
                    }
                }
            }
        }
    }
}

/// Every element type the engine keeps inline, at the edges of the inline capacity.
#[test]
fn inline_lists_hold_any_copy_type_across_their_spill() {
    fn check<T: Copy + PartialEq + std::fmt::Debug, const N: usize>(make: impl Fn(usize) -> T) {
        for count in [0, 1, N.saturating_sub(1), N, N + 1, N + 2, 2 * N + 3] {
            let want: Vec<T> = (0..count).map(&make).collect();
            let collected: Inline<T, N> = want.iter().copied().collect();
            assert_eq!(&*collected, want.as_slice());
            let mut pushed: Inline<T, N> = Inline::new();
            for (index, item) in want.iter().enumerate() {
                pushed.push(*item);
                assert_eq!(pushed.len(), index + 1);
                assert_eq!(&pushed[..=index], &want[..=index]);
            }
        }
    }
    check::<usize, 8>(|i| i);
    check::<u8, 1>(|i| i.to_le_bytes()[0]);
    check::<(), 3>(|_| ());
    check::<u16, 0>(|i| i.to_le_bytes()[0].into());
    check::<[u64; 9], 2>(|i| [i as u64; 9]);
    check::<&str, 4>(|i| if i % 2 == 0 { "even" } else { "odd" });
}

/// The exact sum of exact f32 products: fixed point in two's complement, its least significant bit
/// 2^-298, the smallest product's. Shares nothing with floating point until `to_f64`.
struct Exact([u64; 11]);
impl Exact {
    fn new() -> Self {
        Self([0; 11])
    }
    /// `value`'s integer mantissa and binary exponent.
    fn parts(value: f32) -> (i64, i32) {
        let bits = value.to_bits();
        let exponent = ((bits >> 23) & 0xff) as i32;
        let fraction = i64::from(bits & 0x007f_ffff);
        let (mantissa, scale) = if exponent == 0 { (fraction, -149) } else { (fraction | 0x0080_0000, exponent - 150) };
        (if bits >> 31 == 1 { -mantissa } else { mantissa }, scale)
    }
    fn add_scaled(&mut self, mantissa: i64, scale: i32) {
        let shift = usize::try_from(scale + 298).expect("no product is smaller");
        let (limb, bit) = (shift / 64, shift % 64);
        let wide = i128::from(mantissa) << bit;
        let pieces = [wide as u64, (wide >> 64) as u64];
        let fill = if wide < 0 { u64::MAX } else { 0 };
        let mut carry = false;
        for index in limb..self.0.len() {
            let piece = pieces.get(index - limb).copied().unwrap_or(fill);
            let (sum, first) = self.0[index].overflowing_add(piece);
            let (sum, second) = sum.overflowing_add(u64::from(carry));
            self.0[index] = sum;
            carry = first || second;
        }
    }
    fn add_product(&mut self, x: f32, y: f32) {
        let (mx, sx) = Self::parts(x);
        let (my, sy) = Self::parts(y);
        self.add_scaled(mx * my, sx + sy);
    }
    fn subtract(&mut self, value: f32) {
        let (mantissa, scale) = Self::parts(value);
        self.add_scaled(-mantissa, scale);
    }
    /// The magnitude, rounded: good enough for an error to compare with a bound.
    fn magnitude(&self) -> f64 {
        let negative = self.0[10] >> 63 == 1;
        let mut limbs = self.0;
        if negative {
            let mut carry = true;
            for limb in &mut limbs {
                let (flipped, overflow) = (!*limb).overflowing_add(u64::from(carry));
                *limb = flipped;
                carry = overflow;
            }
        }
        let mut total = 0.0f64;
        for limb in limbs.iter().rev() {
            total = total * 18_446_744_073_709_551_616.0 + *limb as f64;
        }
        total * 2f64.powi(-298)
    }
}

#[test]
fn the_exact_accumulator_is_exact() {
    let mut sum = Exact::new();
    sum.add_product(1e30, 1e8);
    sum.add_product(-1e30, 1e8);
    assert_eq!(sum.magnitude(), 0.0);
    sum.add_product(f32::from_bits(1), f32::from_bits(1));
    assert_eq!(sum.magnitude(), 2f64.powi(-298));
    sum.add_product(1.5, 1.5);
    assert_eq!(sum.magnitude(), 2.25);
    sum.add_product(-3.0, 2.0);
    assert_eq!(sum.magnitude(), 3.75);
    sum.add_scaled(1, -298);
    sum.subtract(-3.75);
    assert_eq!(sum.magnitude(), 2f64.powi(-297), "the smallest bits are still there");
}

/// Adversarial dot products. Each pair is `a` then `b`, of length `k`.
fn dot_families(k: usize, rng: &mut impl FnMut() -> u32) -> Vec<(&'static str, Vec<f32>, Vec<f32>)> {
    let unit = |rng: &mut dyn FnMut() -> u32| (rng() >> 8) as f32 / (1u32 << 24) as f32;
    let sign = |i: usize| if i.is_multiple_of(2) { 1.0f32 } else { -1.0 };
    let mut families = Vec::new();
    let mut row = |name, f: &mut dyn FnMut(usize) -> (f32, f32)| {
        let (a, b): (Vec<f32>, Vec<f32>) = (0..k).map(f).unzip();
        families.push((name, a, b));
    };
    // Nearly everything cancels, leaving what was small.
    let mut r = |i: usize| {
        let x = unit(rng) * 4.0 + 1.0;
        (if i < k / 2 { x } else { -1.0 }, 1.0 + (i as f32) * 1e-3)
    };
    row("cancellation", &mut r);
    let mut r = |i: usize| (sign(i) * 1.0e15 * (1.0 + unit(rng) * 1e-5), 1.0e15);
    row("alternating huge", &mut r);
    let mut r = |i: usize| {
        let exponent = (rng() % 180) as i32 - 90;
        (sign(i) * 2f32.powi(exponent) * (1.0 + unit(rng)), 2f32.powi(-exponent / 2))
    };
    row("exponents apart", &mut r);
    let mut r = |i: usize| (f32::from_bits(1 + rng() % 0x007f_ffff) * sign(i), 1.0 + unit(rng));
    row("subnormal terms", &mut r);
    let mut r = |i: usize| (f32::from_bits(1 + rng() % 0x007f_ffff) * sign(i), f32::from_bits(1 + rng() % 0x007f_ffff));
    row("products below every float", &mut r);
    let mut r = |i: usize| (sign(i) * 1e-23 * (1.0 + unit(rng)), 1e-22 * (1.0 + unit(rng)));
    row("products at the edge of underflow", &mut r);
    let mut r = |_| (1.0 + unit(rng), 1.0 + unit(rng));
    row("all positive", &mut r);
    let mut r = |i: usize| (if i.is_multiple_of(7) { 3.0e3 } else { 1e-3 } * sign(i), 1.0 + unit(rng));
    row("spikes", &mut r);
    let mut r = |i: usize| (if i.is_multiple_of(3) { -0.0 } else { 0.0 }, sign(i));
    row("signed zeros", &mut r);
    families
}

/// `(rows of a concatenated, b)` through a one-column product compiled `numeric`, on `tensors`.
fn product_column(a: &[f32], b: &[f32], rows: usize, numeric: Numeric, prepare: bool, isa: Isa) -> Vec<f32> {
    let k = b.len();
    let budget = Budget::new(1 << 28);
    let domain = Domain::new(16, 16);
    let (shape_a, shape_b) = (Shape::Matrix(rows, k), Shape::Matrix(k, 1));
    let program = Expr::slot(0, shape_a, domain.clone())
        .unwrap()
        .matmul(&Expr::slot(1, shape_b, domain).unwrap())
        .unwrap()
        .compile_with(numeric)
        .unwrap();
    let ta = Tensor::import(&bytes_of(a), shape_a, &budget).unwrap();
    let ta = if prepare { ta.prepare_left(&budget).unwrap() } else { ta };
    let tb = Tensor::import(&bytes_of(b), shape_b, &budget).unwrap();
    let mut out = vec![0u8; rows * 4];
    program.run_with(&[ta.borrow(), tb.borrow()], &mut out, &budget, isa, CHUNK).unwrap();
    floats(&out)
}

/// What the machine's rounding allows a sum whose terms each pass through `depth` fused or plain
/// operations, written with no first-order approximation: relative error on the exact magnitude, plus
/// half a subnormal step for each product that underflows and is not rescued by later additions.
fn dot_bound(depth: usize, k: usize, magnitude: f64) -> f64 {
    let unit = 2f64.powi(-24);
    let growth = depth as f64 * unit / (1.0 - depth as f64 * unit);
    growth * magnitude + k as f64 * 2f64.powi(-150)
}

/// Against exact arithmetic, for lengths either side of everything `relaxed_dot` divides by: the depth
/// of its fixed tree (one per pass over the lanes, five to combine them, one per tail element) rather
/// than the `k` an ordered sum has, plus what no relative bound covers, products below every float.
#[test]
fn every_path_of_a_dot_product_honours_its_bound_against_exact_arithmetic() {
    let mut state = 0x243f_6a88_u32;
    let mut rng = move || {
        state ^= state << 13;
        state ^= state >> 17;
        state ^= state << 5;
        state
    };
    for k in [1, 2, 31, 33, 63, 64, 65, 95, 96, 97, 127, 128, 129, 255, 257, 1000, 4097, 65_536 + 13] {
        for (name, a, b) in dot_families(k, &mut rng) {
            let mut exact = Exact::new();
            let mut magnitude = Exact::new();
            for (x, y) in a.iter().zip(&b) {
                exact.add_product(*x, *y);
                magnitude.add_product(x.abs(), y.abs());
            }
            let magnitude = magnitude.magnitude();
            assert!(magnitude < f64::from(f32::MAX), "{name}: partial sums may overflow, which the bound excludes");
            let lanes_depth = k / 32 + 5 + k % 32;
            let check = |result: f32, depth: usize, label: &str| {
                let mut error = Exact::new();
                error.0 = exact.0;
                error.subtract(result);
                let (error, bound) = (error.magnitude(), dot_bound(depth, k, magnitude));
                assert!(error <= bound, "{name} k={k} {label}: {result}, error {error:e} over {bound:e}");
            };
            let isas = [Isa::Baseline, Isa::Avx2, Isa::Fma];
            for isa in isas.into_iter().filter(|isa| isa.available()) {
                let label = format!("{isa:?}");
                check(product_column(&a, &b, 1, Numeric::Strict, false, isa)[0], k, &format!("strict {label}"));
                let relaxed = isa == Isa::Fma && k >= 64;
                let depth = if relaxed { lanes_depth } else { k };
                check(product_column(&a, &b, 1, Numeric::Fast, false, isa)[0], depth, &format!("fast {label}"));
                // A prepared panel is ordered, on two rows so that the panel kernel runs at all.
                let two: Vec<f32> = a.iter().chain(&a).copied().collect();
                for value in product_column(&two, &b, 2, Numeric::Fast, true, isa) {
                    check(value, k, &format!("fast prepared {label}"));
                }
            }
        }
    }
}

/// Past `f32::MAX` partial sums overflow, and where they do depends on the order, so the bound stops
/// holding. A reordered sum's answer there is its own, fixed by its tree: infinities that meet make NaN
/// where an ordered sum of the same terms stays finite. This records that, as the contract says.
#[test]
fn past_overflow_a_reordered_sum_answers_for_its_own_order() {
    if !Isa::Fma.available() && !cfg!(target_arch = "aarch64") {
        return;
    }
    let big = f32::MAX;
    // Two of each lane's terms fill it; the next lane over holds their negation.
    let a: Vec<f32> = (0..64).map(|i| if i % 2 == 0 { big } else { -big }).collect();
    let b = vec![1.0f32; 64];
    assert!(product_column(&a, &b, 1, Numeric::Fast, false, Isa::Fma)[0].is_nan(), "lanes overflowed apart");
    let ordered = product_column(&a, &b, 1, Numeric::Strict, false, Isa::detect())[0];
    assert_eq!(ordered, 0.0, "in order, each pair cancels before the next");
    // Nothing is lost when no sum overflows: all of this is exact.
    let halves: Vec<f32> = a.iter().map(|v| v / 64.0).collect();
    assert_eq!(product_column(&halves, &b, 1, Numeric::Fast, false, Isa::Fma)[0], 0.0);
}

/// NaN and infinity reach the answer on every path, and zeros keep the ordered sum's sign.
#[test]
fn special_values_mean_the_same_on_every_path_of_a_dot_product() {
    let isas = [Isa::Baseline, Isa::Avx2, Isa::Fma];
    for k in [8, 64, 65, 97, 300] {
        for isa in isas.into_iter().filter(|isa| isa.available()) {
            for numeric in [Numeric::Strict, Numeric::Fast] {
                for prepare in [false, true] {
                    let run = |a: &[f32], b: &[f32]| {
                        let two: Vec<f32> = a.iter().chain(a).copied().collect();
                        product_column(&two, b, 2, numeric, prepare, isa)[0]
                    };
                    let (ones, zeros) = (vec![1.0f32; k], vec![0.0f32; k]);
                    for at in [0, 1, k / 2, k - 1] {
                        let mut a = ones.clone();
                        a[at] = f32::NAN;
                        assert!(run(&a, &ones).is_nan(), "NaN {k} {isa:?} {numeric:?} {prepare}");
                        a[at] = f32::INFINITY;
                        assert_eq!(run(&a, &ones), f32::INFINITY, "inf {k} {isa:?} {numeric:?} {prepare}");
                        assert!(run(&a, &zeros).is_nan(), "inf*0 {k} {isa:?} {numeric:?} {prepare}");
                        let mut opposed = a.clone();
                        opposed[(at + 1) % k] = f32::NEG_INFINITY;
                        assert!(run(&opposed, &ones).is_nan(), "inf-inf {k} {isa:?} {numeric:?} {prepare}");
                    }
                    // Every sum starts from +0, and +0 + -0 is +0.
                    let negative = vec![-0.0f32; k];
                    assert_eq!(run(&negative, &ones).to_bits(), 0, "zeros {k} {isa:?} {numeric:?} {prepare}");
                }
            }
        }
    }
}
