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
use l3i_graph_cpu_research::{Budget, CHUNK, Domain, Expr, Isa, Program, Shape, Storage, Tensor, View};
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
        row("accurate_softmax_kernel", n, || softmax(black_box(&values), black_box(&mut out)));
        let total: f64 = out.iter().map(|v| f64::from(*v)).sum();
        assert!((total - 1.0).abs() < 1e-6);
        row("accurate_rms_norm_kernel", n, || normalize(black_box(&values), black_box(&mut out), 1e-5, false).unwrap());
        row("accurate_layer_norm_kernel", n, || {
            normalize(black_box(&values), black_box(&mut out), 1e-5, true).unwrap();
        });
    }
}
