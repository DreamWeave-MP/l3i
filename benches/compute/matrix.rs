//! The matrix tournament: one thread by default, packing included, every side validated against an
//! independent f64 bound. `L3I_BENCH_SHAPES=256x256x256,6x128x16` chooses shapes, `L3I_BENCH_ONLY`
//! keeps rows whose label holds one of its comma-separated pieces, and `L3I_BENCH_BLAS` loads CBLAS
//! libraries by name, for example `openblas=/usr/lib64/libopenblaso.so.0,blis=/path/libblis.so.4`.
//! They load in the same process with their symbols kept apart, so every side runs under the same
//! conditions. Libraries differ in what they promise: the labels say which rows are which contract.
#![allow(clippy::cast_precision_loss, clippy::cast_possible_wrap, clippy::too_many_lines)]
use crate::counter::{
    CACHE_READ_MISS, Counter, PERF_COUNT_HW_CACHE_L1D, PERF_COUNT_HW_CACHE_LL, PERF_COUNT_HW_CPU_CYCLES,
    PERF_COUNT_HW_INSTRUCTIONS, PERF_TYPE_HARDWARE, PERF_TYPE_HW_CACHE,
};
use crate::counting_alloc::ALLOCATIONS;
use l3i::compute::{Budget, Domain, Expr, Limits, Numeric, Shape, Tensor};
use std::{hint::black_box, sync::atomic::Ordering, time::Instant};

fn wanted(label: &str) -> bool {
    std::env::var("L3I_BENCH_ONLY").map_or(true, |only| only.split(',').any(|piece| label.contains(piece)))
}

/// Reports `run` under `label`, unless `L3I_BENCH_ONLY` leaves it out: then it says it didn't run.
fn timing(label: &str, mut run: impl FnMut()) -> bool {
    if !wanted(label) {
        return false;
    }
    let before = ALLOCATIONS.load(Ordering::Relaxed);
    run();
    let allocations = ALLOCATIONS.load(Ordering::Relaxed) - before;
    let mut repetitions = 1;
    let once = loop {
        let start = Instant::now();
        for _ in 0..repetitions {
            run();
        }
        let elapsed = start.elapsed().as_secs_f64();
        if elapsed >= 0.003 {
            break elapsed / f64::from(repetitions);
        }
        repetitions *= 2;
    };
    // Past the caches a run is long and these only have to find a median.
    let (samples, counted_runs) = if once > 0.02 { (9usize, 5usize) } else { (31, 15) };
    let mut elapsed: Vec<f64> = (0..samples)
        .map(|_| {
            let start = Instant::now();
            for _ in 0..repetitions {
                run();
            }
            start.elapsed().as_secs_f64() * 1e6 / f64::from(repetitions)
        })
        .collect();
    elapsed.sort_by(f64::total_cmp);
    // Cycles are what a change to a kernel moves: frequency and machine load don't touch them.
    let mut counted = |kind: u32, config: u64| {
        Counter::open(kind, config).map_or(0.0, |counter| {
            let mut counts: Vec<u64> = (0..counted_runs)
                .map(|_| {
                    counter.measure(&mut || {
                        for _ in 0..repetitions {
                            run();
                        }
                    })
                })
                .collect();
            counts.sort_unstable();
            counts[counted_runs / 2] as f64 / f64::from(repetitions)
        })
    };
    let cycles = counted(PERF_TYPE_HARDWARE, PERF_COUNT_HW_CPU_CYCLES);
    let instructions = counted(PERF_TYPE_HARDWARE, PERF_COUNT_HW_INSTRUCTIONS);
    let l1_misses = counted(PERF_TYPE_HW_CACHE, PERF_COUNT_HW_CACHE_L1D | CACHE_READ_MISS);
    let last_misses = counted(PERF_TYPE_HW_CACHE, PERF_COUNT_HW_CACHE_LL | CACHE_READ_MISS);
    println!(
        "{label} median_us={:.3} min_us={:.3} p90_us={:.3} cycles={cycles:.0} instructions={instructions:.0} l1_read_misses={l1_misses:.0} ll_read_misses={last_misses:.0} allocations={allocations}",
        elapsed[samples / 2],
        elapsed[0],
        elapsed[samples * 9 / 10],
    );
    true
}

