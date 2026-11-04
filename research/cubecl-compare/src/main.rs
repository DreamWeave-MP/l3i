//! Head to head: the chunked kernels against CubeCL 0.11's CPU backend, on one set of inputs.
//!
//! The metric is retired user-space instructions across every thread of the process, from the
//! CPU's own counter, never time. Run inside a disposable container (see `run-in-container.sh`).
//! Each side runs in its own process. CubeCL gets its best configuration: `Vector` kernels at
//! several widths, `UNIT_POS` partitioning, and the same matmul structure as the native kernel.
//! Kernel-only rows reuse preallocated storage and wait on `sync`, excluding upload and readback.
//!
//! What this metric cannot see: memory stalls, port pressure and parallel speedup. A threaded row
//! is total work including any spinning, not how soon the work finished.
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
    clippy::cast_sign_loss,
    clippy::type_complexity,
    clippy::semicolon_if_nothing_returned
)]
use cubecl::bytes::Bytes;
use cubecl::cpu::{CpuDevice, CpuRuntime};
use cubecl::prelude::*;
use cubecl_runtime::runtime::Runtime;
use cubecl_runtime::server::Handle;
use l3i_graph_cpu_research::{Budget, Domain, Expr, Isa, Numeric, Shape, View};
use rayon::prelude::*;
use std::hint::black_box;
use std::sync::{Arc, OnceLock};

const SAMPLES: usize = 15;
const WARMUP: usize = 3;
const VERSIONS: [usize; 3] = [1, 8, 16];
const THREADS: [usize; 2] = [1, 8];
const ELEMENTS: [usize; 5] = [16, 1024, 16384, 65536, 1 << 20];
const SIDES: [usize; 3] = [48, 240, 384];
const MLP_SIDES: [usize; 2] = [48, 240];
const COEFFICIENTS: [f32; 7] = [0.3, -0.7, 0.11, 0.5, -0.25, 0.9, 0.13];

// ---------------------------------------------------------------------------------------------
// The counter: `perf_event_open`, one hardware counter inherited by every later thread.
// ---------------------------------------------------------------------------------------------
#[repr(C)]
struct PerfEventAttr {
    kind: u32,
    size: u32,
    config: u64,
    rest: [u64; 13],
}
struct Counter(i32);
impl Counter {
    fn open() -> Self {
        // `disabled | inherit | exclude_kernel | exclude_hv`.
        let flags = 1u64 | (1 << 1) | (1 << 5) | (1 << 6);
        let mut rest = [0u64; 13];
        rest[3] = flags; // sample_period, sample_type, read_format, then the flag word.
        let mut attr = PerfEventAttr { kind: 0, size: std::mem::size_of::<PerfEventAttr>() as u32, config: 1, rest };
        // SAFETY: a well-formed attribute block, for this process on any CPU.
        let fd = unsafe { libc::syscall(libc::SYS_perf_event_open, &raw mut attr, 0, -1, -1, 0u64) };
        assert!(fd >= 0, "perf_event_open failed: {}", std::io::Error::last_os_error());
        Self(fd as i32)
    }
    fn control(&self, request: libc::c_ulong) {
        // SAFETY: a perf descriptor this process owns, and an argumentless request.
        assert_eq!(unsafe { libc::ioctl(self.0, request, 0) }, 0);
    }
    fn read(&self) -> u64 {
        let mut value = 0u64;
        // SAFETY: reads eight bytes into a local u64.
        assert_eq!(unsafe { libc::read(self.0, (&raw mut value).cast(), 8) }, 8);
        value
    }
}
static COUNTER: OnceLock<Counter> = OnceLock::new();
const ENABLE: libc::c_ulong = 0x2400;
const DISABLE: libc::c_ulong = 0x2401;
/// The instructions `body` retires across every thread.
fn count(body: impl FnOnce()) -> u64 {
    // A difference, because an exited thread's count folds into a total RESET does not clear.
    let counter = COUNTER.get().expect("opened first thing in main");
    let before = counter.read();
    counter.control(ENABLE);
    body();
    counter.control(DISABLE);
    counter.read() - before
}
fn row(label: &str, n: usize, mut body: impl FnMut()) {
    let empty = {
        let mut samples: Vec<u64> = (0..SAMPLES).map(|_| count(|| {})).collect();
        samples.sort_unstable();
        samples[SAMPLES / 2]
    };
    for _ in 0..WARMUP {
        body();
    }
    let mut samples: Vec<u64> = (0..SAMPLES).map(|_| count(&mut body).saturating_sub(empty)).collect();
    samples.sort_unstable();
    println!(
        "{label:<46} n={n:<9} median={:<12} min={:<12} max={:<12} per_element={:.3}",
        samples[SAMPLES / 2],
        samples[0],
        samples[SAMPLES - 1],
        samples[SAMPLES / 2] as f64 / n as f64
    );
}

