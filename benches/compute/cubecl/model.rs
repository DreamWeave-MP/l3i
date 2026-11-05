//! Experiment: where a small transformer-style forward pass spends its work, by kernel family.
//!
//! Synthetic weights, one attention head, no training. The computation is the point: layer
//! normalization, three linear projections, scaled dot-product attention, a residual and a
//! feed-forward layer, twice. Operations the plan cannot express yet (normalization, softmax,
//! transposition) are plain native loops here, shared by both sides and labelled, so the split shows
//! which families deserve work before any are added to the IR.
//!
//! Two backends share everything but matmul and elementwise: the plan's kernels, and a baseline of
//! `matrixmultiply`'s packed BLAS-style microkernels with handwritten autovectorized loops.
#![allow(clippy::many_single_char_names)]
use super::{Counts, count_all, hard_fixture};
use cubecl::prelude::CubeElement;
use l3i::compute::{Budget, Domain, Expr, Numeric, Program, Shape, View};
use std::collections::HashMap;
use std::sync::Arc;

const SEQUENCE: usize = 64;
const WIDTH: usize = 128;
const BLOCKS: usize = 2;
const PASSES: usize = 7;

#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
enum Family {
    MatMul,
    Elementwise,
    Normalization,
    Softmax,
    Transpose,
}
const FAMILIES: [(Family, &str); 5] = [
    (Family::MatMul, "matmul"),
    (Family::Elementwise, "elementwise"),
    (Family::Normalization, "normalization_native"),
    (Family::Softmax, "softmax_native"),
    (Family::Transpose, "transpose_native"),
];

/// The two operations whose implementation differs between backends.
trait Backend {
    fn matmul(&mut self, a: &[f32], m: usize, k: usize, b: &[f32], n: usize, out: &mut [f32]);
    /// `out = a + b`, elementwise.
    fn add(&mut self, a: &[f32], b: &[f32], out: &mut [f32]);
    /// `out = relu(a + bias)`.
    fn relu_shift(&mut self, a: &[f32], bias: f32, out: &mut [f32]);
    /// `out = a * scale`.
    fn scale(&mut self, a: &[f32], scale: f32, out: &mut [f32]);
}

/// The plan's kernels, one compiled program per shape and operation.
struct Plan {
    domain: Arc<Domain>,
    budget: Arc<Budget>,
    numeric: Numeric,
    programs: HashMap<(u8, usize, usize, usize, u32), Program>,
}
impl Plan {
    fn new(numeric: Numeric) -> Self {
        Self { domain: Domain::new(1 << 16, 1 << 12), budget: Budget::new(1 << 30), numeric, programs: HashMap::new() }
    }
    fn elementwise(
        &mut self,
        key: (u8, usize, u32),
        a: &[f32],
        b: Option<&[f32]>,
        out: &mut [f32],
        graph: impl Fn(&Arc<Domain>, Shape) -> Expr,
    ) {
        let shape = Shape::Matrix(1, out.len());
        let (domain, numeric) = (self.domain.clone(), self.numeric);
        let program = self
            .programs
            .entry((key.0, key.1, 0, 0, key.2))
            .or_insert_with(|| graph(&domain, shape).compile_with(numeric).unwrap());
        let first = View::contiguous(f32::as_bytes(a), shape).unwrap();
        let bytes = bytemuck_mut(out);
        if let Some(b) = b {
            let second = View::contiguous(f32::as_bytes(b), shape).unwrap();
            program.run_into(&[first, second], bytes, &self.budget).unwrap();
        } else {
            program.run_into(&[first], bytes, &self.budget).unwrap();
        }
    }
}
fn bytemuck_mut(values: &mut [f32]) -> &mut [u8] {
    // SAFETY: any f32 has valid bytes, u8 has no alignment needs, and any byte pattern is an f32.
    unsafe { std::slice::from_raw_parts_mut(values.as_mut_ptr().cast(), std::mem::size_of_val(values)) }
}
impl Backend for Plan {
    fn matmul(&mut self, a: &[f32], m: usize, k: usize, b: &[f32], n: usize, out: &mut [f32]) {
        let (domain, numeric) = (self.domain.clone(), self.numeric);
        let program = self.programs.entry((0, m, k, n, 0)).or_insert_with(|| {
            Expr::slot(0, Shape::Matrix(m, k), domain.clone())
                .unwrap()
                .matmul(&Expr::slot(1, Shape::Matrix(k, n), domain.clone()).unwrap())
                .unwrap()
                .compile_with(numeric)
                .unwrap()
        });
        let views = [
            View::contiguous(f32::as_bytes(a), Shape::Matrix(m, k)).unwrap(),
            View::contiguous(f32::as_bytes(b), Shape::Matrix(k, n)).unwrap(),
        ];
        program.run_into(&views, bytemuck_mut(out), &self.budget).unwrap();
    }
    fn add(&mut self, a: &[f32], b: &[f32], out: &mut [f32]) {
        self.elementwise((1, out.len(), 0), a, Some(b), out, |domain, shape| {
            Expr::slot(0, shape, domain.clone()).unwrap().add(&Expr::slot(1, shape, domain.clone()).unwrap()).unwrap()
        });
    }
    fn relu_shift(&mut self, a: &[f32], bias: f32, out: &mut [f32]) {
        self.elementwise((2, out.len(), bias.to_bits()), a, None, out, move |domain, shape| {
            let constant = Expr::constant(bias, domain.clone()).unwrap();
            Expr::slot(0, shape, domain.clone()).unwrap().add(&constant).unwrap().relu().unwrap()
        });
    }
    fn scale(&mut self, a: &[f32], scale: f32, out: &mut [f32]) {
        self.elementwise((3, out.len(), scale.to_bits()), a, None, out, move |domain, shape| {
            let constant = Expr::constant(scale, domain.clone()).unwrap();
            Expr::slot(0, shape, domain.clone()).unwrap().mul(&constant).unwrap()
        });
    }
}

