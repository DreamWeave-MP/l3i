# Packing-inclusive CPU matrix comparison

Run from repository root to inherit verified Clang/LTO configuration:

```
scripts/compute_compare.sh compute_matrix
L3I_BENCH_OPENBLAS=/usr/lib64/libopenblaso.so.0 scripts/compute_compare.sh compute_matrix
```

OpenBLAS is opt-in and benchmark-only. Supply an absolute library path for your host. The wrapper requires LP64, rejects USE64BITINT, and requires OMP_NUM_THREADS=1 for an OpenMP build. No production dependency changes. Matrixmultiply 0.3.11 is the same version used by the earlier CubeCL comparison, now isolated from CubeCL.

Inputs and outputs are prepared, row-major and nonoverlapping. Ordinary rows include internal packing; explicitly named `prepared` rows reuse weights packed before timing and report their retained bytes. Graph timings include binding/dispatch; library timings call SGEMM directly, so fixed overhead is not equivalent. All backends materialize output. One thread, warm repeated data, 31 samples per row; median/min/p90 in microseconds. No cache flushing or fixed frequency. Graph strict preserves ordered f32; fast allows FMA; libraries select their own contraction/reassociation. The common independent f64 oracle uses a magnitude-based rounding bound; it does not prove bitwise equivalence. Strict bitwise tests live in graph-cpu.

## Findings and corrections

Raw reports are in l3i-matrix-*.txt, including process peak RSS. The first report omitted OMP_NUM_THREADS despite calling openblas_set_num_threads; its large-shape OpenBLAS timings are invalid for single-thread comparison and are retained to document the mistake. The second corrected the environment but overlapped compilation; treat it as exploratory. Tail-after and dotrows reports use the corrected environment after builds completed. OpenBLAS reports 0.3.29, Haswell, USE_OPENMP on this i7-10870H.

The comparison revealed expensive scalar tails. Holding tail outputs in registers across increasing k improved the initial 31x127x17 strict observation from 26.949 to 6.799 us. A dedicated four-independent-row column kernel then reduced 256x256x1 from 63.261 to 25.198 us strict (21.115 fast); the original was 355.024 us. Same-run matrixmultiply was 29.542 us and OpenBLAS 14.368 us. These are initial process medians, not a broad competitiveness claim. Repeat evidence is preserved in dotrows-2.

The newer `l3i-matrix-review-1.txt` includes tuned SGEMV in both orientations and prepared-weight rows. It supersedes the earlier SGEMM-only matrix-vector comparison. For 1x128x128, for example, graph strict/fast measured 0.950/0.824 us versus transposed OpenBLAS GEMV 0.765 us: beating SGEMM was not proof of a matrix-vector win. Read each shape and numerical contract independently. Native x86 and ARM program/JSL shape artifacts are recorded in [native evidence](native-1f0e952.md). Hardware counters, controlled-frequency repetitions, larger cold weight sets and matched arithmetic contracts remain limitations.