// ---------------------------------------------------------------------------------------------
// CubeCL kernels. `_nofma` variants give each product a second use, never taken when `guard` is 0,
// because CubeCL's optimizer fuses every product whose only use is an add. LLVM has no contraction
// flag to fuse them later, so those variants are strict; the others are CubeCL's default.
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
    for i in (unit * count / threads)..((unit + 1) * count / threads) {
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
    let half = Vector::new(0.3f32);
    for i in (unit * count / threads)..((unit + 1) * count / threads) {
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

#[cube(launch_unchecked)]
fn chain16_nofma<N: Size>(
    a: &[Vector<f32, N>],
    b: &[Vector<f32, N>],
    out: &mut [Vector<f32, N>],
    count: usize,
    threads: usize,
    guard: usize,
) {
    let unit = UNIT_POS as usize;
    let half = Vector::new(0.3f32);
    for i in (unit * count / threads)..((unit + 1) * count / threads) {
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
            out[0] = p0 / p1 / p2 / p3 / p4;
        }
        out[i] = v;
    }
}

/// A degree-eight polynomial by Horner's rule, the shape of an activation approximation.
#[cube(launch_unchecked)]
fn horner<N: Size>(
    a: &[Vector<f32, N>],
    b: &[Vector<f32, N>],
    out: &mut [Vector<f32, N>],
    count: usize,
    threads: usize,
) {
    let unit = UNIT_POS as usize;
    for i in (unit * count / threads)..((unit + 1) * count / threads) {
        let x = a[i];
        let mut v = x * Vector::new(0.37f32) + b[i];
        v = v * x + Vector::new(0.3f32);
        v = v * x + Vector::new(-0.7f32);
        v = v * x + Vector::new(0.11f32);
        v = v * x + Vector::new(0.5f32);
        v = v * x + Vector::new(-0.25f32);
        v = v * x + Vector::new(0.9f32);
        v = v * x + Vector::new(0.13f32);
        out[i] = v;
    }
}

#[cube(launch_unchecked)]
fn horner_nofma<N: Size>(
    a: &[Vector<f32, N>],
    b: &[Vector<f32, N>],
    out: &mut [Vector<f32, N>],
    count: usize,
    threads: usize,
    guard: usize,
) {
    let unit = UNIT_POS as usize;
    for i in (unit * count / threads)..((unit + 1) * count / threads) {
        let x = a[i];
        let p0 = x * Vector::new(0.37f32);
        let mut v = p0 + b[i];
        let p1 = v * x;
        v = p1 + Vector::new(0.3f32);
        let p2 = v * x;
        v = p2 + Vector::new(-0.7f32);
        let p3 = v * x;
        v = p3 + Vector::new(0.11f32);
        let p4 = v * x;
        v = p4 + Vector::new(0.5f32);
        let p5 = v * x;
        v = p5 + Vector::new(-0.25f32);
        let p6 = v * x;
        v = p6 + Vector::new(0.9f32);
        let p7 = v * x;
        v = p7 + Vector::new(0.13f32);
        if guard == 1 {
            out[0] = p0 / p1 / p2 / p3 / p4 / p5 / p6 / p7;
        }
        out[i] = v;
    }
}

/// A residual-shaped DAG: `y` is read three times, so a linear chain does not describe it.
#[cube(launch_unchecked)]
fn residual<N: Size>(
    a: &[Vector<f32, N>],
    b: &[Vector<f32, N>],
    out: &mut [Vector<f32, N>],
    count: usize,
    threads: usize,
) {
    let unit = UNIT_POS as usize;
    for i in (unit * count / threads)..((unit + 1) * count / threads) {
        let x = a[i];
        let w = b[i];
        let y = relu_lanes::<N>(x * Vector::new(0.3f32) + w);
        out[i] = relu_lanes::<N>(y * y + y * w + x);
    }
}

#[cube(launch_unchecked)]
fn residual_nofma<N: Size>(
    a: &[Vector<f32, N>],
    b: &[Vector<f32, N>],
    out: &mut [Vector<f32, N>],
    count: usize,
    threads: usize,
    guard: usize,
) {
    let unit = UNIT_POS as usize;
    for i in (unit * count / threads)..((unit + 1) * count / threads) {
        let x = a[i];
        let w = b[i];
        let p0 = x * Vector::new(0.3f32);
        let y = relu_lanes::<N>(p0 + w);
        let p1 = y * y;
        let p2 = y * w;
        if guard == 1 {
            out[0] = p0 / p1 / p2;
        }
        out[i] = relu_lanes::<N>(p1 + p2 + x);
    }
}

/// `relu(t + 0.1)`, the elementwise stage of the two-layer network.
#[cube(launch_unchecked)]
fn shift_relu<N: Size>(t: &[Vector<f32, N>], out: &mut [Vector<f32, N>], count: usize, threads: usize) {
    let unit = UNIT_POS as usize;
    for i in (unit * count / threads)..((unit + 1) * count / threads) {
        out[i] = relu_lanes::<N>(t[i] + Vector::new(0.1f32));
    }
}

/// The native kernel's structure: `b` rows in ascending blocks of 64, a block of four output rows
/// and two vectors held in registers across them. Default: fused by CubeCL's optimizer.
#[cube(launch_unchecked)]
fn matmul_micro<N: Size>(
    a: &[f32],
    b: &[Vector<f32, N>],
    out: &mut [Vector<f32, N>],
    m: usize,
    k: usize,
    v: usize,
    threads: usize,
) {
    let unit = UNIT_POS as usize;
    let start = unit * (m / 4) / threads * 4;
    let end = (unit + 1) * (m / 4) / threads * 4;
    for i in start..end {
        for j in 0..v {
            out[i * v + j] = Vector::new(0.0f32);
        }
    }
    let blocks = (k + 63) / 64;
    for block in 0..blocks {
        let first = block * 64;
        let last = min(first + 64, k);
        for j2 in 0..(v / 2) {
            let j = j2 * 2;
            for i4 in (start / 4)..(end / 4) {
                let i = i4 * 4;
                let mut r00 = out[i * v + j];
                let mut r01 = out[i * v + j + 1];
                let mut r10 = out[(i + 1) * v + j];
                let mut r11 = out[(i + 1) * v + j + 1];
                let mut r20 = out[(i + 2) * v + j];
                let mut r21 = out[(i + 2) * v + j + 1];
                let mut r30 = out[(i + 3) * v + j];
                let mut r31 = out[(i + 3) * v + j + 1];
                for p in first..last {
                    let b0 = b[p * v + j];
                    let b1 = b[p * v + j + 1];
                    let s0 = Vector::new(a[i * k + p]);
                    let s1 = Vector::new(a[(i + 1) * k + p]);
                    let s2 = Vector::new(a[(i + 2) * k + p]);
                    let s3 = Vector::new(a[(i + 3) * k + p]);
                    r00 = r00 + s0 * b0;
                    r01 = r01 + s0 * b1;
                    r10 = r10 + s1 * b0;
                    r11 = r11 + s1 * b1;
                    r20 = r20 + s2 * b0;
                    r21 = r21 + s2 * b1;
                    r30 = r30 + s3 * b0;
                    r31 = r31 + s3 * b1;
                }
                out[i * v + j] = r00;
                out[i * v + j + 1] = r01;
                out[(i + 1) * v + j] = r10;
                out[(i + 1) * v + j + 1] = r11;
                out[(i + 2) * v + j] = r20;
                out[(i + 2) * v + j + 1] = r21;
                out[(i + 3) * v + j] = r30;
                out[(i + 3) * v + j + 1] = r31;
            }
        }
    }
}

/// `matmul_micro` with every product given a second use, so it stays strict.
#[cube(launch_unchecked)]
fn matmul_micro_nofma<N: Size>(
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
    let start = unit * (m / 4) / threads * 4;
    let end = (unit + 1) * (m / 4) / threads * 4;
    for i in start..end {
        for j in 0..v {
            out[i * v + j] = Vector::new(0.0f32);
        }
    }
    let blocks = (k + 63) / 64;
    for block in 0..blocks {
        let first = block * 64;
        let last = min(first + 64, k);
        for j2 in 0..(v / 2) {
            let j = j2 * 2;
            for i4 in (start / 4)..(end / 4) {
                let i = i4 * 4;
                let mut r00 = out[i * v + j];
                let mut r01 = out[i * v + j + 1];
                let mut r10 = out[(i + 1) * v + j];
                let mut r11 = out[(i + 1) * v + j + 1];
                let mut r20 = out[(i + 2) * v + j];
                let mut r21 = out[(i + 2) * v + j + 1];
                let mut r30 = out[(i + 3) * v + j];
                let mut r31 = out[(i + 3) * v + j + 1];
                for p in first..last {
                    let b0 = b[p * v + j];
                    let b1 = b[p * v + j + 1];
                    let s0 = Vector::new(a[i * k + p]);
                    let s1 = Vector::new(a[(i + 1) * k + p]);
                    let s2 = Vector::new(a[(i + 2) * k + p]);
                    let s3 = Vector::new(a[(i + 3) * k + p]);
                    let (p0, p1, p2, p3) = (s0 * b0, s0 * b1, s1 * b0, s1 * b1);
                    let (p4, p5, p6, p7) = (s2 * b0, s2 * b1, s3 * b0, s3 * b1);
                    if guard == 1 {
                        out[0] = p0 / p1 / p2 / p3 / p4 / p5 / p6 / p7;
                    }
                    r00 = r00 + p0;
                    r01 = r01 + p1;
                    r10 = r10 + p2;
                    r11 = r11 + p3;
                    r20 = r20 + p4;
                    r21 = r21 + p5;
                    r30 = r30 + p6;
                    r31 = r31 + p7;
                }
                out[i * v + j] = r00;
                out[i * v + j + 1] = r01;
                out[(i + 1) * v + j] = r10;
                out[(i + 1) * v + j + 1] = r11;
                out[(i + 2) * v + j] = r20;
                out[(i + 2) * v + j + 1] = r21;
                out[(i + 3) * v + j] = r30;
                out[(i + 3) * v + j + 1] = r31;
            }
        }
    }
}

/// The six-row form of the kernels above, with blocks of 128: the same structure as the native kernel.
#[cube(launch_unchecked)]
fn matmul_micro6<N: Size>(
    a: &[f32],
    b: &[Vector<f32, N>],
    out: &mut [Vector<f32, N>],
    m: usize,
    k: usize,
    v: usize,
    threads: usize,
) {
    let unit = UNIT_POS as usize;
    let start = unit * (m / 6) / threads * 6;
    let end = (unit + 1) * (m / 6) / threads * 6;
    for i in start..end {
        for j in 0..v {
            out[i * v + j] = Vector::new(0.0f32);
        }
    }
    let blocks = (k + 127) / 128;
    for block in 0..blocks {
        let first = block * 128;
        let last = min(first + 128, k);
        for j2 in 0..(v / 2) {
            let j = j2 * 2;
            for i6 in (start / 6)..(end / 6) {
                let i = i6 * 6;
                let mut r00 = out[i * v + j];
                let mut r01 = out[i * v + j + 1];
                let mut r10 = out[(i + 1) * v + j];
                let mut r11 = out[(i + 1) * v + j + 1];
                let mut r20 = out[(i + 2) * v + j];
                let mut r21 = out[(i + 2) * v + j + 1];
                let mut r30 = out[(i + 3) * v + j];
                let mut r31 = out[(i + 3) * v + j + 1];
                let mut r40 = out[(i + 4) * v + j];
                let mut r41 = out[(i + 4) * v + j + 1];
                let mut r50 = out[(i + 5) * v + j];
                let mut r51 = out[(i + 5) * v + j + 1];
                for p in first..last {
                    let b0 = b[p * v + j];
                    let b1 = b[p * v + j + 1];
                    let s0 = Vector::new(a[i * k + p]);
                    let s1 = Vector::new(a[(i + 1) * k + p]);
                    let s2 = Vector::new(a[(i + 2) * k + p]);
                    let s3 = Vector::new(a[(i + 3) * k + p]);
                    let s4 = Vector::new(a[(i + 4) * k + p]);
                    let s5 = Vector::new(a[(i + 5) * k + p]);
                    r00 = r00 + s0 * b0;
                    r01 = r01 + s0 * b1;
                    r10 = r10 + s1 * b0;
                    r11 = r11 + s1 * b1;
                    r20 = r20 + s2 * b0;
                    r21 = r21 + s2 * b1;
                    r30 = r30 + s3 * b0;
                    r31 = r31 + s3 * b1;
                    r40 = r40 + s4 * b0;
                    r41 = r41 + s4 * b1;
                    r50 = r50 + s5 * b0;
                    r51 = r51 + s5 * b1;
                }
                out[i * v + j] = r00;
                out[i * v + j + 1] = r01;
                out[(i + 1) * v + j] = r10;
                out[(i + 1) * v + j + 1] = r11;
                out[(i + 2) * v + j] = r20;
                out[(i + 2) * v + j + 1] = r21;
                out[(i + 3) * v + j] = r30;
                out[(i + 3) * v + j + 1] = r31;
                out[(i + 4) * v + j] = r40;
                out[(i + 4) * v + j + 1] = r41;
                out[(i + 5) * v + j] = r50;
                out[(i + 5) * v + j + 1] = r51;
            }
        }
    }
}

#[cube(launch_unchecked)]
fn matmul_micro6_nofma<N: Size>(
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
    let start = unit * (m / 6) / threads * 6;
    let end = (unit + 1) * (m / 6) / threads * 6;
    for i in start..end {
        for j in 0..v {
            out[i * v + j] = Vector::new(0.0f32);
        }
    }
    let blocks = (k + 127) / 128;
    for block in 0..blocks {
        let first = block * 128;
        let last = min(first + 128, k);
        for j2 in 0..(v / 2) {
            let j = j2 * 2;
            for i6 in (start / 6)..(end / 6) {
                let i = i6 * 6;
                let mut r00 = out[i * v + j];
                let mut r01 = out[i * v + j + 1];
                let mut r10 = out[(i + 1) * v + j];
                let mut r11 = out[(i + 1) * v + j + 1];
                let mut r20 = out[(i + 2) * v + j];
                let mut r21 = out[(i + 2) * v + j + 1];
                let mut r30 = out[(i + 3) * v + j];
                let mut r31 = out[(i + 3) * v + j + 1];
                let mut r40 = out[(i + 4) * v + j];
                let mut r41 = out[(i + 4) * v + j + 1];
                let mut r50 = out[(i + 5) * v + j];
                let mut r51 = out[(i + 5) * v + j + 1];
                for p in first..last {
                    let b0 = b[p * v + j];
                    let b1 = b[p * v + j + 1];
                    let s0 = Vector::new(a[i * k + p]);
                    let s1 = Vector::new(a[(i + 1) * k + p]);
                    let s2 = Vector::new(a[(i + 2) * k + p]);
                    let s3 = Vector::new(a[(i + 3) * k + p]);
                    let s4 = Vector::new(a[(i + 4) * k + p]);
                    let s5 = Vector::new(a[(i + 5) * k + p]);
                    let (p00, p01) = (s0 * b0, s0 * b1);
                    let (p10, p11) = (s1 * b0, s1 * b1);
                    let (p20, p21) = (s2 * b0, s2 * b1);
                    let (p30, p31) = (s3 * b0, s3 * b1);
                    let (p40, p41) = (s4 * b0, s4 * b1);
                    let (p50, p51) = (s5 * b0, s5 * b1);
                    if guard == 1 {
                        out[0] = p00 / p01 / p10 / p11 / p20 / p21 / p30 / p31 / p40 / p41 / p50 / p51;
                    }
                    r00 = r00 + p00;
                    r01 = r01 + p01;
                    r10 = r10 + p10;
                    r11 = r11 + p11;
                    r20 = r20 + p20;
                    r21 = r21 + p21;
                    r30 = r30 + p30;
                    r31 = r31 + p31;
                    r40 = r40 + p40;
                    r41 = r41 + p41;
                    r50 = r50 + p50;
                    r51 = r51 + p51;
                }
                out[i * v + j] = r00;
                out[i * v + j + 1] = r01;
                out[(i + 1) * v + j] = r10;
                out[(i + 1) * v + j + 1] = r11;
                out[(i + 2) * v + j] = r20;
                out[(i + 2) * v + j + 1] = r21;
                out[(i + 3) * v + j] = r30;
                out[(i + 3) * v + j + 1] = r31;
                out[(i + 4) * v + j] = r40;
                out[(i + 4) * v + j + 1] = r41;
                out[(i + 5) * v + j] = r50;
                out[(i + 5) * v + j + 1] = r51;
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

// ---------------------------------------------------------------------------------------------
// Fixtures and independent scalar oracles.
// ---------------------------------------------------------------------------------------------
fn bytes_of(values: &[f32]) -> Vec<u8> {
    values.iter().flat_map(|v| v.to_le_bytes()).collect()
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
/// One element of a workload, with `fma` choosing how a product and its add round.
type Oracle = fn(f32, f32, fn(f32, f32, f32) -> f32) -> f32;
fn strict_step(a: f32, b: f32, c: f32) -> f32 {
    a * b + c
}
fn fused_step(a: f32, b: f32, c: f32) -> f32 {
    a.mul_add(b, c)
}
fn oracle_workload(x: f32, y: f32, _: fn(f32, f32, f32) -> f32) -> f32 {
    relu(x + y + 2.0)
}
fn oracle_chain(x: f32, y: f32, step: fn(f32, f32, f32) -> f32) -> f32 {
    let mut v = x + y;
    for operand in [x, y, x, y, x] {
        v = relu(step(v, 0.3, operand));
    }
    v
}
fn oracle_horner(x: f32, y: f32, step: fn(f32, f32, f32) -> f32) -> f32 {
    let mut v = step(x, 0.37, y);
    for c in COEFFICIENTS {
        v = step(v, x, c);
    }
    v
}
fn oracle_residual_strict(x: f32, w: f32, _: fn(f32, f32, f32) -> f32) -> f32 {
    let y = relu(x * 0.3 + w);
    relu(y * y + y * w + x)
}
fn apply(oracle: Oracle, a: &[f32], b: &[f32], step: fn(f32, f32, f32) -> f32) -> Vec<f32> {
    a.iter().zip(b).map(|(x, y)| oracle(*x, *y, step)).collect()
}
fn oracle_matmul(a: &[f32], b: &[f32], side: usize, step: fn(f32, f32, f32) -> f32) -> Vec<f32> {
    let mut out = vec![0.0f32; side * side];
    for i in 0..side {
        for j in 0..side {
            let mut sum = 0.0f32;
            for p in 0..side {
                sum = step(a[i * side + p], b[p * side + j], sum);
            }
            out[i * side + j] = sum;
        }
    }
    out
}
fn oracle_mlp(x: &[f32], w1: &[f32], w2: &[f32], side: usize) -> Vec<f32> {
    let hidden: Vec<f32> = oracle_matmul(x, w1, side, strict_step).iter().map(|v| relu(*v + 0.1)).collect();
    oracle_matmul(&hidden, w2, side, strict_step)
}
fn floats(bytes: &[u8]) -> Vec<f32> {
    bytes.as_chunks::<4>().0.iter().map(|b| f32::from_le_bytes(*b)).collect()
}
/// How many elements differ from an oracle, with any NaN equal to any NaN.
fn differences(actual: &[f32], expected: &[f32]) -> usize {
    assert_eq!(actual.len(), expected.len());
    actual.iter().zip(expected).filter(|(a, e)| !((a.is_nan() && e.is_nan()) || a.to_bits() == e.to_bits())).count()
}

// ---------------------------------------------------------------------------------------------
// Our side. Threads split the output through the public API, one program per part shape, on rayon.
// ---------------------------------------------------------------------------------------------
fn slots(shape: Shape, domain: &Arc<Domain>) -> (Expr, Expr) {
    (Expr::slot(0, shape, domain.clone()).unwrap(), Expr::slot(1, shape, domain.clone()).unwrap())
}
fn graph_workload(shape: Shape, domain: &Arc<Domain>) -> Expr {
    let (x, y) = slots(shape, domain);
    x.add(&y).unwrap().add(&Expr::constant(2.0, domain.clone()).unwrap()).unwrap().relu().unwrap()
}
fn graph_chain(shape: Shape, domain: &Arc<Domain>) -> Expr {
    let (x, y) = slots(shape, domain);
    let half = Expr::constant(0.3, domain.clone()).unwrap();
    let mut v = x.add(&y).unwrap();
    for operand in [&x, &y, &x, &y, &x] {
        v = v.mul(&half).unwrap().add(operand).unwrap().relu().unwrap();
    }
    v
}
fn graph_horner(shape: Shape, domain: &Arc<Domain>) -> Expr {
    let (x, y) = slots(shape, domain);
    let mut v = x.mul(&Expr::constant(0.37, domain.clone()).unwrap()).unwrap().add(&y).unwrap();
    for c in COEFFICIENTS {
        v = v.mul(&x).unwrap().add(&Expr::constant(c, domain.clone()).unwrap()).unwrap();
    }
    v
}
fn graph_residual(shape: Shape, domain: &Arc<Domain>) -> Expr {
    let (x, w) = slots(shape, domain);
    let y = x.mul(&Expr::constant(0.3, domain.clone()).unwrap()).unwrap().add(&w).unwrap().relu().unwrap();
    y.mul(&y).unwrap().add(&y.mul(&w).unwrap()).unwrap().add(&x).unwrap().relu().unwrap()
}
type Graph = fn(Shape, &Arc<Domain>) -> Expr;

fn pin_current(cpu: usize) {
    // SAFETY: a zeroed cpu_set_t is valid, and the index is taken modulo this host's CPU count.
    unsafe {
        let mut set: libc::cpu_set_t = std::mem::zeroed();
        libc::CPU_SET(cpu % 16, &mut set);
        libc::sched_setaffinity(0, std::mem::size_of::<libc::cpu_set_t>(), &raw const set);
    }
}
fn pinned_pool(threads: usize) -> rayon::ThreadPool {
    rayon::ThreadPoolBuilder::new().num_threads(threads).start_handler(pin_current).build().unwrap()
}

fn ours_elementwise(
    label: &str,
    graph: Graph,
    numeric: Numeric,
    oracle: Option<(Oracle, fn(f32, f32, f32) -> f32)>,
    n: usize,
) {
    let domain = Domain::new(1 << 16, 1 << 12);
    let budget = Budget::new(1 << 30);
    let (a, b) = (hard_fixture(n, 1, false), hard_fixture(n, 2, false));
    let (ab, bb) = (bytes_of(&a), bytes_of(&b));
    let mut out = vec![0u8; n * 4];
    for threads in THREADS {
        let pool = (threads > 1).then(|| pinned_pool(threads));
        let part = n.div_ceil(threads);
        let compile = |len: usize| graph(Shape::Matrix(1, len), &domain).compile_with(numeric).unwrap();
        let full = compile(part);
        let tail = compile(n - part * (threads - 1).min(n / part));
        row(&format!("ours_{label}_threads{threads}"), n, || {
            if threads == 1 {
                let shape = Shape::Matrix(1, n);
                let views = [View::contiguous(&ab, shape).unwrap(), View::contiguous(&bb, shape).unwrap()];
                full.run_into(black_box(&views), black_box(&mut out), &budget).unwrap();
            } else {
                pool.as_ref().expect("threaded rows have a pool").install(|| {
                    out.par_chunks_mut(part * 4).enumerate().for_each(|(index, chunk)| {
                        let start = index * part * 4;
                        let len = chunk.len();
                        let shape = Shape::Matrix(1, len / 4);
                        let views = [
                            View::contiguous(&ab[start..start + len], shape).unwrap(),
                            View::contiguous(&bb[start..start + len], shape).unwrap(),
                        ];
                        let program = if len / 4 == part { &full } else { &tail };
                        program.run_into(&views, chunk, &budget).unwrap();
                    });
                });
            }
            black_box(&out);
        });
        if let Some((oracle, step)) = oracle {
            println!(
                "ours_{label}_check threads={threads} n={n} differences={}",
                differences(&floats(&out), &apply(oracle, &a, &b, step))
            );
        }
    }
    println!(
        "ours_{label}_compile n={n} instructions={}",
        count(|| {
            black_box(graph(Shape::Matrix(1, n), &domain).compile_with(numeric).unwrap());
        })
    );
    // Edge values: strict programs must match on NaN, infinities, signed zero and subnormals.
    if numeric == Numeric::Strict {
        let (x, y) = (hard_fixture(n, 1, true), hard_fixture(n, 2, true));
        let (xb, yb) = (bytes_of(&x), bytes_of(&y));
        let shape = Shape::Matrix(1, n);
        let views = [View::contiguous(&xb, shape).unwrap(), View::contiguous(&yb, shape).unwrap()];
        graph(shape, &domain).compile().unwrap().run_into(&views, &mut out, &budget).unwrap();
        if let Some((oracle, step)) = oracle {
            println!(
                "ours_{label}_edges n={n} differences={}",
                differences(&floats(&out), &apply(oracle, &x, &y, step))
            );
        }
    }
}

fn ours() {
    pin_current(0);
    println!(
        "# side=ours isa={:?} fast={:?} metric=user_instructions_all_threads samples={SAMPLES}",
        Isa::detect(),
        Isa::detect_fast()
    );
    for n in ELEMENTS {
        ours_elementwise("workload", graph_workload, Numeric::Strict, Some((oracle_workload, strict_step)), n);
        ours_elementwise("chain16", graph_chain, Numeric::Strict, Some((oracle_chain, strict_step)), n);
        ours_elementwise("chain16_fast", graph_chain, Numeric::Fast, Some((oracle_chain, fused_step)), n);
        ours_elementwise("horner", graph_horner, Numeric::Strict, Some((oracle_horner, strict_step)), n);
        ours_elementwise("horner_fast", graph_horner, Numeric::Fast, Some((oracle_horner, fused_step)), n);
        ours_elementwise("residual", graph_residual, Numeric::Strict, Some((oracle_residual_strict, strict_step)), n);
        ours_elementwise("residual_fast", graph_residual, Numeric::Fast, None, n);
    }
    let domain = Domain::new(1 << 16, 1 << 12);
    let budget = Budget::new(1 << 30);
    for side in SIDES {
        let shape = Shape::Matrix(side, side);
        let (a, b) = (hard_fixture(side * side, 3, false), hard_fixture(side * side, 4, false));
        let (ab, bb) = (bytes_of(&a), bytes_of(&b));
        let ops = side * side * side;
        let mut out = vec![0u8; side * side * 4];
        for (name, numeric, step) in
            [("", Numeric::Strict, strict_step as fn(f32, f32, f32) -> f32), ("_fast", Numeric::Fast, fused_step)]
        {
            for threads in THREADS {
                let pool = (threads > 1).then(|| pinned_pool(threads));
                let rows = side.div_ceil(threads);
                let tail_rows = side - rows * (threads - 1).min(side / rows);
                let make = |r: usize| {
                    let left = Expr::slot(0, Shape::Matrix(r, side), domain.clone()).unwrap();
                    left.matmul(&Expr::slot(1, shape, domain.clone()).unwrap()).unwrap().compile_with(numeric).unwrap()
                };
                let (full, tail) = (make(rows), make(tail_rows.max(1)));
                row(&format!("ours_matmul{name}_threads{threads}"), ops, || {
                    if threads == 1 {
                        let views = [View::contiguous(&ab, shape).unwrap(), View::contiguous(&bb, shape).unwrap()];
                        full.run_into(black_box(&views), black_box(&mut out), &budget).unwrap();
                        return;
                    }
                    pool.as_ref().expect("threaded rows have a pool").install(|| {
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
                    "ours_matmul{name}_check side={side} threads={threads} differences={}",
                    differences(&floats(&out), &oracle_matmul(&a, &b, side, step))
                );
            }
        }
    }
    for side in MLP_SIDES {
        let shape = Shape::Matrix(side, side);
        let (x, w1, w2) = (
            hard_fixture(side * side, 5, false),
            hard_fixture(side * side, 6, false),
            hard_fixture(side * side, 7, false),
        );
        let (xb, w1b, w2b) = (bytes_of(&x), bytes_of(&w1), bytes_of(&w2));
        let slot = |id| Expr::slot(id, shape, domain.clone()).unwrap();
        let hidden = slot(0)
            .matmul(&slot(1))
            .unwrap()
            .add(&Expr::constant(0.1, domain.clone()).unwrap())
            .unwrap()
            .relu()
            .unwrap();
        let graph = hidden.matmul(&slot(2)).unwrap();
        let mut out = vec![0u8; side * side * 4];
        let views = [
            View::contiguous(&xb, shape).unwrap(),
            View::contiguous(&w1b, shape).unwrap(),
            View::contiguous(&w2b, shape).unwrap(),
        ];
        for (name, numeric) in [("", Numeric::Strict), ("_fast", Numeric::Fast)] {
            let program = graph.compile_with(numeric).unwrap();
            row(&format!("ours_mlp2{name}_threads1"), side * side * side * 2, || {
                program.run_into(black_box(&views), black_box(&mut out), &budget).unwrap();
            });
            if numeric == Numeric::Strict {
                println!(
                    "ours_mlp2_check side={side} differences={}",
                    differences(&floats(&out), &oracle_mlp(&x, &w1, &w2, side))
                );
            }
        }
    }
    assert_eq!(budget.used(), 0);
}

// ---------------------------------------------------------------------------------------------
// CubeCL's side.
// ---------------------------------------------------------------------------------------------
type Launch = unsafe fn(&Client, CubeDim, usize, Handle, Handle, Handle, usize, usize);

macro_rules! launcher {
    ($name:ident, $kernel:ident $(, $guard:literal)?) => {
        unsafe fn $name(client: &Client, dim: CubeDim, lanes: usize, a: Handle, b: Handle, out: Handle, count: usize, threads: usize) {
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
                    $($guard,)?
                );
            }
        }
    };
}
launcher!(launch_workload, workload);
launcher!(launch_chain, chain16);
launcher!(launch_chain_nofma, chain16_nofma, 0);
launcher!(launch_horner, horner);
launcher!(launch_horner_nofma, horner_nofma, 0);
launcher!(launch_residual, residual);
launcher!(launch_residual_nofma, residual_nofma, 0);

fn sync(client: &Client) {
    cubecl::future::block_on(client.sync()).unwrap();
}

fn cubecl_elementwise(
    client: &Client,
    label: &str,
    launch: Launch,
    oracle: Option<(Oracle, fn(f32, f32, f32) -> f32)>,
    n: usize,
) {
    let (a, b) = (hard_fixture(n, 1, false), hard_fixture(n, 2, false));
    let (ha, hb) = (client.create_from_slice(f32::as_bytes(&a)), client.create_from_slice(f32::as_bytes(&b)));
    let out = client.empty(n * 4);
    let (ex, ey) = (hard_fixture(n, 1, true), hard_fixture(n, 2, true));
    let (hex, hey) = (client.create_from_slice(f32::as_bytes(&ex)), client.create_from_slice(f32::as_bytes(&ey)));
    for lanes in VERSIONS {
        if n % lanes != 0 {
            continue;
        }
        let count_vectors = n / lanes;
        for threads in THREADS {
            let dim = CubeDim::new_1d(threads as u32);
            let go = |p: &Handle, q: &Handle| {
                // SAFETY: every buffer here holds `n` f32 elements, `count_vectors * lanes == n`.
                unsafe { launch(client, dim, lanes, p.clone(), q.clone(), out.clone(), count_vectors, threads) }
            };
            println!(
                "cubecl_{label}_first_launch lanes={lanes} threads={threads} n={n} instructions={}",
                count(|| {
                    go(&ha, &hb);
                    sync(client);
                })
            );
            row(&format!("cubecl_{label}_lanes{lanes}_threads{threads}"), n, || {
                go(black_box(&ha), black_box(&hb));
                sync(client);
            });
            if let Some((oracle, step)) = oracle {
                let result = floats(&client.read_one_unchecked(out.clone()));
                let plain = differences(&result, &apply(oracle, &a, &b, step));
                go(&hex, &hey);
                let edge = floats(&client.read_one_unchecked(out.clone()));
                let edge = differences(&edge, &apply(oracle, &ex, &ey, step));
                println!(
                    "cubecl_{label}_check lanes={lanes} threads={threads} n={n} differences={plain} edge_differences={edge}"
                );
            }
        }
    }
}

fn cubecl_side() {
    println!("# side=cubecl version=0.11.3 metric=user_instructions_all_threads samples={SAMPLES}");
    let mut client = None;
    println!("cubecl_client_init instructions={}", count(|| client = Some(CpuRuntime::client(&CpuDevice))));
    let client = client.expect("set above");
    for n in ELEMENTS {
        cubecl_elementwise(&client, "workload", launch_workload, Some((oracle_workload, strict_step)), n);
        cubecl_elementwise(&client, "chain16", launch_chain, Some((oracle_chain, fused_step)), n);
        cubecl_elementwise(&client, "chain16_nofma", launch_chain_nofma, Some((oracle_chain, strict_step)), n);
        cubecl_elementwise(&client, "horner", launch_horner, Some((oracle_horner, fused_step)), n);
        cubecl_elementwise(&client, "horner_nofma", launch_horner_nofma, Some((oracle_horner, strict_step)), n);
        cubecl_elementwise(&client, "residual", launch_residual, None, n);
        cubecl_elementwise(
            &client,
            "residual_nofma",
            launch_residual_nofma,
            Some((oracle_residual_strict, strict_step)),
            n,
        );
        // The path a JSL host must take: copy both inputs in, dispatch, read the result back.
        let (a, b) = (hard_fixture(n, 1, false), hard_fixture(n, 2, false));
        let (ab, bb) = (f32::as_bytes(&a), f32::as_bytes(&b));
        if n % 8 == 0 {
            let (ha, hb, hout) = (client.empty(n * 4), client.empty(n * 4), client.empty(n * 4));
            row("cubecl_workload_write_dispatch_read_persistent", n, || {
                client.write(&ha, Bytes::from_elems(a.clone()));
                client.write(&hb, Bytes::from_elems(b.clone()));
                // SAFETY: as above.
                unsafe {
                    launch_workload(&client, CubeDim::new_1d(1), 8, ha.clone(), hb.clone(), hout.clone(), n / 8, 1)
                };
                black_box(client.read_one_unchecked(hout.clone()));
            });
            row("cubecl_workload_copy_in_dispatch_readback_fresh", n, || {
                let ha = client.create_from_slice(black_box(ab));
                let hb = client.create_from_slice(black_box(bb));
                let out = client.empty(n * 4);
                // SAFETY: as above.
                unsafe { launch_workload(&client, CubeDim::new_1d(1), 8, ha, hb, out.clone(), n / 8, 1) };
                black_box(client.read_one_unchecked(out));
            });
        }
    }
    let matmul6 = |ha: &Handle, hb: &Handle, out: &Handle, side: usize, threads: usize, strict: bool| {
        // SAFETY: as for `matmul`.
        unsafe {
            let (x, y, z) = (
                BufferArg::from_raw_parts(ha.clone(), side * side),
                BufferArg::from_raw_parts(hb.clone(), side * side),
                BufferArg::from_raw_parts(out.clone(), side * side),
            );
            let (dim, cubes) = (CubeDim::new_1d(threads as u32), CubeCount::Static(1, 1, 1));
            if strict {
                matmul_micro6_nofma::launch_unchecked(
                    &client,
                    cubes,
                    dim,
                    8,
                    x,
                    y,
                    z,
                    side,
                    side,
                    side / 8,
                    threads,
                    0,
                );
            } else {
                matmul_micro6::launch_unchecked(&client, cubes, dim, 8, x, y, z, side, side, side / 8, threads);
            }
        }
    };
    let matmul = |ha: &Handle, hb: &Handle, out: &Handle, side: usize, threads: usize, strict: bool| {
        // SAFETY: `a` holds side^2 scalars; `b` and `out` hold side rows of side/8 eight-lane vectors.
        unsafe {
            let (x, y, z) = (
                BufferArg::from_raw_parts(ha.clone(), side * side),
                BufferArg::from_raw_parts(hb.clone(), side * side),
                BufferArg::from_raw_parts(out.clone(), side * side),
            );
            let (dim, cubes) = (CubeDim::new_1d(threads as u32), CubeCount::Static(1, 1, 1));
            if strict {
                matmul_micro_nofma::launch_unchecked(&client, cubes, dim, 8, x, y, z, side, side, side / 8, threads, 0);
            } else {
                matmul_micro::launch_unchecked(&client, cubes, dim, 8, x, y, z, side, side, side / 8, threads);
            }
        }
    };
    for side in SIDES {
        let (a, b) = (hard_fixture(side * side, 3, false), hard_fixture(side * side, 4, false));
        let (ha, hb) = (client.create_from_slice(f32::as_bytes(&a)), client.create_from_slice(f32::as_bytes(&b)));
        let out = client.empty(side * side * 4);
        for (name, strict, step) in
            [("nofma", true, strict_step as fn(f32, f32, f32) -> f32), ("default", false, fused_step)]
        {
            for threads in THREADS {
                println!(
                    "cubecl_matmul_{name}_first_launch side={side} threads={threads} instructions={}",
                    count(|| {
                        matmul(&ha, &hb, &out, side, threads, strict);
                        sync(&client);
                    })
                );
                row(&format!("cubecl_matmul6_{name}_threads{threads}"), side * side * side, || {
                    matmul6(&ha, &hb, &out, side, threads, strict);
                    sync(&client);
                });
                let result = floats(&client.read_one_unchecked(out.clone()));
                println!(
                    "cubecl_matmul6_{name}_check side={side} threads={threads} differences={}",
                    differences(&result, &oracle_matmul(&a, &b, side, step))
                );
                row(&format!("cubecl_matmul_{name}_threads{threads}"), side * side * side, || {
                    matmul(&ha, &hb, &out, side, threads, strict);
                    sync(&client);
                });
                let result = floats(&client.read_one_unchecked(out.clone()));
                println!(
                    "cubecl_matmul_{name}_check side={side} threads={threads} differences={}",
                    differences(&result, &oracle_matmul(&a, &b, side, step))
                );
            }
        }
    }
    for side in MLP_SIDES {
        let (x, w1, w2) = (
            hard_fixture(side * side, 5, false),
            hard_fixture(side * side, 6, false),
            hard_fixture(side * side, 7, false),
        );
        let (hx, hw1, hw2) = (
            client.create_from_slice(f32::as_bytes(&x)),
            client.create_from_slice(f32::as_bytes(&w1)),
            client.create_from_slice(f32::as_bytes(&w2)),
        );
        let (h1, h2, out) =
            (client.empty(side * side * 4), client.empty(side * side * 4), client.empty(side * side * 4));
        for (name, strict) in [("nofma", true), ("default", false)] {
            let go = || {
                // Each contract's better kernel: four rows when strict, six when fused.
                let multiply = |x: &Handle, y: &Handle, z: &Handle| {
                    if strict { matmul(x, y, z, side, 1, true) } else { matmul6(x, y, z, side, 1, false) }
                };
                multiply(&hx, &hw1, &h1);
                // SAFETY: both buffers hold side^2 scalars, side^2 / 8 eight-lane vectors.
                unsafe {
                    shift_relu::launch_unchecked(
                        &client,
                        CubeCount::Static(1, 1, 1),
                        CubeDim::new_1d(1),
                        8,
                        BufferArg::from_raw_parts(h1.clone(), side * side),
                        BufferArg::from_raw_parts(h2.clone(), side * side),
                        side * side / 8,
                        1,
                    );
                }
                multiply(&h2, &hw2, &out);
            };
            row(&format!("cubecl_mlp2_{name}_threads1"), side * side * side * 2, || {
                go();
                sync(&client);
            });
            if strict {
                let result = floats(&client.read_one_unchecked(out.clone()));
                println!(
                    "cubecl_mlp2_check side={side} differences={}",
                    differences(&result, &oracle_mlp(&x, &w1, &w2, side))
                );
            }
        }
    }
}

/// Identifies what semantics CubeCL's default output has, by comparing against candidates.
fn diagnose() {
    let client = CpuRuntime::client(&CpuDevice);
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
    let side = 64;
    let (p, q) = (hard_fixture(side * side, 3, false), hard_fixture(side * side, 4, false));
    let (hp, hq) = (client.create_from_slice(f32::as_bytes(&p)), client.create_from_slice(f32::as_bytes(&q)));
    let out = client.empty(side * side * 4);
    // SAFETY: as in `cubecl_side`.
    unsafe {
        matmul_micro::launch_unchecked(
            &client,
            CubeCount::Static(1, 1, 1),
            CubeDim::new_1d(1),
            8,
            BufferArg::from_raw_parts(hp, side * side),
            BufferArg::from_raw_parts(hq, side * side),
            BufferArg::from_raw_parts(out.clone(), side * side),
            side,
            side,
            side / 8,
            1,
        );
    }
    let actual = floats(&client.read_one_unchecked(out));
    println!(
        "diagnose matmul side={side} vs_strict={} vs_fma={}",
        differences(&actual, &oracle_matmul(&p, &q, side, strict_step)),
        differences(&actual, &oracle_matmul(&p, &q, side, fused_step)),
    );
}

fn main() {
    // Before any thread exists, so every thread inherits the counter.
    assert!(COUNTER.set(Counter::open()).is_ok());
    match std::env::args().nth(1).as_deref() {
        Some("diagnose") => diagnose(),
        Some("ours") => ours(),
        Some("cubecl") => cubecl_side(),
        _ => {
            eprintln!("usage: l3i-cubecl-compare ours|cubecl|diagnose");
            std::process::exit(2);
        }
    }
}