/// Packed BLAS-style matmul and plain loops under AVX2 and FMA: a well-optimized native baseline.
struct Baseline;
#[target_feature(enable = "avx2,fma")]
fn baseline_add(a: &[f32], b: &[f32], out: &mut [f32]) {
    for ((slot, a), b) in out.iter_mut().zip(a).zip(b) {
        *slot = a + b;
    }
}
#[target_feature(enable = "avx2,fma")]
fn baseline_relu_shift(a: &[f32], bias: f32, out: &mut [f32]) {
    for (slot, a) in out.iter_mut().zip(a) {
        let value = a + bias;
        *slot = if value <= 0.0 { 0.0 } else { value };
    }
}
#[target_feature(enable = "avx2,fma")]
fn baseline_scale(a: &[f32], scale: f32, out: &mut [f32]) {
    for (slot, a) in out.iter_mut().zip(a) {
        *slot = a * scale;
    }
}
impl Backend for Baseline {
    fn matmul(&mut self, a: &[f32], m: usize, k: usize, b: &[f32], n: usize, out: &mut [f32]) {
        assert!(a.len() == m * k && b.len() == k * n && out.len() == m * n);
        // SAFETY: the pointers and row-major strides describe exactly the asserted slices.
        unsafe {
            matrixmultiply::sgemm(
                m,
                k,
                n,
                1.0,
                a.as_ptr(),
                k as isize,
                1,
                b.as_ptr(),
                n as isize,
                1,
                0.0,
                out.as_mut_ptr(),
                n as isize,
                1,
            );
        }
    }
    fn add(&mut self, a: &[f32], b: &[f32], out: &mut [f32]) {
        // SAFETY: the benchmark asserts AVX2 and FMA on entry.
        unsafe { baseline_add(a, b, out) }
    }
    fn relu_shift(&mut self, a: &[f32], bias: f32, out: &mut [f32]) {
        // SAFETY: as above.
        unsafe { baseline_relu_shift(a, bias, out) }
    }
    fn scale(&mut self, a: &[f32], scale: f32, out: &mut [f32]) {
        // SAFETY: as above.
        unsafe { baseline_scale(a, scale, out) }
    }
}