/// `4 k eps sum(|a b|)` and the exact-enough answer, from f64 arithmetic that shares nothing with any
/// side. A wrong kernel can't get a flattering number.
struct Validation {
    expected: Vec<f64>,
    magnitude: Vec<f64>,
    k: usize,
}
impl Validation {
    fn new(a: &[f32], b: &[f32], m: usize, k: usize, n: usize) -> Self {
        let mut expected = vec![0.0f64; m * n];
        let mut magnitude = vec![0.0f64; m * n];
        for i in 0..m {
            let (sums, sizes) = (&mut expected[i * n..(i + 1) * n], &mut magnitude[i * n..(i + 1) * n]);
            for p in 0..k {
                let scale = f64::from(a[i * k + p]);
                for ((sum, size), factor) in sums.iter_mut().zip(sizes.iter_mut()).zip(&b[p * n..(p + 1) * n]) {
                    let term = scale * f64::from(*factor);
                    *sum += term;
                    *size += term.abs();
                }
            }
        }
        Self { expected, magnitude, k }
    }
    fn check(&self, result: &[f32]) {
        for ((actual, want), scale) in result.iter().zip(&self.expected).zip(&self.magnitude) {
            let bound = 4.0 * self.k as f64 * f64::from(f32::EPSILON) * scale + 1e-10;
            assert!((f64::from(*actual) - want).abs() <= bound, "independent f64 error bound");
        }
    }
}

pub fn main() {
    let libraries = blas::load();
    println!(
        "prepared contiguous inputs; preallocated outputs; packing included; one thread; no cached packed weights"
    );
    println!("strict: ordered f32; fast: ordered FMA where available; matrixmultiply: library-selected arithmetic");
    let shapes = std::env::var("L3I_BENCH_SHAPES").map_or_else(
        |_| {
            vec![
                (1usize, 128, 128),
                (1, 256, 256),
                (1, 1024, 1024),
                (1, 4096, 1),
                (4, 1024, 1024),
                (8, 512, 512),
                (16, 16, 16),
                (31, 127, 17),
                (64, 64, 64),
                (64, 128, 1),
                (64, 128, 128),
                (128, 128, 128),
                (128, 64, 512),
                (256, 256, 1),
                (256, 256, 256),
                (4096, 16, 1),
            ]
        },
        |shapes| {
            shapes
                .split(',')
                .map(|shape| {
                    let mut parts = shape.split('x').map(|part| part.parse::<usize>().expect("MxKxN"));
                    (parts.next().unwrap(), parts.next().unwrap(), parts.next().unwrap())
                })
                .collect()
        },
    );
    for (m, k, n) in shapes {
        println!("shape m={m} k={k} n={n}");
        let a: Vec<f32> = (0..m * k).map(|i| (i % 29) as f32 * 0.013 - 0.17).collect();
        let b: Vec<f32> = (0..k * n).map(|i| (i % 31) as f32 * 0.017 - 0.21).collect();
        let validation = Validation::new(&a, &b, m, k, n);
        // Limits a trusted host would raise; these shapes are this bench's own.
        let limits = Limits { elements: 1 << 28, work: 1 << 40 };
        let budget = Budget::with_elements(8 << 30, limits.elements);
        let domain = Domain::with_limits(1024, 1024, limits);
        let bytes = |values: &[f32]| values.iter().flat_map(|v| v.to_le_bytes()).collect::<Vec<u8>>();
        let ta = Tensor::import(&bytes(&a), Shape::Matrix(m, k), &budget).unwrap();
        let tb = Tensor::import(&bytes(&b), Shape::Matrix(k, n), &budget).unwrap();
        let graph = Expr::slot(0, ta.shape(), domain.clone())
            .unwrap()
            .matmul(&Expr::slot(1, tb.shape(), domain).unwrap())
            .unwrap();
        let mut output = vec![0u8; m * n * 4];
        let read = |bytes: &[u8]| bytes.as_chunks::<4>().0.iter().map(|v| f32::from_le_bytes(*v)).collect::<Vec<_>>();
        for mode in [Numeric::Strict, Numeric::Fast] {
            let plan = graph.compile_with(mode).unwrap();
            if timing(&format!("graph_{mode:?}"), || {
                plan.run_into(black_box(&[ta.borrow(), tb.borrow()]), black_box(&mut output), &budget).unwrap();
            }) {
                validation.check(&read(&output));
            }
            // Weights packed once beforehand, the packing charged and not counted per call. Only
            // shapes with a kernel that reads a packing have this row.
            if m > 1 {
                let (left, right) = if n == 1 {
                    (ta.prepare_left(&budget).unwrap(), tb.clone())
                } else {
                    (ta.clone(), tb.prepare_right(&budget).unwrap())
                };
                if timing(&format!("graph_{mode:?}_prepared"), || {
                    plan.run_into(black_box(&[left.borrow(), right.borrow()]), black_box(&mut output), &budget)
                        .unwrap();
                }) {
                    validation.check(&read(&output));
                }
            }
        }
        let mut output = vec![0.0f32; m * n];
        let ran = timing("matrixmultiply", || {
            // SAFETY: nonoverlapping live arrays have exactly the row-major extents and strides supplied.
            unsafe {
                matrixmultiply::sgemm(
                    m,
                    k,
                    n,
                    1.0,
                    black_box(a.as_ptr()),
                    k as isize,
                    1,
                    black_box(b.as_ptr()),
                    n as isize,
                    1,
                    0.0,
                    black_box(output.as_mut_ptr()),
                    n as isize,
                    1,
                );
            }
        });
        if ran {
            validation.check(&output);
        }
        for library in &libraries {
            if timing(&format!("{}_sgemm", library.name), || library.multiply(m, k, n, &a, &b, &mut output)) {
                validation.check(&output);
            }
            // A vector product has its own tuned BLAS routine, which is the fair comparison.
            let vector = if n == 1 {
                timing(&format!("{}_sgemv", library.name), || library.matrix_vector(m, k, &a, &b, &mut output))
            } else if m == 1 {
                timing(&format!("{}_sgemv_transposed", library.name), || {
                    library.vector_matrix(k, n, &a, &b, &mut output);
                })
            } else {
                false
            };
            if vector {
                validation.check(&output);
            }
        }
        println!("graph_budget_peak_bytes={}", budget.peak());
    }
}

