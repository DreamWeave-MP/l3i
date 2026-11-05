//! Single-thread, packing-inclusive comparison; matrixmultiply may contract/reassociate.
#![allow(clippy::cast_precision_loss, clippy::cast_possible_wrap, clippy::too_many_lines)]
use l3i_graph_cpu_research::{Budget, Domain, Expr, Numeric, Shape, Tensor};
use std::{hint::black_box, time::Instant};

fn timing(label: &str, mut run: impl FnMut()) {
    run();
    let mut repetitions = 1;
    loop {
        let start = Instant::now();
        for _ in 0..repetitions {
            run();
        }
        if start.elapsed().as_secs_f64() >= 0.003 {
            break;
        }
        repetitions *= 2;
    }
    let mut samples: Vec<f64> = (0..31)
        .map(|_| {
            let start = Instant::now();
            for _ in 0..repetitions {
                run();
            }
            start.elapsed().as_secs_f64() * 1e6 / f64::from(repetitions)
        })
        .collect();
    samples.sort_by(f64::total_cmp);
    println!("{label} median_us={:.3} min_us={:.3} p90_us={:.3}", samples[15], samples[0], samples[27]);
}

fn main() {
    #[cfg(feature = "openblas")]
    blas::initialize();
    println!(
        "prepared contiguous inputs; preallocated outputs; packing included; one thread; no cached packed weights"
    );
    println!("strict: ordered f32; fast: ordered FMA where available; matrixmultiply: library-selected arithmetic");
    for (m, k, n) in [
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
    ] {
        println!("shape m={m} k={k} n={n}");
        let a: Vec<f32> = (0..m * k).map(|i| (i % 29) as f32 * 0.013 - 0.17).collect();
        let b: Vec<f32> = (0..k * n).map(|i| (i % 31) as f32 * 0.017 - 0.21).collect();
        let mut expected = vec![0.0f64; m * n];
        let mut magnitude = vec![0.0f64; m * n];
        for i in 0..m {
            for j in 0..n {
                for p in 0..k {
                    let v = f64::from(a[i * k + p]) * f64::from(b[p * n + j]);
                    expected[i * n + j] += v;
                    magnitude[i * n + j] += v.abs();
                }
            }
        }
        let validate = |result: &[f32]| {
            for ((actual, want), scale) in result.iter().zip(&expected).zip(&magnitude) {
                let bound = 4.0 * k as f64 * f64::from(f32::EPSILON) * scale + 1e-10;
                assert!((f64::from(*actual) - want).abs() <= bound, "independent f64 error bound");
            }
        };
        let budget = Budget::new(64 << 20);
        let domain = Domain::new(1024, 1024);
        let bytes = |values: &[f32]| values.iter().flat_map(|v| v.to_le_bytes()).collect::<Vec<u8>>();
        let ta = Tensor::import(&bytes(&a), Shape::Matrix(m, k), &budget).unwrap();
        let tb = Tensor::import(&bytes(&b), Shape::Matrix(k, n), &budget).unwrap();
        let graph = Expr::slot(0, ta.shape(), domain.clone())
            .unwrap()
            .matmul(&Expr::slot(1, tb.shape(), domain).unwrap())
            .unwrap();
        let mut output = vec![0u8; m * n * 4];
        for mode in [Numeric::Strict, Numeric::Fast] {
            let plan = graph.compile_with(mode).unwrap();
            timing(&format!("graph_{mode:?}"), || {
                plan.run_into(black_box(&[ta.borrow(), tb.borrow()]), black_box(&mut output), &budget).unwrap();
            });
            validate(&output.as_chunks::<4>().0.iter().map(|v| f32::from_le_bytes(*v)).collect::<Vec<_>>());
            // Weights packed once beforehand, the packing charged and not counted per call. Only
            // shapes with a kernel that reads a packing have this row.
            if m > 1 {
                let (left, right) = if n == 1 {
                    (ta.prepare_left(&budget).unwrap(), tb.clone())
                } else {
                    (ta.clone(), tb.prepare_right(&budget).unwrap())
                };
                timing(&format!("graph_{mode:?}_prepared"), || {
                    plan.run_into(black_box(&[left.borrow(), right.borrow()]), black_box(&mut output), &budget)
                        .unwrap();
                });
                validate(&output.as_chunks::<4>().0.iter().map(|v| f32::from_le_bytes(*v)).collect::<Vec<_>>());
            }
        }
        let mut output = vec![0.0f32; m * n];
        timing("matrixmultiply", || {
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
        validate(&output);
        #[cfg(feature = "openblas")]
        {
            timing("openblas_sgemm", || blas::multiply(m, k, n, &a, &b, &mut output));
            validate(&output);
            // A vector product has its own tuned BLAS routine, which is the fair comparison.
            if n == 1 {
                timing("openblas_sgemv", || blas::matrix_vector(m, k, &a, &b, &mut output));
                validate(&output);
            } else if m == 1 {
                timing("openblas_sgemv_transposed", || blas::vector_matrix(k, n, &a, &b, &mut output));
                validate(&output);
            }
        }
        println!("graph_budget_peak_bytes={}", budget.peak());
    }
}

#[cfg(feature = "openblas")]
mod blas {
    use std::ffi::{CStr, c_char, c_int};
    unsafe extern "C" {
        fn cblas_sgemv(
            order: c_int,
            trans: c_int,
            m: c_int,
            n: c_int,
            alpha: f32,
            a: *const f32,
            lda: c_int,
            x: *const f32,
            incx: c_int,
            beta: f32,
            y: *mut f32,
            incy: c_int,
        );
        fn openblas_set_num_threads(count: c_int);
        fn openblas_get_config() -> *const c_char;
        fn cblas_sgemm(
            order: c_int,
            trans_a: c_int,
            trans_b: c_int,
            m: c_int,
            n: c_int,
            k: c_int,
            alpha: f32,
            a: *const f32,
            lda: c_int,
            b: *const f32,
            ldb: c_int,
            beta: f32,
            c: *mut f32,
            ldc: c_int,
        );
    }
    pub fn initialize() {
        // SAFETY: OpenBLAS returns a process-lifetime NUL-terminated configuration string.
        let config = unsafe { CStr::from_ptr(openblas_get_config()) }.to_string_lossy();
        assert!(!config.contains("USE64BITINT"), "this benchmark requires the LP64 CBLAS ABI");
        if config.contains("USE_OPENMP") {
            assert_eq!(
                std::env::var("OMP_NUM_THREADS").as_deref(),
                Ok("1"),
                "set OMP_NUM_THREADS=1 before launching an OpenMP OpenBLAS benchmark"
            );
        }

        // SAFETY: initialize the process-wide library before any work; one is a valid thread count.
        unsafe {
            openblas_set_num_threads(1);
        }
        println!("OpenBLAS {config}; threads=1");
    }
    /// `out = a x`, with `a` `m` by `k`.
    pub fn matrix_vector(m: usize, k: usize, a: &[f32], x: &[f32], out: &mut [f32]) {
        assert_eq!((a.len(), x.len(), out.len()), (m * k, k, m));
        let (m, k) = (c_int::try_from(m).unwrap(), c_int::try_from(k).unwrap());
        // SAFETY: LP64 checked at initialization; 101 is row-major, 111 is no transpose. The disjoint
        // slices have exactly these extents.
        unsafe { cblas_sgemv(101, 111, m, k, 1.0, a.as_ptr(), k, x.as_ptr(), 1, 0.0, out.as_mut_ptr(), 1) };
    }
    /// `out = x b`, with `b` `k` by `n`, as the transposed product.
    pub fn vector_matrix(k: usize, n: usize, x: &[f32], b: &[f32], out: &mut [f32]) {
        assert_eq!((b.len(), x.len(), out.len()), (k * n, k, n));
        let (k, n) = (c_int::try_from(k).unwrap(), c_int::try_from(n).unwrap());
        // SAFETY: as for `matrix_vector`, with 112 the transpose.
        unsafe { cblas_sgemv(101, 112, k, n, 1.0, b.as_ptr(), n, x.as_ptr(), 1, 0.0, out.as_mut_ptr(), 1) };
    }
    #[allow(clippy::many_single_char_names)]
    pub fn multiply(m: usize, k: usize, n: usize, a: &[f32], b: &[f32], out: &mut [f32]) {
        assert_eq!(a.len(), m * k);
        assert_eq!(b.len(), k * n);
        assert_eq!(out.len(), m * n);
        let (m, k, n) = (c_int::try_from(m).unwrap(), c_int::try_from(k).unwrap(), c_int::try_from(n).unwrap());
        // SAFETY: LP64 checked at initialization; 101 is row-major, 111 is no transpose.
        // The disjoint slices cover all row-major dimensions; leading dimensions match their rows.
        unsafe {
            cblas_sgemm(101, 111, 111, m, n, k, 1.0, a.as_ptr(), k, b.as_ptr(), n, 0.0, out.as_mut_ptr(), n);
        }
    }
}