/// Native loops the plan cannot express yet, shared by both backends.
#[target_feature(enable = "avx2,fma")]
fn normalize(x: &[f32], width: usize, out: &mut [f32]) {
    for (row, target) in x.chunks_exact(width).zip(out.chunks_exact_mut(width)) {
        let mean = row.iter().sum::<f32>() / width as f32;
        let variance = row.iter().map(|v| (v - mean) * (v - mean)).sum::<f32>() / width as f32;
        let scale = 1.0 / (variance + 1e-5).sqrt();
        for (slot, value) in target.iter_mut().zip(row) {
            *slot = (value - mean) * scale;
        }
    }
}
#[target_feature(enable = "avx2,fma")]
fn softmax(x: &mut [f32], width: usize) {
    for row in x.chunks_exact_mut(width) {
        let largest = row.iter().copied().fold(f32::NEG_INFINITY, f32::max);
        // exp(y) by a polynomial and squarings: an autovectorizable shape, not a libm call.
        for value in row.iter_mut() {
            let y = ((*value - largest) / 16.0).max(-8.0);
            let mut e = 1.0 + y * (1.0 + y * (0.5 + y * (1.0 / 6.0 + y / 24.0)));
            e *= e;
            e *= e;
            e *= e;
            e *= e;
            *value = e;
        }
        let total: f32 = row.iter().sum();
        let scale = 1.0 / total;
        for value in row.iter_mut() {
            *value *= scale;
        }
    }
}
#[target_feature(enable = "avx2,fma")]
fn transpose(a: &[f32], m: usize, n: usize, out: &mut [f32]) {
    for i in 0..m {
        for j in 0..n {
            out[j * m + i] = a[i * n + j];
        }
    }
}

struct Weights {
    input: Vec<f32>,
    query: Vec<f32>,
    key: Vec<f32>,
    value: Vec<f32>,
    output: Vec<f32>,
    hidden: Vec<f32>,
    project: Vec<f32>,
}
fn weights() -> Weights {
    let matrix = |seed: u32, scale: f32, n: usize| -> Vec<f32> {
        hard_fixture(n, seed, false).iter().map(|v| v * scale).collect()
    };
    let scale = 1.0 / (WIDTH as f32).sqrt();
    let square = WIDTH * WIDTH;
    Weights {
        input: matrix(11, 1.0, SEQUENCE * WIDTH),
        query: matrix(12, scale, square),
        key: matrix(13, scale, square),
        value: matrix(14, scale, square),
        output: matrix(15, scale, square),
        hidden: matrix(16, scale, square),
        project: matrix(17, scale, square),
    }
}

