//! Native benchmarks: the attribution ladder, the chunk sweep and the workload matrix.
//!
//! Every row is `SAMPLES` timed samples, each long enough to swamp the clock, reported as
//! median, minimum and p90. Each row states its contract: where output goes and what allocates.
#![allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss,
    clippy::float_cmp,
    clippy::inline_always,
    clippy::many_single_char_names,
    clippy::too_many_lines
)]
use l3i_graph_cpu_research::forward::Model;
use l3i_graph_cpu_research::reduction::exp_nonpositive;
use l3i_graph_cpu_research::reference::softmax_platform_exp;
use l3i_graph_cpu_research::{Budget, CHUNK, Domain, Expr, Isa, Numeric, Program, Shape, Storage, Tensor, View};
use std::alloc::{GlobalAlloc, Layout, System};
use std::hint::black_box;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

const SAMPLES: usize = 31;
const SAMPLE_TIME: Duration = Duration::from_millis(3);

/// Counts Rust-side heap acquisitions, to report allocations per invocation.
struct Counting;
static ALLOCATIONS: AtomicUsize = AtomicUsize::new(0);
static BYTES: AtomicUsize = AtomicUsize::new(0);
// SAFETY: delegates every operation to the system allocator unchanged.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        BYTES.fetch_add(layout.size(), Ordering::Relaxed);
        // SAFETY: forwarded contract.
        unsafe { System.alloc(layout) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: forwarded contract.
        unsafe { System.dealloc(ptr, layout) }
    }
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        BYTES.fetch_add(layout.size(), Ordering::Relaxed);
        // SAFETY: forwarded contract.
        unsafe { System.alloc_zeroed(layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        BYTES.fetch_add(new_size, Ordering::Relaxed);
        // SAFETY: forwarded contract.
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}
#[global_allocator]
static COUNTING: Counting = Counting;

/// Times `body`, returning per-call microseconds as (median, min, p90).
fn sample(mut body: impl FnMut()) -> (f64, f64, f64) {
    let mut iterations = 1usize;
    loop {
        let start = Instant::now();
        for _ in 0..iterations {
            body();
        }
        let elapsed = start.elapsed();
        if elapsed >= SAMPLE_TIME || iterations >= 1 << 24 {
            break;
        }
        let scale = (SAMPLE_TIME.as_secs_f64() / elapsed.as_secs_f64().max(1e-9)).clamp(1.5, 16.0);
        iterations = ((iterations as f64 * scale) as usize).max(iterations + 1);
    }
    let mut times: Vec<f64> = (0..SAMPLES)
        .map(|_| {
            let start = Instant::now();
            for _ in 0..iterations {
                body();
            }
            start.elapsed().as_secs_f64() * 1e6 / iterations as f64
        })
        .collect();
    times.sort_by(f64::total_cmp);
    (times[SAMPLES / 2], times[0], times[SAMPLES * 9 / 10])
}

fn row(label: &str, n: usize, body: impl FnMut()) -> f64 {
    let (median, min, p90) = sample(body);
    let per_element = if n == 0 { 0.0 } else { median * 1000.0 / n as f64 };
    println!(
        "{label:<46} n={n:<8} median_us={median:<12.3} min_us={min:<12.3} p90_us={p90:<12.3} ns_per_element={per_element:.3}"
    );
    median
}

fn floats(bytes: &[u8]) -> &[[u8; 4]] {
    bytes.as_chunks::<4>().0
}
fn fixture(n: usize) -> (Vec<u8>, Vec<u8>) {
    let a: Vec<u8> = (0..n).flat_map(|i| ((i % 101) as f32 - 50.0).to_le_bytes()).collect();
    let b: Vec<u8> = (0..n).flat_map(|i| ((i % 13) as f32 - 6.0).to_le_bytes()).collect();
    (a, b)
}
fn domain() -> Arc<Domain> {
    Domain::new(1 << 16, 1 << 12)
}
/// `relu(x + y + 2)`, over any shapes.
fn workload(x: Shape, y: Shape, domain: &Arc<Domain>) -> Program {
    let (x, y) = (Expr::slot(0, x, domain.clone()).unwrap(), Expr::slot(1, y, domain.clone()).unwrap());
    x.add(&y).unwrap().add(&Expr::constant(2.0, domain.clone()).unwrap()).unwrap().relu().unwrap().compile().unwrap()
}
#[inline(always)]
fn relu(v: f32) -> f32 {
    if v <= 0.0 { 0.0 } else { v }
}

// ---------------------------------------------------------------------------------------------
// Handwritten loops: what the compiler achieves with no graph at all. The fused AVX2 loops are the
// ceiling any generated machine code could reach, so they bound what a JIT could buy.
// ---------------------------------------------------------------------------------------------
#[inline(always)]
fn fused_f32_body(a: &[f32], b: &[f32], out: &mut [f32]) {
    for ((a, b), out) in a.iter().zip(b).zip(out) {
        *out = relu(*a + *b + 2.0);
    }
}
#[inline(never)]
fn fused_f32_baseline(a: &[f32], b: &[f32], out: &mut [f32]) {
    fused_f32_body(a, b, out);
}
#[inline(always)]
fn fused_bytes_body(a: &[[u8; 4]], b: &[[u8; 4]], out: &mut [[u8; 4]]) {
    for ((a, b), out) in a.iter().zip(b).zip(out) {
        *out = relu(f32::from_le_bytes(*a) + f32::from_le_bytes(*b) + 2.0).to_le_bytes();
    }
}
#[inline(never)]
fn fused_bytes_baseline(a: &[[u8; 4]], b: &[[u8; 4]], out: &mut [[u8; 4]]) {
    fused_bytes_body(a, b, out);
}
/// A 16-operation chain, the longest handwritten fused comparison.
#[inline(always)]
fn fused_chain_body(a: &[[u8; 4]], b: &[[u8; 4]], out: &mut [[u8; 4]]) {
    for ((a, b), out) in a.iter().zip(b).zip(out) {
        let (x, y) = (f32::from_le_bytes(*a), f32::from_le_bytes(*b));
        let mut v = x + y;
        v = relu(v * 0.5 + x);
        v = relu(v * 0.5 + y);
        v = relu(v * 0.5 + x);
        v = relu(v * 0.5 + y);
        v = relu(v * 0.5 + x);
        *out = v.to_le_bytes();
    }
}
#[inline(never)]
fn fused_chain_baseline(a: &[[u8; 4]], b: &[[u8; 4]], out: &mut [[u8; 4]]) {
    fused_chain_body(a, b, out);
}
/// Three monomorphic passes over the whole array: `dream.data`'s composition with no per-element
/// kind dispatch, answering what fusion itself buys.
#[inline(always)]
fn three_pass_body(a: &[f32], b: &[f32], out: &mut [f32]) {
    for ((a, b), out) in a.iter().zip(b).zip(&mut *out) {
        *out = *a + *b;
    }
    for out in &mut *out {
        *out += 2.0;
    }
    for out in &mut *out {
        *out = relu(*out);
    }
}
#[inline(never)]
fn three_pass_baseline(a: &[f32], b: &[f32], out: &mut [f32]) {
    three_pass_body(a, b, out);
}
#[cfg(target_arch = "x86_64")]
mod avx2 {
    use super::{fused_bytes_body, fused_chain_body, fused_f32_body, three_pass_body};
    #[target_feature(enable = "avx2")]
    pub fn fused_f32(a: &[f32], b: &[f32], out: &mut [f32]) {
        fused_f32_body(a, b, out);
    }
    #[target_feature(enable = "avx2")]
    pub fn fused_bytes(a: &[[u8; 4]], b: &[[u8; 4]], out: &mut [[u8; 4]]) {
        fused_bytes_body(a, b, out);
    }
    #[target_feature(enable = "avx2")]
    pub fn fused_chain(a: &[[u8; 4]], b: &[[u8; 4]], out: &mut [[u8; 4]]) {
        fused_chain_body(a, b, out);
    }
    #[target_feature(enable = "avx2")]
    pub fn three_pass(a: &[f32], b: &[f32], out: &mut [f32]) {
        three_pass_body(a, b, out);
    }
}

#[derive(Debug)]
struct Bytes(Vec<u8>);
impl Storage for Bytes {
    fn bytes(&self) -> &[u8] {
        &self.0
    }
}

/// Fused per-element dispatch over direct slices: the cost of dispatch alone, without `at`.
#[derive(Clone, Copy)]
enum Mini {
    Slot(usize),
    Constant(f32),
    Add(usize, usize),
    Relu(usize),
}
#[inline(never)]
fn dispatch_only(code: &[Mini], inputs: [&[f32]; 2], out: &mut [f32]) {
    let mut registers = [0.0f32; 8];
    for (i, out) in out.iter_mut().enumerate() {
        for (r, op) in code.iter().enumerate() {
            registers[r] = match *op {
                Mini::Slot(s) => inputs[s][i],
                Mini::Constant(v) => v,
                Mini::Add(a, b) => registers[a] + registers[b],
                Mini::Relu(a) => relu(registers[a]),
            };
        }
        *out = registers[code.len() - 1];
    }
}

fn attribution() {
    println!("\n# Attribution ladder: relu(a+b+2), n=65536, preallocated output unless stated");
    let n = 65536;
    let (a, b) = fixture(n);
    let (fa, fb): (Vec<f32>, Vec<f32>) = (
        floats(&a).iter().map(|e| f32::from_le_bytes(*e)).collect(),
        floats(&b).iter().map(|e| f32::from_le_bytes(*e)).collect(),
    );
    let mut out = vec![0.0f32; n];
    let mut out_bytes = vec![0u8; n * 4];
    let domain = domain();
    let budget = Budget::new(1 << 30);
    let shape = Shape::Matrix(1, n);
    let program = workload(shape, shape, &domain);
    let views = [View::contiguous(&a, shape).unwrap(), View::contiguous(&b, shape).unwrap()];

    row("rust_fused_f32_baseline", n, || fused_f32_baseline(black_box(&fa), black_box(&fb), black_box(&mut out)));
    #[cfg(target_arch = "x86_64")]
    {
        // SAFETY: this benchmark requires an AVX2 host; Isa::detect is asserted first.
        assert_eq!(Isa::detect(), Isa::Avx2);
        row("rust_fused_f32_avx2", n, || unsafe {
            avx2::fused_f32(black_box(&fa), black_box(&fb), black_box(&mut out));
        });
        row("rust_fused_bytes_avx2", n, || unsafe {
            avx2::fused_bytes(
                black_box(floats(&a)),
                black_box(floats(&b)),
                black_box(out_bytes.as_chunks_mut::<4>().0),
            );
        });
        row("rust_three_pass_monomorphic_avx2", n, || unsafe {
            avx2::three_pass(black_box(&fa), black_box(&fb), black_box(&mut out));
        });
    }
    row("rust_fused_bytes_baseline", n, || {
        fused_bytes_baseline(black_box(floats(&a)), black_box(floats(&b)), black_box(out_bytes.as_chunks_mut::<4>().0));
    });
    row("rust_three_pass_monomorphic_baseline", n, || {
        three_pass_baseline(black_box(&fa), black_box(&fb), black_box(&mut out));
    });
    let code = [Mini::Slot(0), Mini::Slot(1), Mini::Add(0, 1), Mini::Constant(2.0), Mini::Add(2, 3), Mini::Relu(4)];
    row("dispatch_per_element_direct_slices", n, || {
        dispatch_only(black_box(&code), [black_box(&fa), black_box(&fb)], black_box(&mut out));
    });
    let (va, vb) = (views[0], views[1]);
    row("fused_checked_view_indexing_no_dispatch", n, || {
        let (va, vb) = (black_box(va), black_box(vb));
        for (i, slot) in out_bytes.as_chunks_mut::<4>().0.iter_mut().enumerate() {
            *slot = relu(va.at(0, i) + vb.at(0, i) + 2.0).to_le_bytes();
        }
    });
    row("reference_interpreter_preallocated", n, || {
        program.reference_into(black_box(&views), black_box(&mut out_bytes));
    });
    row("reference_interpreter_with_result_alloc", n, || {
        black_box(program.run_reference(black_box(&views), &budget).unwrap());
    });
    for isa in [Isa::Baseline, Isa::detect()] {
        row(&format!("chunked_{isa:?}_preallocated"), n, || {
            program.run_with(black_box(&views), black_box(&mut out_bytes), &budget, isa, CHUNK).unwrap();
        });
    }
    let (ta, tb) = (Tensor::import(&a, shape, &budget).unwrap(), Tensor::import(&b, shape, &budget).unwrap());
    row("chunked_detected_with_result_alloc", n, || {
        black_box(program.run(black_box(&[&ta, &tb]), &budget).unwrap());
    });
    row("zero_filled_result_allocation_alone", n, || {
        black_box(Tensor::import(&a, shape, &budget).unwrap());
    });

    println!("\n# Chunk sweep: relu(a+b+2) and a 16-operation chain, preallocated");
    let chain = chain_program(shape, &domain);
    for chunk in [16, 64, 256, 512, 1024, 2048, 4096, 16384] {
        row(&format!("chunk_{chunk}_workload"), n, || {
            program.run_with(black_box(&views), black_box(&mut out_bytes), &budget, Isa::detect(), chunk).unwrap();
        });
        row(&format!("chunk_{chunk}_chain16"), n, || {
            chain.run_with(black_box(&views), black_box(&mut out_bytes), &budget, Isa::detect(), chunk).unwrap();
        });
    }
    #[cfg(target_arch = "x86_64")]
    row("rust_fused_chain16_avx2", n, || unsafe {
        avx2::fused_chain(black_box(floats(&a)), black_box(floats(&b)), black_box(out_bytes.as_chunks_mut::<4>().0));
    });
    row("rust_fused_chain16_baseline", n, || {
        fused_chain_baseline(black_box(floats(&a)), black_box(floats(&b)), black_box(out_bytes.as_chunks_mut::<4>().0));
    });
    // Check the handwritten chain and the program agree, so the comparison is of one computation.
    let mut check = vec![0u8; n * 4];
    chain.run_into(&views, &mut check, &budget).unwrap();
    fused_chain_baseline(floats(&a), floats(&b), out_bytes.as_chunks_mut::<4>().0);
    assert_eq!(check, out_bytes);
}

/// `relu(v*0.5 + x)` five times over `x+y`: sixteen kernels, hand-equal to `fused_chain_body`.
fn chain_program(shape: Shape, domain: &Arc<Domain>) -> Program {
    let x = Expr::slot(0, shape, domain.clone()).unwrap();
    let y = Expr::slot(1, shape, domain.clone()).unwrap();
    let half = Expr::constant(0.5, domain.clone()).unwrap();
    let mut v = x.add(&y).unwrap();
    for operand in [&x, &y, &x, &y, &x] {
        v = v.mul(&half).unwrap().add(operand).unwrap().relu().unwrap();
    }
    v.compile().unwrap()
}

fn matrix() {
    println!("\n# Workload matrix: each executor against an independently computed result");
    let domain = domain();
    let budget = Budget::new(1 << 30);
    for n in [0usize, 1, 16, 1024, 65536, 1 << 20] {
        let (a, b) = fixture(n);
        let shape = Shape::Matrix(1, n);
        let program = workload(shape, shape, &domain);
        let views = [View::contiguous(&a, shape).unwrap(), View::contiguous(&b, shape).unwrap()];
        let mut out = vec![0u8; n * 4];
        row("contiguous_chunked_preallocated", n, || {
            program.run_into(black_box(&views), black_box(&mut out), &budget).unwrap();
        });
        for (i, value) in floats(&out).iter().enumerate() {
            let expected = (((i % 101) as i32 - 50) + ((i % 13) as i32 - 6) + 2).max(0);
            assert_eq!(f32::from_le_bytes(*value), expected as f32);
        }
        let mut reference = vec![0u8; n * 4];
        row("contiguous_reference_preallocated", n, || {
            program.reference_into(black_box(&views), black_box(&mut reference));
        });
        assert_eq!(out, reference);
        #[cfg(target_arch = "x86_64")]
        row("contiguous_rust_fused_avx2", n, || unsafe {
            avx2::fused_bytes(
                black_box(floats(&a)),
                black_box(floats(&b)),
                black_box(reference.as_chunks_mut::<4>().0),
            );
        });
        assert_eq!(out, reference);

        let before = (ALLOCATIONS.load(Ordering::Relaxed), BYTES.load(Ordering::Relaxed));
        let runs = 100;
        for _ in 0..runs {
            program.run_into(&views, &mut out, &budget).unwrap();
        }
        let after = (ALLOCATIONS.load(Ordering::Relaxed), BYTES.load(Ordering::Relaxed));
        println!(
            "contiguous_chunked_preallocated_heap n={n} allocations_per_run={:.1} bytes_per_run={:.0}",
            (after.0 - before.0) as f64 / f64::from(runs),
            (after.1 - before.1) as f64 / f64::from(runs)
        );
        let (ta, tb) = (Tensor::import(&a, shape, &budget).unwrap(), Tensor::import(&b, shape, &budget).unwrap());
        let before = (ALLOCATIONS.load(Ordering::Relaxed), BYTES.load(Ordering::Relaxed));
        for _ in 0..runs {
            black_box(program.run(&[&ta, &tb], &budget).unwrap());
        }
        let after = (ALLOCATIONS.load(Ordering::Relaxed), BYTES.load(Ordering::Relaxed));
        println!(
            "contiguous_chunked_fresh_result_heap n={n} allocations_per_run={:.1} bytes_per_run={:.0}",
            (after.0 - before.0) as f64 / f64::from(runs),
            (after.1 - before.1) as f64 / f64::from(runs)
        );
        row("contiguous_chunked_fresh_result", n, || {
            black_box(program.run(black_box(&[&ta, &tb]), &budget).unwrap());
        });
        let result = program.run(&[&ta, &tb], &budget).unwrap();
        row("readback_new_vec", n, || {
            black_box(black_box(&result).readback().unwrap());
        });
    }

    println!("\n# Strided and broadcast inputs (preallocated)");
    for n in [1024usize, 65536] {
        // Every second element of a 2n allocation, against a contiguous input.
        let (a, b) = fixture(n);
        let mut padded = vec![0u8; n * 8];
        for (i, e) in floats(&a).iter().enumerate() {
            padded[i * 8..i * 8 + 4].copy_from_slice(e);
        }
        let shape = Shape::Matrix(1, n);
        let strided = Tensor::from_storage(Arc::new(Bytes(padded)), shape, 0, [n * 8, 8]).unwrap();
        let b_t = Tensor::import(&b, shape, &budget).unwrap();
        let program = workload(shape, shape, &domain);
        let views = [strided.borrow(), b_t.borrow()];
        let mut out = vec![0u8; n * 4];
        row("strided_chunked", n, || {
            program.run_into(black_box(&views), black_box(&mut out), &budget).unwrap();
        });
        let mut reference = vec![0u8; n * 4];
        row("strided_reference", n, || {
            program.reference_into(black_box(&views), black_box(&mut reference));
        });
        assert_eq!(out, reference);

        // A transposed square, walked across its columns (the cache's worst case).
        let side = (n as f64).sqrt() as usize;
        let square = Shape::Matrix(side, side);
        let (s, t) = fixture(side * side);
        let transposed = Tensor::import(&s, square, &budget).unwrap().view(square, 0, [4, side * 4]).unwrap();
        let t_t = Tensor::import(&t, square, &budget).unwrap();
        let program = workload(square, square, &domain);
        let views = [transposed.borrow(), t_t.borrow()];
        let mut out = vec![0u8; side * side * 4];
        row("transposed_chunked", side * side, || {
            program.run_into(black_box(&views), black_box(&mut out), &budget).unwrap();
        });
        let mut reference = vec![0u8; side * side * 4];
        row("transposed_reference", side * side, || {
            program.reference_into(black_box(&views), black_box(&mut reference));
        });
        assert_eq!(out, reference);

        // [rows,cols] + [rows,1]: a column broadcast splatted per row, including short rows.
        for cols in [2usize, 256] {
            let rows = n / cols;
            let (s, t) = fixture(rows * cols);
            let (full, column) = (Shape::Matrix(rows, cols), Shape::Matrix(rows, 1));
            let program = workload(full, column, &domain);
            let views = [View::contiguous(&s, full).unwrap(), View::contiguous(&t[..rows * 4], column).unwrap()];
            let mut out = vec![0u8; rows * cols * 4];
            row(&format!("broadcast_column_cols{cols}_chunked"), rows * cols, || {
                program.run_into(black_box(&views), black_box(&mut out), &budget).unwrap();
            });
            let mut reference = vec![0u8; rows * cols * 4];
            row(&format!("broadcast_column_cols{cols}_reference"), rows * cols, || {
                program.reference_into(black_box(&views), black_box(&mut reference));
            });
            assert_eq!(out, reference);
        }
    }

    println!("\n# Compilation: cold and warm, no machine code generated");
    let n = 65536;
    let shape = Shape::Matrix(1, n);
    row("expression_construction", 0, || {
        let x = Expr::slot(0, shape, domain.clone()).unwrap();
        let y = Expr::slot(1, shape, domain.clone()).unwrap();
        let two = Expr::constant(2.0, domain.clone()).unwrap();
        black_box(x.add(&y).unwrap().add(&two).unwrap().relu().unwrap());
    });
    let x = Expr::slot(0, shape, domain.clone()).unwrap();
    let y = Expr::slot(1, shape, domain.clone()).unwrap();
    let two = Expr::constant(2.0, domain.clone()).unwrap();
    let graph = x.add(&y).unwrap().add(&two).unwrap().relu().unwrap();
    row("validation_and_planning_compile", 0, || {
        black_box(graph.compile().unwrap());
    });
    let start = Instant::now();
    let cold = graph.compile().unwrap();
    println!("first_compile_after_warmup_us={:.3}", start.elapsed().as_secs_f64() * 1e6);
    drop(cold);

    println!("\n# Independent programs: 64 distinct compiled programs, run in turn on one input");
    let (a, b) = fixture(1024);
    let small = Shape::Matrix(1, 1024);
    let views = [View::contiguous(&a, small).unwrap(), View::contiguous(&b, small).unwrap()];
    let start = Instant::now();
    let programs: Vec<Program> = (0..64)
        .map(|i| {
            let x = Expr::slot(0, small, domain.clone()).unwrap();
            let y = Expr::slot(1, small, domain.clone()).unwrap();
            let c = Expr::constant(i as f32, domain.clone()).unwrap();
            x.mul(&c).unwrap().add(&y).unwrap().relu().unwrap().compile().unwrap()
        })
        .collect();
    println!("compile_64_programs_total_us={:.3}", start.elapsed().as_secs_f64() * 1e6);
    let mut out = vec![0u8; 1024 * 4];
    row("run_64_programs_each_once", 64 * 1024, || {
        for program in &programs {
            program.run_into(black_box(&views), black_box(&mut out), &budget).unwrap();
        }
    });

    println!("\n# Matrix multiplication, strict f32, packing included");
    for (m, k, n) in [
        (16usize, 16, 16),
        (64, 64, 64),
        (128, 128, 128),
        (256, 256, 256),
        (400, 400, 400),
        (1, 256, 256),
        (1, 1024, 1024),
        (4, 1024, 1024),
        (256, 256, 1),
        (31, 127, 17),
        (128, 64, 512),
        (512, 64, 32),
    ] {
        println!("shape m={m} k={k} n={n}");
        let left = Shape::Matrix(m, k);
        let right = Shape::Matrix(k, n);
        let a: Vec<u8> = (0..m * k).flat_map(|i| ((i % 17) as f32 * 0.25 - 2.0).to_le_bytes()).collect();
        let b: Vec<u8> = (0..k * n).flat_map(|i| ((i % 23) as f32 * 0.125 - 1.0).to_le_bytes()).collect();
        let program = Expr::slot(0, left, domain.clone())
            .unwrap()
            .matmul(&Expr::slot(1, right, domain.clone()).unwrap())
            .unwrap()
            .compile()
            .unwrap();
        let (ta, tb) = (Tensor::import(&a, left, &budget).unwrap(), Tensor::import(&b, right, &budget).unwrap());
        let views = [ta.borrow(), tb.borrow()];
        let scalar_operations = m * k * n;
        let mut out = vec![0u8; m * n * 4];
        row("matmul_chunked_baseline_preallocated", scalar_operations, || {
            program.run_with(black_box(&views), black_box(&mut out), &budget, Isa::Baseline, CHUNK).unwrap();
        });
        row("matmul_chunked_detected_preallocated", scalar_operations, || {
            program.run_into(black_box(&views), black_box(&mut out), &budget).unwrap();
        });
        row("matmul_chunked_fresh_result", scalar_operations, || {
            black_box(program.run(black_box(&[&ta, &tb]), &budget).unwrap());
        });
        let reference = program.run_reference(&views, &budget).unwrap().readback().unwrap();
        assert_eq!(out, reference, "matmul executors agree on bits");
        row("matmul_reference", scalar_operations, || {
            black_box(program.run_reference(black_box(&views), &budget).unwrap());
        });
    }
    assert_eq!(budget.used(), 0);
    println!("budget_peak_bytes={}", budget.peak());
}

pub fn native() {
    println!("# isa={:?} chunk={CHUNK} samples={SAMPLES} sample_ms={} ", Isa::detect(), SAMPLE_TIME.as_millis());
    reductions();
    attribution();
    matrix();
}

/// Heap acquisitions per call of `body`, across all of Rust's allocation paths.
fn heap(label: &str, body: &mut impl FnMut()) {
    let runs = 64;
    let before = (ALLOCATIONS.load(Ordering::Relaxed), BYTES.load(Ordering::Relaxed));
    for _ in 0..runs {
        body();
    }
    let after = (ALLOCATIONS.load(Ordering::Relaxed), BYTES.load(Ordering::Relaxed));
    println!(
        "{label:<46} allocations_per_run={:.1} bytes_per_run={:.0}",
        (after.0 - before.0) as f64 / f64::from(runs),
        (after.1 - before.1) as f64 / f64::from(runs)
    );
}

/// Whether the `L3I_BENCH_ONLY` filter admits this group of rows.
fn wanted(group: &str) -> bool {
    std::env::var("L3I_BENCH_ONLY").map_or(true, |filter| filter.split(',').any(|name| group.contains(name)))
}

fn row_operand(rows: usize, cols: usize) -> Vec<u8> {
    (0..rows * cols).flat_map(|i| ((i * 37 % 127) as f32 * 0.125 - 8.0).to_le_bytes()).collect()
}

/// Row operations over inference-shaped inputs, whole program invocations into preallocated output.
fn shaped_rows() {
    println!("\n# Row stages by shape: preallocated output, whole program, one thread");
    let domain = domain();
    let budget = Budget::new(1 << 30);
    let shapes = [
        (1usize, 128usize),
        (1, 4096),
        (1, 65536),
        (64, 64),
        (128, 128),
        (64, 128),
        (1000, 7),
        (333, 13),
        (7, 1000),
        (4096, 1),
    ];
    for (rows, cols) in shapes {
        let shape = Shape::Matrix(rows, cols);
        let tensor = Tensor::import(&row_operand(rows, cols), shape, &budget).unwrap();
        // The same shape read across the columns of a contiguous cols-by-rows owner.
        let transposed = Tensor::import(&row_operand(cols, rows), Shape::Matrix(cols, rows), &budget)
            .unwrap()
            .view(shape, 0, [4, rows * 4])
            .ok();
        let x = Expr::slot(0, shape, domain.clone()).unwrap();
        let operations: [(&str, Expr); 4] = [
            ("softmax", x.softmax().unwrap()),
            ("layer_norm", x.normalize(1e-5, true).unwrap()),
            ("rms_norm", x.normalize(1e-5, false).unwrap()),
            ("sum_rows", x.sum_rows().unwrap()),
        ];
        for (name, expression) in operations {
            let program = expression.compile().unwrap();
            let mut out = vec![0u8; program.output_shape().elements().unwrap() * 4];
            let label = format!("{name}_{rows}x{cols}");
            let views = [tensor.borrow()];
            let mut body = || program.run_into(black_box(&views), black_box(&mut out), &budget).unwrap();
            row(&label, rows * cols, &mut body);
            heap(&label, &mut body);
            if let Some(transposed) = &transposed
                && rows > 1
                && cols > 1
            {
                let views = [transposed.borrow()];
                row(&format!("{label}_strided"), rows * cols, || {
                    program.run_into(black_box(&views), black_box(&mut out), &budget).unwrap();
                });
            }
        }
    }
    assert_eq!(budget.used(), 0);
}

/// Matrix products by shape, in both numerical modes, with allocation counts.
fn shaped_matmul() {
    println!("\n# Matmul by shape: preallocated output, whole program including packing, one thread");
    let domain = domain();
    let budget = Budget::new(1 << 30);
    for (m, k, n) in [
        (1usize, 128usize, 128usize),
        (1, 4096, 1),
        (1, 1024, 1024),
        (4, 1024, 1024),
        (8, 512, 512),
        (64, 128, 1),
        (256, 256, 1),
        (4096, 16, 1),
        (16, 16, 16),
        (5, 7, 3),
        (31, 127, 17),
        (64, 64, 64),
        (64, 128, 128),
        (128, 128, 128),
        (256, 256, 256),
        (128, 64, 512),
        (512, 64, 32),
        (400, 400, 400),
    ] {
        let (left, right) = (Shape::Matrix(m, k), Shape::Matrix(k, n));
        let a: Vec<u8> = (0..m * k).flat_map(|i| ((i % 17) as f32 * 0.25 - 2.0).to_le_bytes()).collect();
        let b: Vec<u8> = (0..k * n).flat_map(|i| ((i % 23) as f32 * 0.125 - 1.0).to_le_bytes()).collect();
        let (ta, tb) = (Tensor::import(&a, left, &budget).unwrap(), Tensor::import(&b, right, &budget).unwrap());
        let views = [ta.borrow(), tb.borrow()];
        let expression = Expr::slot(0, left, domain.clone())
            .unwrap()
            .matmul(&Expr::slot(1, right, domain.clone()).unwrap())
            .unwrap();
        let mut out = vec![0u8; m * n * 4];
        for (mode, numeric) in [("strict", Numeric::Strict), ("fast", Numeric::Fast)] {
            let program = expression.compile_with(numeric).unwrap();
            let label = format!("matmul_{mode}_{m}x{k}x{n}");
            let mut body = || program.run_into(black_box(&views), black_box(&mut out), &budget).unwrap();
            row(&label, m * k * n, &mut body);
            heap(&label, &mut body);
        }
        let strict = expression.compile().unwrap();
        row(&format!("matmul_baseline_{m}x{k}x{n}"), m * k * n, || {
            strict.run_with(black_box(&views), black_box(&mut out), &budget, Isa::Baseline, CHUNK).unwrap();
        });
    }
    assert_eq!(budget.used(), 0);
}

/// The whole synthetic pass: planning, binding, execution, and what each kernel family costs alone.
fn forward_pass() {
    println!("\n# Synthetic forward pass as one graph, one thread");
    let model = Model { tokens: 64, width: 128, blocks: 2 };
    let slots = model.slots();
    let inputs = model.inputs();
    let domain = Domain::new(1 << 12, 1 << 12);
    let budget = Budget::new(1 << 30);
    let tensors: Vec<Tensor> =
        inputs.iter().zip(&slots).map(|(bytes, shape)| Tensor::import(bytes, *shape, &budget).unwrap()).collect();
    let expression = model.graph(&domain).unwrap();
    row("forward_expression_construction", 0, || {
        black_box(model.graph(&domain).unwrap());
    });
    for (mode, numeric) in [("strict", Numeric::Strict), ("fast", Numeric::Fast)] {
        row(&format!("forward_compile_{mode}"), 0, || {
            black_box(expression.compile_with(numeric).unwrap());
        });
        let program = expression.compile_with(numeric).unwrap();
        let elements = program.output_shape().elements().unwrap();
        let mut out = vec![0u8; elements * 4];
        row(&format!("forward_bind_views_{mode}"), 0, || {
            let views: Vec<View<'_>> = tensors.iter().map(Tensor::borrow).collect();
            program.signature().iter().zip(&views).for_each(|(shape, view)| assert_eq!(*shape, view.shape()));
            black_box(views);
        });
        let views: Vec<View<'_>> = tensors.iter().map(Tensor::borrow).collect();
        let label = format!("forward_run_into_{mode}");
        let mut body = || program.run_into(black_box(&views), black_box(&mut out), &budget).unwrap();
        row(&label, elements, &mut body);
        heap(&label, &mut body);
        assert!(floats(&out).iter().all(|e| f32::from_le_bytes(*e).is_finite()));
        let references: Vec<&Tensor> = tensors.iter().collect();
        row(&format!("forward_fresh_result_{mode}"), elements, || {
            black_box(program.run(black_box(&references), &budget).unwrap());
        });
        println!("forward_budget_peak_bytes_{mode}={}", budget.peak());
    }
    assert_eq!(budget.used(), tensors.iter().map(|t| t.shape().elements().unwrap() * 4).sum::<usize>());
}

pub fn shapes() {
    println!(
        "# isa={:?} fast_isa={:?} samples={SAMPLES} sample_ms={}",
        Isa::detect(),
        Isa::detect_fast(),
        SAMPLE_TIME.as_millis()
    );
    if wanted("rows") {
        shaped_rows();
    }
    if wanted("matmul") {
        shaped_matmul();
    }
    if wanted("forward") {
        forward_pass();
    }
}

/// Softmax with the platform's `exp` once per element, keeping every exponential in f64 scratch.
fn softmax_platform_once(values: &[f32], out: &mut [f32], exponentials: &mut [f64]) {
    let max = f64::from(values.iter().copied().fold(f32::NEG_INFINITY, f32::max));
    let mut total = 0.0;
    for (value, target) in values.iter().zip(exponentials.iter_mut()) {
        *target = (f64::from(*value) - max).exp();
        total += *target;
    }
    for (slot, exponential) in out.iter_mut().zip(exponentials.iter()) {
        *slot = (*exponential / total) as f32;
    }
}

/// The deterministic exponential twice per element: no scratch, twice the arithmetic.
#[inline(always)]
fn softmax_two_pass_body(values: &[f32], out: &mut [f32]) {
    let max = f64::from(values.iter().copied().fold(f32::NEG_INFINITY, f32::max));
    let mut totals = [0.0f64; 4];
    let (blocks, rest) = values.as_chunks::<4>();
    for block in blocks {
        for lane in 0..4 {
            totals[lane] += exp_nonpositive(f64::from(block[lane]) - max);
        }
    }
    for (lane, value) in rest.iter().enumerate() {
        totals[lane] += exp_nonpositive(f64::from(*value) - max);
    }
    let total = totals[0] + totals[1] + totals[2] + totals[3];
    for (slot, value) in out.iter_mut().zip(values) {
        *slot = (exp_nonpositive(f64::from(*value) - max) / total) as f32;
    }
}

/// `e^x` for `x <= 0` to about 2e-7 relative in f32: degree 5 and f32 exponents. An experiment under
/// its own error model, and not the accurate contract.
#[inline(always)]
fn exp_f32_approximate(x: f32) -> f32 {
    let rounded = x * std::f32::consts::LOG2_E + 12_582_912.0;
    let k = rounded - 12_582_912.0;
    let r = x - k * 0.693_359_4 - k * -2.121_944_4e-4;
    let p = 1.0 + r * (1.0 + r * (0.5 + r * (0.166_666_67 + r * (0.041_666_668 + r * 0.008_333_334))));
    let scale = f32::from_bits(rounded.to_bits().wrapping_add(127) << 23);
    if x < -87.0 { 0.0 } else { p * scale }
}
#[inline(always)]
fn softmax_approximate_body(values: &[f32], out: &mut [f32]) {
    let max = values.iter().copied().fold(f32::NEG_INFINITY, f32::max);
    for (slot, value) in out.iter_mut().zip(values) {
        *slot = exp_f32_approximate(*value - max);
    }
    let mut totals = [0.0f32; 8];
    let (blocks, rest) = out.as_chunks::<8>();
    for block in blocks {
        for lane in 0..8 {
            totals[lane] += block[lane];
        }
    }
    for (lane, value) in rest.iter().enumerate() {
        totals[lane] += *value;
    }
    let total: f32 = totals.iter().sum();
    let scale = 1.0 / total;
    for slot in out.iter_mut() {
        *slot *= scale;
    }
}
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
fn softmax_two_pass_avx2(values: &[f32], out: &mut [f32]) {
    softmax_two_pass_body(values, out);
}
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2,fma")]
fn softmax_approximate_fma(values: &[f32], out: &mut [f32]) {
    softmax_approximate_body(values, out);
}
fn softmax_two_pass(values: &[f32], out: &mut [f32]) {
    #[cfg(target_arch = "x86_64")]
    if Isa::Avx2.available() {
        // SAFETY: AVX2 was just detected.
        return unsafe { softmax_two_pass_avx2(values, out) };
    }
    softmax_two_pass_body(values, out);
}
fn softmax_approximate(values: &[f32], out: &mut [f32]) {
    #[cfg(target_arch = "x86_64")]
    if Isa::Fma.available() {
        // SAFETY: AVX2 and FMA were just detected.
        return unsafe { softmax_approximate_fma(values, out) };
    }
    softmax_approximate_body(values, out);
}

/// Every softmax strategy the review asked about, timed and held against the same f64 oracle.
pub fn softmax_strategies() {
    println!("\n# Softmax strategies, one row, preallocated");
    let budget = Budget::new(1 << 26);
    for n in [16usize, 128, 1024, 4096, 65536] {
        let values: Vec<f32> = (0..n).map(|i| (i * 37 % 127) as f32 * 0.125 - 8.0).collect();
        let mut out = vec![0.0f32; n];
        let mut exponentials = vec![0.0f64; n];
        row("softmax_a_platform_exp_twice", n, || {
            softmax_platform_exp(black_box(&values), black_box(&mut out));
        });
        row("softmax_b_platform_exp_once_scratch", n, || {
            softmax_platform_once(black_box(&values), black_box(&mut out), black_box(&mut exponentials));
        });
        row("softmax_d_deterministic_exp_twice_no_scratch", n, || {
            softmax_two_pass(black_box(&values), black_box(&mut out));
        });
        let shape = Shape::Matrix(1, n);
        let tensor = Tensor::import(&row_operand(1, n), shape, &budget).unwrap();
        let program = Expr::slot(0, shape, domain()).unwrap().softmax().unwrap().compile().unwrap();
        let mut bytes = vec![0u8; n * 4];
        row("softmax_c_deterministic_exp_once_program", n, || {
            program.run_into(black_box(&[tensor.borrow()]), black_box(&mut bytes), &budget).unwrap();
        });
        row("softmax_e_approximate_f32", n, || {
            softmax_approximate(black_box(&values), black_box(&mut out));
        });
    }
    println!("\n# Approximate f32 softmax against the platform-exp f64 oracle");
    let cases: [(&str, Vec<f32>); 4] = [
        ("moderate", (0..1024).map(|i| (i * 37 % 127) as f32 * 0.125 - 8.0).collect()),
        ("wide_90", (0..1024).map(|i| (i * 37 % 1021) as f32 * 0.09 - 90.0).collect()),
        ("extreme_negative", (0..1024).map(|i| if i == 0 { 0.0 } else { -(i as f32) }).collect()),
        ("large_offset", (0..1024).map(|i| 1.0e7 + (i * 37 % 127) as f32).collect()),
    ];
    for (name, values) in &cases {
        let mut oracle = vec![0.0f32; values.len()];
        let mut approximate = oracle.clone();
        softmax_platform_exp(values, &mut oracle);
        softmax_approximate(values, &mut approximate);
        let (mut absolute, mut relative) = (0.0f64, 0.0f64);
        let mut total = 0.0f64;
        for (a, o) in approximate.iter().zip(&oracle) {
            let difference = (f64::from(*a) - f64::from(*o)).abs();
            absolute = absolute.max(difference);
            if *o > 0.0 {
                relative = relative.max(difference / f64::from(*o));
            } else if *a > 0.0 {
                relative = f64::INFINITY;
            }
            total += f64::from(*a);
        }
        println!(
            "softmax_e_error case={name} max_absolute={absolute:.3e} max_relative={relative:.3e} total_minus_one={:.3e}",
            total - 1.0
        );
    }
    assert_eq!(budget.used(), 0);
}

pub fn reductions() {
    use l3i_graph_cpu_research::reduction::{normalize, softmax, sum};
    println!("\n# Row kernels: preallocated, accurate normalization, ordered f32 sum");
    for n in [1, 16, 1024, 65_536, 1_048_576] {
        let values: Vec<f32> = (0..n).map(|i| (i % 127) as f32 * 0.125 - 8.0).collect();
        let mut out = vec![0.0; n];
        let budget = Budget::new(1 << 26);
        let domain = Domain::new(1024, 1024);
        let input: Vec<u8> = values.iter().flat_map(|v| v.to_le_bytes()).collect();
        let tensor = Tensor::import(&input, Shape::Matrix(1, n), &budget).unwrap();
        let program = Expr::slot(0, Shape::Matrix(1, n), domain).unwrap().sum_rows().unwrap().compile().unwrap();
        let mut result = [0u8; 4];
        row("ordered_sum_program", n, || {
            program.run_into(black_box(&[tensor.borrow()]), black_box(&mut result), &budget).unwrap();
        });
        assert_eq!(f32::from_le_bytes(result).to_bits(), sum(&values).to_bits());
        row("ordered_sum_kernel", n, || {
            black_box(sum(black_box(&values)));
        });
        let softmax_plan =
            Expr::slot(0, Shape::Matrix(1, n), Domain::new(1024, 1024)).unwrap().softmax().unwrap().compile().unwrap();
        let mut softmax_bytes = vec![0u8; n * 4];
        row("accurate_softmax_program", n, || {
            softmax_plan.run_into(black_box(&[tensor.borrow()]), black_box(&mut softmax_bytes), &budget).unwrap();
        });
        let mut exponentials = vec![0.0; n];
        row("accurate_softmax_kernel", n, || {
            softmax(black_box(&values), black_box(&mut out), black_box(&mut exponentials));
        });
        let total: f64 = out.iter().map(|v| f64::from(*v)).sum();
        assert!((total - 1.0).abs() < 1e-6);
        for (center, label) in [(false, "accurate_rms_norm_program"), (true, "accurate_layer_norm_program")] {
            let plan = Expr::slot(0, Shape::Matrix(1, n), Domain::new(1024, 1024))
                .unwrap()
                .normalize(1e-5, center)
                .unwrap()
                .compile()
                .unwrap();
            row(label, n, || {
                plan.run_into(black_box(&[tensor.borrow()]), black_box(&mut softmax_bytes), &budget).unwrap();
            });
        }
        row("accurate_rms_norm_kernel", n, || normalize(black_box(&values), black_box(&mut out), 1e-5, false).unwrap());
        row("accurate_layer_norm_kernel", n, || {
            normalize(black_box(&values), black_box(&mut out), 1e-5, true).unwrap();
        });
    }
}
