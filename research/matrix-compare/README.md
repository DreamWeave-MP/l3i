# Packing-inclusive CPU matrix comparison

Run from repository root to inherit verified Clang/LTO configuration:

```
cargo run --release --locked --manifest-path research/matrix-compare/Cargo.toml
L3I_BENCH_OPENBLAS=/usr/lib64/libopenblaso.so.0 cargo build --release --locked --features openblas --manifest-path research/matrix-compare/Cargo.toml
OMP_NUM_THREADS=1 OPENBLAS_NUM_THREADS=1 /usr/bin/time -v research/matrix-compare/target/release/l3i-matrix-compare
```

OpenBLAS is opt-in and benchmark-only. Supply an absolute library path for your host. The wrapper requires LP64, rejects USE64BITINT, and requires OMP_NUM_THREADS=1 for an OpenMP build. No production dependency changes. Matrixmultiply 0.3.11 is the same version used by the earlier CubeCL comparison, now isolated from CubeCL.

Inputs and outputs are prepared, row-major and nonoverlapping. Every invocation includes internal packing, without cached packed weights. Graph timings include binding/dispatch; library timings call SGEMM directly, so fixed overhead is not equivalent. All backends materialize output. One thread, warm repeated data, 31 samples per row; median/min/p90 in microseconds. No cache flushing or fixed frequency. Graph strict preserves ordered f32; fast allows FMA; libraries select their own contraction/reassociation. The common independent f64 oracle uses a magnitude-based rounding bound; it does not prove bitwise equivalence. Strict bitwise tests live in graph-cpu.

## Findings and corrections

Raw reports are in ../results/l3i-matrix-*.txt, including process peak RSS. The first report omitted OMP_NUM_THREADS despite calling openblas_set_num_threads; its large-shape OpenBLAS timings are invalid for single-thread comparison and are retained to document the mistake. The second corrected the environment but overlapped compilation; treat it as exploratory. Tail-after and dotrows reports use the corrected environment after builds completed. OpenBLAS reports 0.3.29, Haswell, USE_OPENMP on this i7-10870H.

The comparison revealed expensive scalar tails. Holding tail outputs in registers across increasing k improved the initial 31x127x17 strict observation from 26.949 to 6.799 us. A dedicated four-independent-row column kernel then reduced 256x256x1 from 63.261 to 25.198 us strict (21.115 fast); the original was 355.024 us. Same-run matrixmultiply was 29.542 us and OpenBLAS 14.368 us. These are initial process medians, not a broad competitiveness claim. Repeat evidence is preserved in dotrows-2.

OpenBLAS remains stronger on irregular matrices and small batches. Square fast graph matmul approaches matrixmultiply in this sweep, but strict arithmetic has a different contract. The graph single-row streaming path does well against SGEMM; a tuned BLAS GEMV comparison remains necessary before claiming a matrix-vector win. Persistent weights, kernel-only timing, JSL shape coverage, counters, allocation instrumentation and ARM performance remain outstanding.