/// One forward pass, adding each stage's counters to its family.
fn forward(backend: &mut dyn Backend, w: &Weights, totals: &mut HashMap<Family, Counts>) -> Vec<f32> {
    let cells = SEQUENCE * WIDTH;
    let mut x = w.input.clone();
    let mut normal = vec![0.0f32; cells];
    let (mut q, mut k, mut v) = (vec![0.0f32; cells], vec![0.0f32; cells], vec![0.0f32; cells]);
    let mut k_t = vec![0.0f32; cells];
    let mut scores = vec![0.0f32; SEQUENCE * SEQUENCE];
    let mut scaled = vec![0.0f32; SEQUENCE * SEQUENCE];
    let (mut context, mut projected, mut sum) = (vec![0.0f32; cells], vec![0.0f32; cells], vec![0.0f32; cells]);
    let (mut hidden, mut active, mut feed) = (vec![0.0f32; cells], vec![0.0f32; cells], vec![0.0f32; cells]);
    let mut stage = |family: Family, body: &mut dyn FnMut()| {
        let counts = count_all(body);
        let entry = totals.entry(family).or_default();
        *entry = entry.plus(counts);
    };
    for _ in 0..BLOCKS {
        // SAFETY for the native loops throughout: the caller asserts AVX2 and FMA.
        stage(Family::Normalization, &mut || unsafe { normalize(&x, WIDTH, &mut normal) });
        stage(Family::MatMul, &mut || backend.matmul(&normal, SEQUENCE, WIDTH, &w.query, WIDTH, &mut q));
        stage(Family::MatMul, &mut || backend.matmul(&normal, SEQUENCE, WIDTH, &w.key, WIDTH, &mut k));
        stage(Family::MatMul, &mut || backend.matmul(&normal, SEQUENCE, WIDTH, &w.value, WIDTH, &mut v));
        stage(Family::Transpose, &mut || unsafe { transpose(&k, SEQUENCE, WIDTH, &mut k_t) });
        stage(Family::MatMul, &mut || backend.matmul(&q, SEQUENCE, WIDTH, &k_t, SEQUENCE, &mut scores));
        stage(Family::Elementwise, &mut || backend.scale(&scores, 1.0 / (WIDTH as f32).sqrt(), &mut scaled));
        stage(Family::Softmax, &mut || unsafe { softmax(&mut scaled, SEQUENCE) });
        stage(Family::MatMul, &mut || backend.matmul(&scaled, SEQUENCE, SEQUENCE, &v, WIDTH, &mut context));
        stage(Family::MatMul, &mut || backend.matmul(&context, SEQUENCE, WIDTH, &w.output, WIDTH, &mut projected));
        stage(Family::Elementwise, &mut || backend.add(&x, &projected, &mut sum));
        stage(Family::MatMul, &mut || backend.matmul(&sum, SEQUENCE, WIDTH, &w.hidden, WIDTH, &mut hidden));
        stage(Family::Elementwise, &mut || backend.relu_shift(&hidden, 0.1, &mut active));
        stage(Family::MatMul, &mut || backend.matmul(&active, SEQUENCE, WIDTH, &w.project, WIDTH, &mut feed));
        stage(Family::Elementwise, &mut || backend.add(&sum, &feed, &mut x));
    }
    x
}

fn report(label: &str, backend: &mut dyn Backend, w: &Weights) -> Vec<f32> {
    // Warm the per-shape programs, then take the pass with the median instruction count.
    let mut scratch = HashMap::new();
    let mut result = forward(backend, w, &mut scratch);
    let mut passes: Vec<HashMap<Family, Counts>> = Vec::new();
    for _ in 0..PASSES {
        let mut totals = HashMap::new();
        result = forward(backend, w, &mut totals);
        passes.push(totals);
    }
    let sum_instructions = |pass: &HashMap<Family, Counts>| pass.values().map(|c| c.instructions).sum::<u64>();
    passes.sort_by_key(sum_instructions);
    let median = &passes[PASSES / 2];
    let total = sum_instructions(median);
    let total_cycles: u64 = median.values().map(|c| c.cycles).sum();
    for (family, name) in FAMILIES {
        let c = median.get(&family).copied().unwrap_or_default();
        println!(
            "model_{label}_{name} instructions={} cycles={} l1d_misses={} llc_misses={} share_of_instructions={:.1}% share_of_cycles={:.1}%",
            c.instructions,
            c.cycles,
            c.l1d_misses,
            c.llc_misses,
            c.instructions as f64 * 100.0 / total as f64,
            c.cycles as f64 * 100.0 / total_cycles as f64,
        );
    }
    println!("model_{label}_total instructions={total} cycles={total_cycles}");
    result
}

pub fn run() {
    assert!(std::is_x86_feature_detected!("avx2") && std::is_x86_feature_detected!("fma"));
    println!("# model sequence={SEQUENCE} width={WIDTH} blocks={BLOCKS} passes={PASSES} (median pass)");
    let w = weights();
    let strict = report("plan_strict", &mut Plan::new(Numeric::Strict), &w);
    let fast = report("plan_fast", &mut Plan::new(Numeric::Fast), &w);
    let baseline = report("baseline", &mut Baseline, &w);
    // The three should agree closely, not exactly: rounding differs by contract.
    let worst =
        |a: &[f32], b: &[f32]| a.iter().zip(b).map(|(x, y)| (x - y).abs() / x.abs().max(1.0)).fold(0.0f32, f32::max);
    println!(
        "model_agreement strict_vs_fast={:.2e} strict_vs_baseline={:.2e}",
        worst(&strict, &fast),
        worst(&strict, &baseline)
    );
    assert!(strict.iter().all(|v| v.is_finite()), "synthetic weights must stay finite");
}
