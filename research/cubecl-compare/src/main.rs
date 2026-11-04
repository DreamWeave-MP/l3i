//! Head to head: the chunked kernels against CubeCL 0.11's CPU backend, on one set of inputs.
//!
//! Each side runs in its own process (`compare ours`, `compare cubecl`), so CubeCL's worker pool
//! cannot disturb the other side's timings. CubeCL gets its own best configuration: `Vector` kernels
//! at several widths and `UNIT_POS` partitioning for threads. Kernel-only rows reuse preallocated
//! storage and wait on `sync`, so they exclude upload and readback; a separate row includes them.
#![allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::needless_range_loop,
    clippy::too_many_arguments,
    clippy::too_many_lines,
    clippy::similar_names,
    clippy::many_single_char_names,
    // CubeCL's kernel language is Rust syntax with its own methods: these suggestions don't apply.
    clippy::assign_op_pattern,
    clippy::manual_div_ceil,
    clippy::manual_is_multiple_of,
    clippy::cast_possible_wrap,
    clippy::doc_markdown,
    clippy::cast_sign_loss
)]
use cubecl::bytes::Bytes;
use cubecl::cpu::{CpuDevice, CpuRuntime};
use cubecl::prelude::*;
use cubecl_runtime::runtime::Runtime;
use cubecl_runtime::server::Handle;
use l3i_graph_cpu_research::{Budget, CHUNK, Domain, Expr, Isa, Program, Shape, View};
use rayon::prelude::*;
use std::hint::black_box;
use std::sync::Arc;
use std::time::{Duration, Instant};

const SAMPLES: usize = 31;
const SAMPLE_TIME: Duration = Duration::from_millis(5);
const VERSIONS: [usize; 3] = [1, 8, 16];
const THREADS: [usize; 3] = [1, 8, 16];
const ELEMENTS: [usize; 5] = [16, 1024, 16384, 65536, 1 << 20];
const SIDES: [usize; 3] = [64, 256, 400];

// ---------------------------------------------------------------------------------------------
// CubeCL kernels. Strict by default: no fast-math flags, and its CPU target sets no contraction.
// ---------------------------------------------------------------------------------------------
#[cube]
fn relu_lanes<N: Size>(x: Vector<f32, N>) -> Vector<f32, N> {
    let zero = Vector::new(0.0f32);
    // Written as `x <= 0 ? +0 : x`: NaN stays, both zeros become +0.
    select_many(x.less_equal(&zero), zero, x)
}

#[cube(launch_unchecked)]
fn workload<N: Size>(
    a: &[Vector<f32, N>],
    b: &[Vector<f32, N>],
    out: &mut [Vector<f32, N>],
    count: usize,
    threads: usize,
) {
    let unit = UNIT_POS as usize;
    let start = unit * count / threads;
    let end = (unit + 1) * count / threads;
    for i in start..end {
        out[i] = relu_lanes::<N>(a[i] + b[i] + Vector::new(2.0f32));
    }
}

#[cube(launch_unchecked, fast_math = FastMath::all())]
fn workload_fast<N: Size>(
    a: &[Vector<f32, N>],
    b: &[Vector<f32, N>],
    out: &mut [Vector<f32, N>],
    count: usize,
    threads: usize,
) {
    let unit = UNIT_POS as usize;
    let start = unit * count / threads;
    let end = (unit + 1) * count / threads;
    for i in start..end {
        out[i] = relu_lanes::<N>(a[i] + b[i] + Vector::new(2.0f32));
    }
}

#[cube(launch_unchecked)]
fn chain16<N: Size>(
    a: &[Vector<f32, N>],
    b: &[Vector<f32, N>],
    out: &mut [Vector<f32, N>],
    count: usize,
    threads: usize,
) {
    let unit = UNIT_POS as usize;
    let start = unit * count / threads;
    let end = (unit + 1) * count / threads;
    let half = Vector::new(0.5f32);
    for i in start..end {
        let x = a[i];
        let y = b[i];
        let mut v = x + y;
        v = relu_lanes::<N>(v * half + x);
        v = relu_lanes::<N>(v * half + y);
        v = relu_lanes::<N>(v * half + x);
        v = relu_lanes::<N>(v * half + y);
        v = relu_lanes::<N>(v * half + x);
        out[i] = v;
    }
}

