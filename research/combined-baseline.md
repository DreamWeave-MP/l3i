# Combined baseline, 2026-11-04

Main e1ca1ccfefe0d386ea54fe81f0fea11f4562a5fd was merged first (ce7e2fc), then PR #6 83f699365fe06532e30b450e8843a7307d1eb19d. Combined baseline: aa9cc7c867d47976033073c911685fee63d46278. Five main conflicts only concerned Rust versions; retained 1.99.1. Dependency merge had no conflicts. Luau is main's 0.745.

Production: 136 unit tests and 562 integration tests passed (four ignored), plus doctests. Socket tests require execution outside the restricted sandbox. Production and research pedantic Clippy passed. Research debug, release no-JIT and release optional-JIT each passed 15 library and two host tests. Formatting passed. Packaging, cross-target checks and final CI remain outstanding.

Baseline command: `/usr/bin/time -v research/graph-cpu/target/release/l3i-graph-cpu-research --bench`. Built with `cargo build --locked --release --manifest-path research/graph-cpu/Cargo.toml`. No luau-jit feature: benchmark labels mentioning native Luau do NOT mean JIT execution in this configuration. Compiler Rust 1.99.1, Clang 23.1.2, repository cross-language LTO configuration. Intel i7-10870H, AVX2/FMA, single-thread execution. Frequency was not fixed; competing load was substantial (one-minute load 13.79 before first sample). Each row reports 31 samples. Fresh-process repeats pending; do not infer improvements from historical measurements.

First run raw timings and process peak RSS are in results/l3i-combined-baseline-1*.txt. Peak RSS includes the full mixed benchmark and Luau GC, not just kernel storage. Payload allocation counts are not total allocator counts.

## Initial architectural audit

Production contiguous kernels are private Span-based operations supporting eight representations, sequential overlapping writes, and established f64 intermediates. Graph kernels use disjoint output slices, byte-addressable immutable views, explicit f32 arithmetic, and fused tiles. Public data APIs remain authoritative. Direct delegation would require borrowing/aliasing and numerical proofs, not merely exposing private functions.

Addition of two f32 operands can share numerical results under separately rounded semantics, but graph fusion avoids intermediate writes and production operations support aliasing. Scaling by f64 is not interchangeable with graph f32 multiplication. Production comparisons, first-occurrence scans and reductions have different contracts from planned strict-f32 reductions. No production rewrite is justified by this audit so far.

Existing graph matmul already packs both left and right panels. It repacks weights per invocation. Investigate repeated immutable weight reuse and matrix-vector shapes before adding another packing layer. Thread-local scratch currently retains up to 64 KiB while charging only active calls; retained storage accounting remains an explicit unresolved integration issue.

## Completed baseline repeats

Three fresh-process runs are preserved in results/l3i-combined-baseline-{1,2,3}*.txt. Median-of-process-medians at 65,536 elements: JSL borrowed runInto 13.853 us (process medians 13.844–16.291), production three-pass data 31.612 us (30.954–35.697). Strict 256-square preallocated program matmul: 560.454 us (544.033–573.525). Process peak RSS: 95,880–96,072 KiB. These are observations under variable load, not confidence intervals or historical speedup claims.

## Single-row streaming experiment

The extended shape benchmark measures strict program execution with input tensors prepared, output preallocated, and weight packing included. It compares every output with the reference evaluator. Existing square shapes remain, with vector, small-batch, rectangular and irregular shapes added.

A single output row has no reuse across output rows to amortize packing. Streaming contiguous right-hand rows directly avoids the copy and uses zero packing scratch. Ascending k, separate strict multiplication/addition, and explicit fast FMA behavior remain intact. No production API or implementation changes.

First before/after observations (us; repeat validation pending):

| Shape m,k,n | Before | Streaming |
| --- | ---: | ---: |
| 1,256,256 | 21.178 | 4.725 |
| 1,1024,1024 | 329.283 | 87.935 |

The baseline scalar path also improves (320.020 to 128.720 us for the larger shape). Results are whole program invocations, not isolated arithmetic. No BLAS competitiveness claim follows. Raw reports: results/l3i-shapes-{before,after}*.txt. Compilation began during the later JSL section of the before report; the matrix rows used here completed before that compilation. Treat later unrelated rows as contaminated.

Release tests exercise single-row dimensions around vector widths and k-block boundaries, nonexact randomized operands, signed zeros, subnormals, infinities, NaNs, empty dimensions and explicit fast FMA results. All available ISA instantiations run strict special-value checks. Packing-scratch sizing is asserted zero. Broader repeated/concurrent and aliasing tests remain in the research suite.

Streaming repeat (fresh process, no concurrent compilation): 4.400 us for 1x256x256 and 83.923 us for 1x1024x1024 on detected AVX2, versus first-after 4.725 and 87.935 us. The improvement is reproducible on this host; additional baseline repeats and isolated kernel/BLAS comparisons remain needed for stronger claims. Results are preserved in results/l3i-shapes-after-2*.txt.

Focused research CI now covers native ubuntu-24.04 x86-64 and ubuntu-24.04-arm, Rust 1.99.1 with the repository's matching LLVM installer, debug and release no-JIT, optional-JIT release, formatting and all-feature pedantic Clippy. actionlint passes locally; remote execution is not yet verified. CubeCL and benchmarks are excluded from the ordinary correctness job. Locally qemu-aarch64-static exists, but an ARM C++ sysroot has not been located. No ARM performance claim is made.

## Scratch lifetime correction

Removed the thread-local cache: scratch now dies before its active budget charge, including on unwinding. This closes the documented idle-thread accounting escape without binding a program to a domain or retaining mutable inputs. Zero-scratch fused and streaming paths allocate no additional scratch. Other stages trade cache reuse for a per-stage allocation and zero initialization; this is a resource-correct default, not an allocation-free execution API. A host-owned, explicitly charged workspace remains a useful future optimization if measured repeated workloads justify it.

Raw mixed-workload measurements are results/l3i-scratch-{bench,rss}.txt. The common 1,024-element preallocated fused expression remained at two heap acquisitions per call and measured 0.198 us versus 0.190 us in the previous streaming run. Variable host load prevents treating that small difference as a demonstrated regression. Peak process RSS is reported separately; it includes runtime and benchmark state and is not a domain memory bound.