mod blas {
    use std::ffi::{CStr, CString, c_char, c_int, c_void};
    unsafe extern "C" {
        fn dlopen(path: *const c_char, flags: c_int) -> *mut c_void;
        fn dlsym(library: *mut c_void, name: *const c_char) -> *mut c_void;
    }
    /// Resolve everything now, and keep this library's symbols out of the global scope.
    const RTLD_NOW_LOCAL: c_int = 2;
    type Sgemv = unsafe extern "C" fn(
        c_int,
        c_int,
        c_int,
        c_int,
        f32,
        *const f32,
        c_int,
        *const f32,
        c_int,
        f32,
        *mut f32,
        c_int,
    );
    type Sgemm = unsafe extern "C" fn(
        c_int,
        c_int,
        c_int,
        c_int,
        c_int,
        c_int,
        f32,
        *const f32,
        c_int,
        *const f32,
        c_int,
        f32,
        *mut f32,
        c_int,
    );
    pub struct Library {
        pub name: String,
        sgemv: Sgemv,
        sgemm: Sgemm,
    }
    fn symbol(library: *mut c_void, name: &CStr) -> *mut c_void {
        // SAFETY: a live handle from `dlopen` and a NUL-terminated name.
        unsafe { dlsym(library, name.as_ptr()) }
    }
    pub fn load() -> Vec<Library> {
        let Ok(list) = std::env::var("L3I_BENCH_BLAS") else {
            return Vec::new();
        };
        let threads: c_int = std::env::var("L3I_BENCH_THREADS").map_or(1, |n| n.parse().expect("a thread count"));
        list.split(',')
            .map(|entry| {
                let (name, path) = entry.split_once('=').expect("name=path");
                let path = CString::new(path).unwrap();
                // SAFETY: a NUL-terminated path; the library stays loaded for the process's life.
                let library = unsafe { dlopen(path.as_ptr(), RTLD_NOW_LOCAL) };
                assert!(!library.is_null(), "could not load {path:?}");
                let (vector, product) = (symbol(library, c"cblas_sgemv"), symbol(library, c"cblas_sgemm"));
                assert!(!vector.is_null() && !product.is_null(), "{path:?} has no CBLAS");
                // Each library's own way to choose a thread count, where it has one.
                let openblas = symbol(library, c"openblas_set_num_threads");
                if !openblas.is_null() {
                    // SAFETY: this symbol is OpenBLAS's `void (int)`, called before any work.
                    unsafe { std::mem::transmute::<*mut c_void, unsafe extern "C" fn(c_int)>(openblas)(threads) };
                    let config = symbol(library, c"openblas_get_config");
                    // SAFETY: `char *(void)` returning a process-lifetime string.
                    let config = unsafe {
                        CStr::from_ptr(std::mem::transmute::<*mut c_void, unsafe extern "C" fn() -> *const c_char>(
                            config,
                        )())
                    }
                    .to_string_lossy();
                    assert!(!config.contains("USE64BITINT"), "this benchmark requires the LP64 CBLAS ABI");
                    if config.contains("USE_OPENMP") {
                        assert_eq!(
                            std::env::var("OMP_NUM_THREADS").ok(),
                            Some(threads.to_string()),
                            "set OMP_NUM_THREADS to L3I_BENCH_THREADS for an OpenMP OpenBLAS"
                        );
                    }
                    println!("{name}: OpenBLAS {config}; threads={threads}");
                }
                let blis = symbol(library, c"bli_thread_set_num_threads");
                if !blis.is_null() {
                    // SAFETY: this symbol is BLIS's `void (dim_t)`, with `dim_t` an i64 on LP64.
                    unsafe { std::mem::transmute::<*mut c_void, unsafe extern "C" fn(i64)>(blis)(threads.into()) };
                    println!("{name}: BLIS; threads={threads}");
                }
                Library {
                    name: name.to_owned(),
                    // SAFETY: both symbols are CBLAS functions of exactly these signatures.
                    sgemv: unsafe { std::mem::transmute::<*mut c_void, Sgemv>(vector) },
                    // SAFETY: as above.
                    sgemm: unsafe { std::mem::transmute::<*mut c_void, Sgemm>(product) },
                }
            })
            .collect()
    }
    impl Library {
        /// `out = a x`, with `a` `m` by `k`.
        pub fn matrix_vector(&self, m: usize, k: usize, a: &[f32], x: &[f32], out: &mut [f32]) {
            assert_eq!((a.len(), x.len(), out.len()), (m * k, k, m));
            let (m, k) = (c_int::try_from(m).unwrap(), c_int::try_from(k).unwrap());
            // SAFETY: LP64 checked at load; 101 is row-major, 111 is no transpose. The disjoint slices
            // have exactly these extents.
            unsafe { (self.sgemv)(101, 111, m, k, 1.0, a.as_ptr(), k, x.as_ptr(), 1, 0.0, out.as_mut_ptr(), 1) };
        }
        /// `out = x b`, with `b` `k` by `n`, as the transposed product.
        pub fn vector_matrix(&self, k: usize, n: usize, x: &[f32], b: &[f32], out: &mut [f32]) {
            assert_eq!((b.len(), x.len(), out.len()), (k * n, k, n));
            let (k, n) = (c_int::try_from(k).unwrap(), c_int::try_from(n).unwrap());
            // SAFETY: as for `matrix_vector`, with 112 the transpose.
            unsafe { (self.sgemv)(101, 112, k, n, 1.0, b.as_ptr(), n, x.as_ptr(), 1, 0.0, out.as_mut_ptr(), 1) };
        }
        #[allow(clippy::many_single_char_names)]
        pub fn multiply(&self, m: usize, k: usize, n: usize, a: &[f32], b: &[f32], out: &mut [f32]) {
            assert_eq!(a.len(), m * k);
            assert_eq!(b.len(), k * n);
            assert_eq!(out.len(), m * n);
            let (m, k, n) = (c_int::try_from(m).unwrap(), c_int::try_from(k).unwrap(), c_int::try_from(n).unwrap());
            // SAFETY: LP64 checked at load; 101 is row-major, 111 is no transpose. The disjoint slices
            // cover all row-major dimensions; leading dimensions match their rows.
            unsafe {
                (self.sgemm)(101, 111, 111, m, n, k, 1.0, a.as_ptr(), k, b.as_ptr(), n, 0.0, out.as_mut_ptr(), n);
            }
        }
    }
}