/// `chain16` with fusion defeated. CubeCL's optimizer fuses a product into its add only when that
/// add is the product's sole use, so a second use, never taken when `guard` is 0, keeps each
/// multiply and add separately rounded. LLVM has no contraction flag to fuse them later.
#[cube(launch_unchecked)]
fn chain16_strict<N: Size>(
    a: &[Vector<f32, N>],
    b: &[Vector<f32, N>],
    out: &mut [Vector<f32, N>],
    count: usize,
    threads: usize,
    guard: usize,
) {
    let unit = UNIT_POS as usize;
    let start = unit * count / threads;
    let end = (unit + 1) * count / threads;
    let half = Vector::new(0.5f32);
    for i in start..end {
        let x = a[i];
        let y = b[i];
        let mut v = x + y;
        let p0 = v * half;
        v = relu_lanes::<N>(p0 + x);
        let p1 = v * half;
        v = relu_lanes::<N>(p1 + y);
        let p2 = v * half;
        v = relu_lanes::<N>(p2 + x);
        let p3 = v * half;
        v = relu_lanes::<N>(p3 + y);
        let p4 = v * half;
        v = relu_lanes::<N>(p4 + x);
        if guard == 1 {
            out[0] = p0 + p1 * p2 * p3 * p4;
        }
        out[i] = v;
    }
}

/// `matmul_tuned` with fusion defeated the same way.
#[cube(launch_unchecked)]
fn matmul_tuned_strict<N: Size>(
    a: &[f32],
    b: &[Vector<f32, N>],
    out: &mut [Vector<f32, N>],
    m: usize,
    k: usize,
    v: usize,
    threads: usize,
    guard: usize,
) {
    let unit = UNIT_POS as usize;
    let start = unit * m / threads;
    let end = (unit + 1) * m / threads;
    for i in start..end {
        for j in 0..v {
            out[i * v + j] = Vector::new(0.0f32);
        }
    }
    let blocks = (k + 63) / 64;
    for block in 0..blocks {
        let first = block * 16;
        let groups = min(k - block * 64, 64usize) / 4;
        for i in start..end {
            for group in 0..groups {
                let p = (first + group) * 4;
                let s0 = Vector::new(a[i * k + p]);
                let s1 = Vector::new(a[i * k + p + 1]);
                let s2 = Vector::new(a[i * k + p + 2]);
                let s3 = Vector::new(a[i * k + p + 3]);
                for j in 0..v {
                    let p0 = s0 * b[p * v + j];
                    let p1 = s1 * b[(p + 1) * v + j];
                    let p2 = s2 * b[(p + 2) * v + j];
                    let p3 = s3 * b[(p + 3) * v + j];
                    if guard == 1 {
                        out[0] = p0 / p1 / p2 / p3;
                    }
                    out[i * v + j] = out[i * v + j] + p0 + p1 + p2 + p3;
                }
            }
        }
    }
}

/// A plain product and add, to expose fusion in elementwise kernels: `a * b + 2`.
#[cube(launch_unchecked)]
fn product_add<N: Size>(a: &[Vector<f32, N>], b: &[Vector<f32, N>], out: &mut [Vector<f32, N>], count: usize) {
    for i in 0..count {
        out[i] = a[i] * b[i] + Vector::new(2.0f32);
    }
}

/// The simple `i-k-j` matmul, strict order: `b` and `out` are vectors of `v` per row.
#[cube(launch_unchecked)]
fn matmul_simple<N: Size>(
    a: &[f32],
    b: &[Vector<f32, N>],
    out: &mut [Vector<f32, N>],
    m: usize,
    k: usize,
    v: usize,
    threads: usize,
) {
    let unit = UNIT_POS as usize;
    let start = unit * m / threads;
    let end = (unit + 1) * m / threads;
    for i in start..end {
        for j in 0..v {
            out[i * v + j] = Vector::new(0.0f32);
        }
        for p in 0..k {
            let s = Vector::new(a[i * k + p]);
            for j in 0..v {
                out[i * v + j] = out[i * v + j] + s * b[p * v + j];
            }
        }
    }
}

/// The same structure as the native kernel: four rows of `b` per load and store of `out`, in
/// ascending blocks of 64. Additions stay in order. `k` must be a multiple of four.
#[cube(launch_unchecked)]
fn matmul_tuned<N: Size>(
    a: &[f32],
    b: &[Vector<f32, N>],
    out: &mut [Vector<f32, N>],
    m: usize,
    k: usize,
    v: usize,
    threads: usize,
) {
    let unit = UNIT_POS as usize;
    let start = unit * m / threads;
    let end = (unit + 1) * m / threads;
    for i in start..end {
        for j in 0..v {
            out[i * v + j] = Vector::new(0.0f32);
        }
    }
    let blocks = (k + 63) / 64;
    for block in 0..blocks {
        let first = block * 16;
        let groups = min(k - block * 64, 64usize) / 4;
        for i in start..end {
            for group in 0..groups {
                let p = (first + group) * 4;
                let s0 = Vector::new(a[i * k + p]);
                let s1 = Vector::new(a[i * k + p + 1]);
                let s2 = Vector::new(a[i * k + p + 2]);
                let s3 = Vector::new(a[i * k + p + 3]);
                for j in 0..v {
                    let mut value = out[i * v + j];
                    value = value + s0 * b[p * v + j];
                    value = value + s1 * b[(p + 1) * v + j];
                    value = value + s2 * b[(p + 2) * v + j];
                    value = value + s3 * b[(p + 3) * v + j];
                    out[i * v + j] = value;
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Harness.
// ---------------------------------------------------------------------------------------------
fn sample(mut body: impl FnMut()) -> (f64, f64, f64) {
    let mut iterations = 1usize;
    loop {
        let start = Instant::now();
        for _ in 0..iterations {
            body();
        }
        let elapsed = start.elapsed();
        if elapsed >= SAMPLE_TIME || iterations >= 1 << 22 {
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
fn row(label: &str, n: usize, body: impl FnMut()) {
    let (median, min, p90) = sample(body);
    println!(
        "{label:<52} n={n:<9} median_us={median:<12.3} min_us={min:<12.3} p90_us={p90:<12.3} ns_per_element={:.4}",
        median * 1000.0 / n as f64
    );
}
fn bytes_of(values: &[f32]) -> Vec<u8> {
    values.iter().flat_map(|v| v.to_le_bytes()).collect()
}
fn fixture(n: usize) -> (Vec<f32>, Vec<f32>) {
    ((0..n).map(|i| (i % 101) as f32 - 50.0).collect(), (0..n).map(|i| (i % 13) as f32 - 6.0).collect())
}
/// Same-magnitude values that round on every operation, with edge values mixed in where `edges`.
fn hard_fixture(n: usize, seed: u32, edges: bool) -> Vec<f32> {
    let mut state = seed;
    (0..n)
        .map(|i| {
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            if edges && i % 7 == 0 {
                return [0.0, -0.0, f32::NAN, f32::INFINITY, f32::NEG_INFINITY, 1e-40, -1e-40]
                    [(state >> 28) as usize % 7];
            }
            f32::from_bits((state >> 9) | ((126 + (state & 1)) << 23) | ((state >> 8 & 1) << 31))
        })
        .collect()
}
fn relu(x: f32) -> f32 {
    if x <= 0.0 { 0.0 } else { x }
}
fn oracle_workload(a: &[f32], b: &[f32]) -> Vec<f32> {
    a.iter().zip(b).map(|(a, b)| relu(*a + *b + 2.0)).collect()
}
fn oracle_chain(a: &[f32], b: &[f32]) -> Vec<f32> {
    a.iter()
        .zip(b)
        .map(|(x, y)| {
            let (x, y) = (*x, *y);
            let mut v = x + y;
            for operand in [x, y, x, y, x] {
                v = relu(v * 0.5 + operand);
            }
            v
        })
        .collect()
}
fn oracle_matmul(a: &[f32], b: &[f32], side: usize) -> Vec<f32> {
    let mut out = vec![0.0f32; side * side];
    for i in 0..side {
        for j in 0..side {
            let mut sum = 0.0f32;
            for p in 0..side {
                sum += a[i * side + p] * b[p * side + j];
            }
            out[i * side + j] = sum;
        }
    }
    out
}
fn floats(bytes: &[u8]) -> Vec<f32> {
    bytes.as_chunks::<4>().0.iter().map(|b| f32::from_le_bytes(*b)).collect()
}
/// How many elements differ from the strict scalar result, with any NaN equal to any NaN.
fn differences(actual: &[f32], expected: &[f32]) -> usize {
    assert_eq!(actual.len(), expected.len());
    actual.iter().zip(expected).filter(|(a, e)| !((a.is_nan() && e.is_nan()) || a.to_bits() == e.to_bits())).count()
}

// ---------------------------------------------------------------------------------------------
// Handwritten fused ceilings: what generated code could reach, with and without FMA.
// ---------------------------------------------------------------------------------------------
macro_rules! ceiling {
    ($name:ident, $features:literal, |$x:ident, $y:ident, $s:ident, $p:ident| $step:expr) => {
        #[target_feature(enable = $features)]
        fn $name(a: &[[u8; 4]], b: &[[u8; 4]], out: &mut [[u8; 4]]) {
            for ((a, b), slot) in a.iter().zip(b).zip(out) {
                let ($x, $y) = (f32::from_le_bytes(*a), f32::from_le_bytes(*b));
                let mut v = $x + $y;
                for $p in [$x, $y, $x, $y, $x] {
                    let $s = v;
                    v = $step;
                    v = if v <= 0.0 { 0.0 } else { v };
                }
                *slot = v.to_le_bytes();
            }
        }
    };
}
ceiling!(chain_avx2, "avx2", |x, y, s, p| s * 0.5 + p);
ceiling!(chain_fma, "avx2,fma", |x, y, s, p| s.mul_add(0.5, p));

/// The native kernel's matmul structure with each multiply-add fused.
#[target_feature(enable = "avx2,fma")]
fn matmul_fma(a: &[[u8; 4]], b: &[[u8; 4]], side: usize, out: &mut [[u8; 4]]) {
    let read = f32::from_le_bytes;
    out.fill([0; 4]);
    for block in (0..side).step_by(64) {
        for (o, row) in out.chunks_exact_mut(side).zip(a.chunks_exact(side)) {
            let mut p = block;
            while p + 4 <= (block + 64).min(side) {
                let (s0, s1, s2, s3) = (read(row[p]), read(row[p + 1]), read(row[p + 2]), read(row[p + 3]));
                let (b0, b1) = (&b[p * side..][..side], &b[(p + 1) * side..][..side]);
                let (b2, b3) = (&b[(p + 2) * side..][..side], &b[(p + 3) * side..][..side]);
                for j in 0..side {
                    let mut v = read(o[j]);
                    v = s0.mul_add(read(b0[j]), v);
                    v = s1.mul_add(read(b1[j]), v);
                    v = s2.mul_add(read(b2[j]), v);
                    v = s3.mul_add(read(b3[j]), v);
                    o[j] = v.to_le_bytes();
                }
                p += 4;
            }
        }
    }
}

fn ceilings(a: &[u8], b: &[u8], n: usize) {
    let (x, y) = (a.as_chunks::<4>().0, b.as_chunks::<4>().0);
    let mut out = vec![[0u8; 4]; n];
    assert!(std::is_x86_feature_detected!("avx2") && std::is_x86_feature_detected!("fma"));
    // SAFETY: both features were just detected.
    row("rust_fused_chain16_strict_threads1", n, || unsafe {
        chain_avx2(black_box(x), black_box(y), black_box(&mut out));
    });
    // SAFETY: as above.
    row("rust_fused_chain16_fma_threads1", n, || unsafe {
        chain_fma(black_box(x), black_box(y), black_box(&mut out));
    });
}

fn pin_current(cpu: usize) {
    // SAFETY: a zeroed cpu_set_t is valid, and CPU indices are in range on this 16-CPU host.
    unsafe {
        let mut set: libc::cpu_set_t = std::mem::zeroed();
        libc::CPU_SET(cpu % 16, &mut set);
        libc::sched_setaffinity(0, std::mem::size_of::<libc::cpu_set_t>(), &raw const set);
    }
}

/// Pins rayon's workers the way CubeCL pins its pool: one per physical core first (on this host,
/// CPUs 0 to 7 are distinct cores and 8 to 15 their siblings).
fn pinned_pool(threads: usize) -> rayon::ThreadPool {
    rayon::ThreadPoolBuilder::new().num_threads(threads).start_handler(pin_current).build().unwrap()
}

// ---------------------------------------------------------------------------------------------
// Our side. Threads split the output with the public API: one program per part shape, on rayon.
// ---------------------------------------------------------------------------------------------
fn our_workload(shape: Shape, domain: &Arc<Domain>) -> Program {
    let x = Expr::slot(0, shape, domain.clone()).unwrap();
    let y = Expr::slot(1, shape, domain.clone()).unwrap();
    x.add(&y).unwrap().add(&Expr::constant(2.0, domain.clone()).unwrap()).unwrap().relu().unwrap().compile().unwrap()
}
fn our_chain(shape: Shape, domain: &Arc<Domain>) -> Program {
    let x = Expr::slot(0, shape, domain.clone()).unwrap();
    let y = Expr::slot(1, shape, domain.clone()).unwrap();
    let half = Expr::constant(0.5, domain.clone()).unwrap();
    let mut v = x.add(&y).unwrap();
    for operand in [&x, &y, &x, &y, &x] {
        v = v.mul(&half).unwrap().add(operand).unwrap().relu().unwrap();
    }
    v.compile().unwrap()
}
fn ours_elementwise(label: &str, make: fn(Shape, &Arc<Domain>) -> Program, a: &[u8], b: &[u8], n: usize) {
    let domain = Domain::new(1 << 16, 1 << 12);
    let budget = Budget::new(1 << 30);
    let mut out = vec![0u8; n * 4];
    for threads in THREADS {
        let pool = pinned_pool(threads);
        let part = n.div_ceil(threads);
        // A program per distinct part length; only the last part can be shorter.
        let full = make(Shape::Matrix(1, part), &domain);
        let tail = make(Shape::Matrix(1, n - part * (threads - 1).min(n / part)), &domain);
        row(&format!("ours_{label}_threads{threads}"), n, || {
            if threads == 1 {
                // On the caller's own pinned thread: a real caller pays no pool wakeup here.
                let views = [
                    View::contiguous(a, Shape::Matrix(1, n)).unwrap(),
                    View::contiguous(b, Shape::Matrix(1, n)).unwrap(),
                ];
                full.run_into(black_box(&views), black_box(&mut out), &budget).unwrap();
            } else {
                pool.install(|| {
                    out.par_chunks_mut(part * 4).enumerate().for_each(|(index, chunk)| {
                        let start = index * part * 4;
                        let len = chunk.len();
                        let shape = Shape::Matrix(1, len / 4);
                        let views = [
                            View::contiguous(&a[start..start + len], shape).unwrap(),
                            View::contiguous(&b[start..start + len], shape).unwrap(),
                        ];
                        let program = if len / 4 == part { &full } else { &tail };
                        program.run_into(&views, chunk, &budget).unwrap();
                    });
                });
            }
            black_box(&out);
        });
    }
    // The compile the executor needs, the same way the other side reports its first launch.
    let start = Instant::now();
    let program = make(Shape::Matrix(1, n), &domain);
    println!("ours_{label}_compile n={n} us={:.3}", start.elapsed().as_secs_f64() * 1e6);
    black_box(program);
}
fn ours() {
    pin_current(0);
    println!("# side=ours isa={:?} chunk={CHUNK} samples={SAMPLES}", Isa::detect());
    let domain = Domain::new(1 << 16, 1 << 12);
    let budget = Budget::new(1 << 30);
    for n in ELEMENTS {
        let (a, b) = fixture(n);
        let (ab, bb) = (bytes_of(&a), bytes_of(&b));
        ours_elementwise("workload", our_workload, &ab, &bb, n);
        ours_elementwise("chain16", our_chain, &ab, &bb, n);
        ceilings(&ab, &bb, n);
        // The strict check, on the inputs that expose rounding, reordering and edge handling.
        for edges in [false, true] {
            let (x, y) = (hard_fixture(n, 1, edges), hard_fixture(n, 2, edges));
            let (xb, yb) = (bytes_of(&x), bytes_of(&y));
            let shape = Shape::Matrix(1, n);
            let views = [View::contiguous(&xb, shape).unwrap(), View::contiguous(&yb, shape).unwrap()];
            let mut out = vec![0u8; n * 4];
            our_workload(shape, &domain).run_into(&views, &mut out, &budget).unwrap();
            let workload = differences(&floats(&out), &oracle_workload(&x, &y));
            our_chain(shape, &domain).run_into(&views, &mut out, &budget).unwrap();
            let chain = differences(&floats(&out), &oracle_chain(&x, &y));
            println!("ours_strict n={n} edges={edges} workload_differences={workload} chain_differences={chain}");
        }
    }
    for side in SIDES {
        let shape = Shape::Matrix(side, side);
        let (a, b) = (hard_fixture(side * side, 3, false), hard_fixture(side * side, 4, false));
        let (ab, bb) = (bytes_of(&a), bytes_of(&b));
        let ops = side * side * side;
        let mut out = vec![0u8; side * side * 4];
        {
            let (x, y) = (ab.as_chunks::<4>().0, bb.as_chunks::<4>().0);
            let mut fused = vec![[0u8; 4]; side * side];
            assert!(std::is_x86_feature_detected!("fma"));
            // SAFETY: AVX2 and FMA are detected at the start of `ceilings`, and FMA just above.
            row("rust_matmul_fma_threads1", ops, || unsafe {
                matmul_fma(black_box(x), black_box(y), side, black_box(&mut fused));
            });
        }
        for threads in THREADS {
            let pool = pinned_pool(threads);
            let rows = side.div_ceil(threads);
            let tail_rows = side - rows * (threads - 1).min(side / rows);
            let make = |r: usize| {
                Expr::slot(0, Shape::Matrix(r, side), domain.clone())
                    .unwrap()
                    .matmul(&Expr::slot(1, shape, domain.clone()).unwrap())
                    .unwrap()
                    .compile()
                    .unwrap()
            };
            let (full, tail) = (make(rows), make(tail_rows.max(1)));
            row(&format!("ours_matmul_threads{threads}"), ops, || {
                if threads == 1 {
                    let views = [View::contiguous(&ab, shape).unwrap(), View::contiguous(&bb, shape).unwrap()];
                    full.run_into(black_box(&views), black_box(&mut out), &budget).unwrap();
                    return;
                }
                pool.install(|| {
                    out.par_chunks_mut(rows * side * 4).enumerate().for_each(|(index, chunk)| {
                        let r = chunk.len() / (side * 4);
                        let start = index * rows * side * 4;
                        let views = [
                            View::contiguous(&ab[start..start + chunk.len()], Shape::Matrix(r, side)).unwrap(),
                            View::contiguous(&bb, shape).unwrap(),
                        ];
                        let program = if r == rows { &full } else { &tail };
                        program.run_into(&views, chunk, &budget).unwrap();
                    });
                });
                black_box(&out);
            });
            println!(
                "ours_matmul_strict side={side} threads={threads} differences={}",
                differences(&floats(&out), &oracle_matmul(&a, &b, side))
            );
        }
    }
}

// ---------------------------------------------------------------------------------------------
// CubeCL's side.
// ---------------------------------------------------------------------------------------------
type Launch = unsafe fn(&Client, CubeDim, usize, Handle, Handle, Handle, usize, usize);

macro_rules! launcher {
    ($name:ident, $kernel:ident) => {
        unsafe fn $name(
            client: &Client,
            dim: CubeDim,
            lanes: usize,
            a: Handle,
            b: Handle,
            out: Handle,
            count: usize,
            threads: usize,
        ) {
            // SAFETY: the caller sized all three buffers for `count` vectors of `lanes`.
            unsafe {
                $kernel::launch_unchecked(
                    client,
                    CubeCount::Static(1, 1, 1),
                    dim,
                    lanes,
                    BufferArg::from_raw_parts(a, count * lanes),
                    BufferArg::from_raw_parts(b, count * lanes),
                    BufferArg::from_raw_parts(out, count * lanes),
                    count,
                    threads,
                );
            }
        }
    };
}
launcher!(launch_workload, workload);
launcher!(launch_workload_fast, workload_fast);
launcher!(launch_chain, chain16);

fn sync(client: &Client) {
    cubecl::future::block_on(client.sync()).unwrap();
}

fn cubecl_elementwise(client: &Client, label: &str, launch: Launch, oracle: fn(&[f32], &[f32]) -> Vec<f32>, n: usize) {
    let (a, b) = fixture(n);
    let (ha, hb) = (client.create_from_slice(f32::as_bytes(&a)), client.create_from_slice(f32::as_bytes(&b)));
    let out = client.empty(n * 4);
    let (x, y) = (hard_fixture(n, 1, false), hard_fixture(n, 2, false));
    let (hx, hy) = (client.create_from_slice(f32::as_bytes(&x)), client.create_from_slice(f32::as_bytes(&y)));
    let (ex, ey) = (hard_fixture(n, 1, true), hard_fixture(n, 2, true));
    let (hex, hey) = (client.create_from_slice(f32::as_bytes(&ex)), client.create_from_slice(f32::as_bytes(&ey)));
    for lanes in VERSIONS {
        if n % lanes != 0 {
            continue;
        }
        let count = n / lanes;
        for threads in THREADS {
            let dim = CubeDim::new_1d(threads as u32);
            let go = |p: &Handle, q: &Handle| {
                // SAFETY: every buffer here holds `n` f32 elements, `count * lanes == n`.
                unsafe { launch(client, dim, lanes, p.clone(), q.clone(), out.clone(), count, threads) }
            };
            let start = Instant::now();
            go(&ha, &hb);
            sync(client);
            println!(
                "cubecl_{label}_first_launch lanes={lanes} threads={threads} n={n} us={:.3}",
                start.elapsed().as_secs_f64() * 1e6
            );
            row(&format!("cubecl_{label}_lanes{lanes}_threads{threads}"), n, || {
                go(black_box(&ha), black_box(&hb));
                sync(client);
            });
            let result = client.read_one_unchecked(out.clone());
            let correct = differences(&floats(&result), &oracle(&a, &b));
            go(&hx, &hy);
            let hard = client.read_one_unchecked(out.clone());
            let hard = differences(&floats(&hard), &oracle(&x, &y));
            go(&hex, &hey);
            let edge = client.read_one_unchecked(out.clone());
            let edge = differences(&floats(&edge), &oracle(&ex, &ey));
            println!(
                "cubecl_{label}_strict lanes={lanes} threads={threads} n={n} plain_differences={correct} rounding_differences={hard} edge_differences={edge}"
            );
        }
    }
}

fn cubecl_side() {
    let start = Instant::now();
    let client = CpuRuntime::client(&CpuDevice);
    println!("# side=cubecl version=0.11.3 samples={SAMPLES}");
    println!("cubecl_client_init us={:.3}", start.elapsed().as_secs_f64() * 1e6);
    for n in ELEMENTS {
        cubecl_elementwise(&client, "workload", launch_workload, oracle_workload, n);
        cubecl_elementwise(&client, "workload_fastmath", launch_workload_fast, oracle_workload, n);
        cubecl_elementwise(&client, "chain16", launch_chain, oracle_chain, n);
        // The path a JSL host must take: copy both inputs in, dispatch, read the result back.
        let (a, b) = fixture(n);
        let (ab, bb) = (f32::as_bytes(&a), f32::as_bytes(&b));
        if n % 8 == 0 {
            // Fusion defeated, so this is the strict computation.
            let (x, y) = (hard_fixture(n, 5, false), hard_fixture(n, 6, false));
            let (hx, hy) = (client.create_from_slice(f32::as_bytes(&x)), client.create_from_slice(f32::as_bytes(&y)));
            let out = client.empty(n * 4);
            for threads in THREADS {
                let go = || {
                    // SAFETY: all three buffers hold `n` scalars, `n / 8` vectors of eight.
                    unsafe {
                        chain16_strict::launch_unchecked(
                            &client,
                            CubeCount::Static(1, 1, 1),
                            CubeDim::new_1d(threads as u32),
                            8,
                            BufferArg::from_raw_parts(hx.clone(), n),
                            BufferArg::from_raw_parts(hy.clone(), n),
                            BufferArg::from_raw_parts(out.clone(), n),
                            n / 8,
                            threads,
                            0,
                        );
                    }
                };
                row(&format!("cubecl_chain16_nofma_lanes8_threads{threads}"), n, || {
                    go();
                    sync(&client);
                });
                let result = floats(&client.read_one_unchecked(out.clone()));
                println!(
                    "cubecl_chain16_nofma_strict n={n} threads={threads} differences={}",
                    differences(&result, &oracle_chain(&x, &y))
                );
            }
            // The same transfer path with persistent handles: no allocation, the copies remain.
            let (ha, hb, hout) = (client.empty(n * 4), client.empty(n * 4), client.empty(n * 4));
            row("cubecl_workload_write_dispatch_read_persistent_lanes8", n, || {
                client.write(&ha, Bytes::from_elems(a.clone()));
                client.write(&hb, Bytes::from_elems(b.clone()));
                // SAFETY: as above.
                unsafe {
                    launch_workload(&client, CubeDim::new_1d(1), 8, ha.clone(), hb.clone(), hout.clone(), n / 8, 1);
                }
                black_box(client.read_one_unchecked(hout.clone()));
            });
            row("cubecl_workload_copy_in_dispatch_readback_lanes8", n, || {
                let ha = client.create_from_slice(black_box(ab));
                let hb = client.create_from_slice(black_box(bb));
                let out = client.empty(n * 4);
                // SAFETY: as above.
                unsafe { launch_workload(&client, CubeDim::new_1d(1), 8, ha, hb, out.clone(), n / 8, 1) };
                black_box(client.read_one_unchecked(out));
            });
        }
    }
    for side in SIDES {
        let (a, b) = (hard_fixture(side * side, 3, false), hard_fixture(side * side, 4, false));
        let expected = oracle_matmul(&a, &b, side);
        let (ha, hb) = (client.create_from_slice(f32::as_bytes(&a)), client.create_from_slice(f32::as_bytes(&b)));
        let out = client.empty(side * side * 4);
        let ops = side * side * side;
        for threads in THREADS {
            let go = || {
                // SAFETY: `a` holds side^2 scalars; `b` and `out` hold side rows of side/8 vectors.
                unsafe {
                    matmul_tuned_strict::launch_unchecked(
                        &client,
                        CubeCount::Static(1, 1, 1),
                        CubeDim::new_1d(threads as u32),
                        8,
                        BufferArg::from_raw_parts(ha.clone(), side * side),
                        BufferArg::from_raw_parts(hb.clone(), side * side),
                        BufferArg::from_raw_parts(out.clone(), side * side),
                        side,
                        side,
                        side / 8,
                        threads,
                        0,
                    );
                }
            };
            row(&format!("cubecl_matmul_tuned_nofma_lanes8_threads{threads}"), ops, || {
                go();
                sync(&client);
            });
            let result = floats(&client.read_one_unchecked(out.clone()));
            println!(
                "cubecl_matmul_nofma_strict side={side} threads={threads} differences={}",
                differences(&result, &expected)
            );
        }
        for lanes in VERSIONS {
            if side % lanes != 0 {
                continue;
            }
            for tuned in [false, true] {
                if tuned && side % 4 != 0 {
                    continue;
                }
                for threads in THREADS {
                    let dim = CubeDim::new_1d(threads as u32);
                    let v = side / lanes;
                    let go = || {
                        // SAFETY: `a` holds side^2 scalars; `b` and `out` hold side rows of `v` vectors.
                        unsafe {
                            let (x, y, z) = (
                                BufferArg::from_raw_parts(ha.clone(), side * side),
                                BufferArg::from_raw_parts(hb.clone(), side * side),
                                BufferArg::from_raw_parts(out.clone(), side * side),
                            );
                            let count = CubeCount::Static(1, 1, 1);
                            if tuned {
                                matmul_tuned::launch_unchecked(
                                    &client, count, dim, lanes, x, y, z, side, side, v, threads,
                                );
                            } else {
                                matmul_simple::launch_unchecked(
                                    &client, count, dim, lanes, x, y, z, side, side, v, threads,
                                );
                            }
                        }
                    };
                    let name = if tuned { "tuned" } else { "simple" };
                    let start = Instant::now();
                    go();
                    sync(&client);
                    println!(
                        "cubecl_matmul_{name}_first_launch side={side} lanes={lanes} threads={threads} us={:.3}",
                        start.elapsed().as_secs_f64() * 1e6
                    );
                    row(&format!("cubecl_matmul_{name}_lanes{lanes}_threads{threads}"), ops, || {
                        go();
                        sync(&client);
                    });
                    let result = client.read_one_unchecked(out.clone());
                    println!(
                        "cubecl_matmul_{name}_strict side={side} lanes={lanes} threads={threads} differences={}",
                        differences(&floats(&result), &expected)
                    );
                }
            }
        }
    }
}

/// Identifies what semantics a differing matmul result has, by comparing against candidates.
fn diagnose() {
    let client = CpuRuntime::client(&CpuDevice);
    for side in [8usize, 64] {
        let (a, b) = (hard_fixture(side * side, 3, false), hard_fixture(side * side, 4, false));
        let (ha, hb) = (client.create_from_slice(f32::as_bytes(&a)), client.create_from_slice(f32::as_bytes(&b)));
        let out = client.empty(side * side * 4);
        // SAFETY: all three buffers hold side^2 scalars.
        unsafe {
            matmul_simple::launch_unchecked(
                &client,
                CubeCount::Static(1, 1, 1),
                CubeDim::new_1d(1),
                1,
                BufferArg::from_raw_parts(ha, side * side),
                BufferArg::from_raw_parts(hb, side * side),
                BufferArg::from_raw_parts(out.clone(), side * side),
                side,
                side,
                side,
                1,
            );
        }
        let actual = floats(&client.read_one_unchecked(out));
        let candidate = |step: fn(f32, f32, f32) -> f32| {
            let mut result = vec![0.0f32; side * side];
            for i in 0..side {
                for j in 0..side {
                    let mut sum = 0.0f32;
                    for p in 0..side {
                        sum = step(sum, a[i * side + p], b[p * side + j]);
                    }
                    result[i * side + j] = sum;
                }
            }
            result
        };
        let strict = candidate(|s, x, y| s + x * y);
        let fused = candidate(|s, x, y| x.mul_add(y, s));
        let mut wide = vec![0.0f32; side * side];
        for i in 0..side {
            for j in 0..side {
                let mut sum = 0.0f64;
                for p in 0..side {
                    sum += f64::from(a[i * side + p]) * f64::from(b[p * side + j]);
                }
                wide[i * side + j] = sum as f32;
            }
        }
        let worst = actual
            .iter()
            .zip(&strict)
            .map(|(x, y)| (i64::from(x.to_bits() as i32) - i64::from(y.to_bits() as i32)).abs())
            .max()
            .unwrap();
        println!(
            "diagnose side={side} vs_strict={} vs_fma={} vs_f64_accumulate={} worst_ulps_from_strict={worst} first_actual={:?} first_strict={:?} first_fma={:?}",
            differences(&actual, &strict),
            differences(&actual, &fused),
            differences(&actual, &wide),
            actual[0],
            strict[0],
            fused[0],
        );
    }
    // Elementwise: the same fusion in `a * b + 2`, against strict and fused scalar results.
    let n = 4096;
    let (a, b) = (hard_fixture(n, 7, false), hard_fixture(n, 8, false));
    let (ha, hb) = (client.create_from_slice(f32::as_bytes(&a)), client.create_from_slice(f32::as_bytes(&b)));
    let out = client.empty(n * 4);
    // SAFETY: all three buffers hold `n` scalars.
    unsafe {
        product_add::launch_unchecked(
            &client,
            CubeCount::Static(1, 1, 1),
            CubeDim::new_1d(1),
            8,
            BufferArg::from_raw_parts(ha, n),
            BufferArg::from_raw_parts(hb, n),
            BufferArg::from_raw_parts(out.clone(), n),
            n / 8,
        );
    }
    let actual = floats(&client.read_one_unchecked(out));
    let strict: Vec<f32> = a.iter().zip(&b).map(|(x, y)| x * y + 2.0).collect();
    let fused: Vec<f32> = a.iter().zip(&b).map(|(x, y)| x.mul_add(*y, 2.0)).collect();
    println!(
        "diagnose elementwise product_add vs_strict={} vs_fma={} fixture_distinguishes={}",
        differences(&actual, &strict),
        differences(&actual, &fused),
        differences(&strict, &fused),
    );
    println!("host_has_fma={}", std::is_x86_feature_detected!("fma"));
}

fn main() {
    match std::env::args().nth(1).as_deref() {
        Some("diagnose") => diagnose(),
        Some("ours") => ours(),
        Some("cubecl") => cubecl_side(),
        _ => {
            eprintln!("usage: l3i-cubecl-compare ours|cubecl");
            std::process::exit(2);
        }
    }
}
