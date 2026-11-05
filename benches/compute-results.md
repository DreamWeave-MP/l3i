# CPU compute results

The engine is `src/compute` behind the `compute` feature; `compute-testing` adds its oracle and comparators to tests and benches. This page and `compute-results/` hold the decision record and raw reports: `compute-*.txt` the benchmark runs, `compare-*.txt` CubeCL's, `mutations.txt` the injected-bug record, `kernel-instructions.txt` and `luau-relu-sum-ir-and-x64.txt` the compiled-code evidence, `contract.md` the engine's contract, `matrix-compare.md`, `row-kernels.md` and `int8.md` their kernels. Paths in older entries name `research/`, this work's previous home; the CubeCL bridge experiments' last revision is `9e70c0f`.

- Harnesses, named explicitly: `cargo bench --features compute-testing --bench main -- compute_shapes` (also `compute_native`, `compute_rows`, `compute_softmax`, `compute_int8`).
- Against competitors: `scripts/compute_compare.sh compute_matrix` (matrixmultiply, and any CBLAS library named in `L3I_BENCH_BLAS`) and `scripts/compute_compare.sh compute_cubecl ours` (CubeCL; `scripts/compute_cubecl_container.sh` counts inside a disposable container).
- Injected bugs: `jess scripts/compute_mutate.jsl`.

## Where it stands (2026-11-05, at `e9e6799`)

These replace every earlier "where it stands" figure. The raw reports are in `compute-results/tournament-e9e6799/`. Single thread on an i7-10870H (AVX2, FMA, no AVX-512), pinned, with packing included unless a row says prepared. All sides ran in one process, at user-space cycles from the CPU's counter, median of five fresh processes unless noted. OpenBLAS is 0.3.29 and BLIS 2.0, each as Fedora 44 builds it, loaded at run time. Each row is validated against an independent f64 bound. Reproduce with:

```sh
L3I_BENCH_BLAS=openblas=/usr/lib64/libopenblaso.so.0,blis=/path/libblis.so.4 \
  L3I_BENCH_SHAPES=256x256x256,1024x1024x1024 scripts/compute_compare.sh compute_matrix
```

### GEMM: Fast against the libraries that fuse

Fast and BLAS both fuse multiply-add, and this table compares nothing else. `prepared` packs `b` once beforehand, a contract the libraries here don't offer through CBLAS, so it isn't a like-for-like win over them.

| Shape | Fast | Prepared | OpenBLAS | BLIS | matrixmultiply |
|---|---:|---:|---:|---:|---:|
| 8x8x8 | 1,285 | 630 | 506 | 3,964 | 339 |
| 16x16x16 | 1,190 | 818 | 1,013 | 4,231 | 1,068 |
| 32x32x32 | 4,143 | 3,267 | 5,033 | 6,437 | 4,466 |
| 64x64x64 | 24,507 | 21,105 | 26,233 | 23,446 | 28,332 |
| 128x128x128 | 173,882 | 161,155 | 173,051 | 167,978 | 197,918 |
| 256x256x256 | 1.37M | 1.27M | 1.25M | 1.32M | 1.44M |
| 512x512x512 | 10.44M | 10.41M | 9.75M | 10.16M | 11.30M |
| 1000x1000x1000 | 83.1M | 82.1M | 71.0M | 76.1M | 91.0M |
| 1024x1024x1024 | 87.3M | 89.8M | 76.9M | 79.6M | 95.2M |
| 2048x2048x2048 (one process) | 730M | 778M | 595M | 633M | 759M |
| 6x128x16 | 1,262 | 1,222 | 2,996 | 4,634 | 3,178 |
| 12x128x16 | 3,472 | 2,156 | 3,494 | 5,479 | 5,644 |
| 128x64x512 | 379,303 | 358,793 | 337,151 | 365,743 | 383,899 |
| 512x64x128 | 353,692 | 350,911 | 338,443 | 352,860 | 385,782 |
| 1000x16x16 | 33,656 | 32,889 | 41,657 | 26,204 | 45,861 |
| 16x16x1000 | 36,797 | 29,280 | 33,789 | 28,862 | 33,745 |
| 31x127x17 | 13,110 | 10,363 | 11,996 | 13,890 | 14,508 |
| 97x300x80 | 189,127 | 176,597 | 198,410 | 207,497 | 248,579 |

Geometric mean of Fast's cycles over each competitor's, by family (below 1.0 is ours ahead):

| Family | against OpenBLAS | against BLIS | against matrixmultiply |
|---|---:|---:|---:|
| 16 to 64 square | 0.97 | 0.57 | 0.96 |
| 128 to 512 square | 1.06 | 1.04 | 0.92 |
| 1000 and 1024 square | 1.15 | 1.09 | 0.91 |
| Rectangular and irregular, 8 shapes | 0.91 | 0.84 | 0.77 |

So OpenBLAS still leads square products past the second-level cache, by 15% on average and 23% in the single 2048 run, and it is the row to beat. Below that it's a contest within a few percent. Past the caches, one binary's cycles swing by more than the smaller gaps here: 256x256x256 read 1.28M to 2.14M across these five processes, so treat a difference under about 5% at those sizes as a tie. A vector product's fair competitor is `sgemv`: rows and cycles are in `after.txt`, where Fast at 1x4096x1 takes 1,236 against 2,235 for OpenBLAS and 1,626 for BLIS, while at 256x256x1 it takes 17,601 against 15,862 and 14,608.

Strict is its own contract with no competitor here: ordered, separately rounded and the same bits everywhere. The same two ports do its multiplies and adds, so 256x256x256 needs at least 2.10M cycles and takes 2.31M.

**Regressions from this round's kernel work.** `1000x16x16` Fast went from 30,453 to 33,656 cycles and `4096x16x1` from 63,341 to 69,017, about 10% each, and strict by similar amounts. An out-of-line kernel costs more per call when `k` is short, and the second of those doesn't reach that kernel, so its cause isn't established. 8x8x8 had also regressed through scratch it never used, which is fixed after these tables were taken: 1,054 now, from 1,115 before this round and still three times matrixmultiply's 339.

### Against CubeCL 0.11.3

Three rounds in alternating processes, one thread, our Fast against CubeCL's best lanes and FMA, cycles in thousands. Its rows at n=16,384 suggest a launch of about 200,000 cycles per call on top of its loop. It also generated these kernels at first use, which is what ours never does.

| Workload | n | Ours | CubeCL | |
|---|---:|---:|---:|---|
| workload | 1,048,576 | 1,303 | 1,247 | tie |
| chain16 | 1,048,576 | 2,303 | 2,044 | CubeCL by 13% |
| horner | 1,048,576 | 2,435 | 1,391 | CubeCL by 1.75x |
| residual | 1,048,576 | 2,033 | 1,302 | CubeCL by 1.56x |
| chain16 | 65,536 | 135 | 291 | ours by 2.2x |
| horner | 65,536 | 136 | 248 | ours by 1.8x |
| residual | 65,536 | 109 | 246 | ours by 2.3x |

**Correction.** Commit `7f53a08` says alignment was most of what looked like a fusion gap. Measured beside CubeCL it wasn't: alignment cut our in-cache cycles by 35 to 45%, but CubeCL's numbers from earlier in this file were noisy too, and it still leads large polynomials and DAGs. What remains is fusion, as a JIT compiles a whole expression into one loop, and memory: at this size one binary's cycles still vary by about 30%. Compiled three-step tiles for the common forms were tried and removed: 10 to 15% fewer instructions, at most 5% fewer cycles, for 150 KB of code.

### Threads

Elapsed microseconds, median, with a persistent rayon pool as the host's workers against OpenBLAS's own threads, because cycles on one thread can't show this. Both are at the same thread count on eight physical cores, with a browser and a desktop running. The Fedora BLIS loaded here is its serial build, so it isn't in this table.

| Shape | Threads | Fast | Prepared | OpenBLAS |
|---|---:|---:|---:|---:|
| 128x128x128 | 4 | 35 | 15 | 15 |
| 256x256x256 | 4 | 122 | 99 | 95 |
| 512x512x512 | 4 | 850 | 766 | 680 |
| 1024x1024x1024 | 4 | 6,028 | 5,540 | 5,620 |
| 128x128x128 | 8 | 47 | 30 | 17 |
| 256x256x256 | 8 | 183 | 101 | 92 |
| 512x512x512 | 8 | 950 | 806 | 396 |
| 1024x1024x1024 | 8 | 6,322 | 5,919 | 4,994 |

At four threads prepared weights are level with OpenBLAS up to 256x256x256 and past 1024, and 13% behind at 512. Without them each worker packing its own copy costs 2.3x at 128x128x128, and less as sizes grow. At eight, OpenBLAS is ahead by up to 2x. Part of that is wake-up latency in a pool that sleeps, which is the host's choice, and part is sharing no packed work between workers, which is ours. Row stages aren't shared at all.

### AArch64 and another x86

Native CI runners at `866413e`, elapsed microseconds, one thread, with OpenBLAS 0.3.26 and BLIS as Ubuntu 24.04 builds them: `tournament-ubuntu-24.04-arm.txt` (Neoverse-N2) and `tournament-ubuntu-24.04.txt` (EPYC 7763). There are no counters there and these are shared machines, so this shows the shape and not a lead. No kernel here is specific to AArch64 yet.

| Shape on Neoverse-N2 | Fast | Strict | OpenBLAS | BLIS | matrixmultiply |
|---|---:|---:|---:|---:|---:|
| 64x64x64 | 11.5 | 33.8 | 19.3 | 16.3 | 11.4 |
| 128x128x128 | 84.9 | 269 | 99.8 | 95.5 | 84.2 |
| 256x256x256 | 682 | 2,159 | 725 | 712 | 660 |
| 512x512x512 | 5,490 | 17,444 | 5,759 | 5,341 | 5,332 |
| 1024x1024x1024 | 44,375 | 140,414 | 42,010 | 42,876 | 42,650 |

Fast is level with all of them from LLVM's code alone. Strict being more than 3x Fast there is more than the port count explains, and it is AArch64's most obvious target. Int8 dots use `sdot`: 0.025 us per 1,024 elements against 0.070 for the baseline.

### Quantized

Exact against an independent i128 oracle, at the extremes and past where a partial sum could overflow. There is no competing library in this table, so this compares our own contracts: `compute_int8`, elapsed, 1024x1024 on the i7.

| Kernel | us | Weight memory |
|---|---:|---:|
| int8 matvec, AVX2 | 47.8 | 1 MiB |
| int8 matvec, baseline | 206 | 1 MiB |
| int4 matvec with group scales | 68.0 | 512 KiB |
| f32 strict matvec | about 280 | 4 MiB |
| f32 strict matvec, prepared | about 83 | 4 MiB, plus the packing |

None of this says how well a real model survives quantizing, which isn't tested.

### What preparing costs

`graph_Fast_prepare_once` against what a prepared call saves, worked out from those rows rather than measured as a sequence. At 64x64x64 and 128x128x128 packing costs under one call's savings, so it pays by the second call. At 512x512x512 and up the saving is smaller than the noise, and repeated products should expect little. A left-prepared `k=256` matvec costs 191,543 cycles and saves 51,042 per strict call, so it pays from the fourth, but saves nothing under Fast with a long inner dimension, whose reordered dot is already as fast.

### Memory and code

The engine allocates through 64-byte aligned, budget-charged blocks, and no row of the tables allocates more than once per call: `allocations=` in the raw reports. Peak budget per shape is `graph_budget_peak_bytes`. `compute` symbols in the bench binary take 1.44 MB of text, from 1.39 MB at the start of this round.

### What moved these

- **LLVM rebuilt six addresses per iteration.** Inlined, the 6x16 kernel's six runs of `a` are visibly a fixed distance apart, so loop strength reduction kept one base: 30 instructions per iteration, against 4 a cycle of rename, or 7.5 cycles where the fused multiply-adds need 6. Out of line it's 23 instructions. The earlier note here that rename width wasn't the limit was wrong for this loop.
- **malloc's large blocks start 16 bytes into a page**, so vector accesses straddled lines and buffers shared page offsets. Aligning everything the engine owns cut chains' in-cache cycles by 35 to 45%. Caller's buffers can't be aligned for them, so a `runInto` over Luau buffers doesn't get all of this.
- Tried and dropped, none clearly better than noise: block sizes other than 128 and 256, including 256 and 128 the other way round, and a prefetch of factors in the kernel loop. Packing all of `a` wasn't retried, though 1024 square's L1 misses (13.8M against BLIS's 6.2M) suggest its 4 KiB stride aliases in the first-level cache, and that's the next thing to try.

### Remaining gaps, by what they could buy

1. **Square products past the caches**, 15 to 23% behind OpenBLAS, and matmul is 84 to 90% of a transformer-style pass. Packing `a` when its stride aliases, and a cache-aware row block.
2. **AArch64 strict**, more than 3x Fast on Neoverse-N2. Read LLVM's code, then an explicit kernel if it is the cause.
3. **Eight threads**, up to 2x behind OpenBLAS. Sharing packed `b` among workers, and row stages.
4. **Short `k` and tiny products**: 8x8x8 is 3x matrixmultiply, and prepared beats the unprepared textbook path there, so the tiny threshold looks wrong; and the 10% regressions above.
5. **Long expressions over a million elements**, 1.5 to 1.75x behind CubeCL. Worth least: elementwise is about 3% of that pass, and a mandatory JIT stays out of scope.

Not measured: tail latency beyond `p90_us` in the raw reports, memory bandwidth, cold start other than CubeCL's launch, JSL end to end for large products, Rook or JessQL workloads.

### What a small call costs

`cargo bench --features compute-testing --bench main -- compute_overhead`, cycles per call over a
loop in JSL that costs 45:

| Call | Cycles | |
|---|---:|---|
| a native function | 47 | |
| a method, no arguments | 89 | |
| a method, a buffer and optional and variadic arguments | 111 | |
| `sum:runInto`, 1x4 | 386 | was 642 |
| `add:runInto`, 1x4 | 757 | was 1,208, and 872 at the start of this round |
| `matmul:runInto`, 2x2x2 | 675 | was 1,159 |

So L3i's binder costs about 110 cycles of these, and rewriting it would gain little. Most of the rest
is planning work per call: a 1x4 `add` from Rust is still 385 cycles and 1,236 instructions with no
allocations, against 114 for a row sum. Stepping one call shows it spread over binding sources and
operands rather than in any one place, so further work here gains little per change.

### What applications spend

JessQL's own bench suite under `jess run --profile`, at JessQL 0.1.6: the hottest lines are row-table
JSL in `engine.jsl`, closures reading `row[key]`, `table.sort` under a comparator reading a field, and
`table.clone` per result row, with 4.6 GB of allocation growth across the suite. Native calls such as
`data.argsort` barely register. The limit there is JessQL's row layout and the interpreter, not this
binder or these kernels, so neither binder work nor more numeric kernels would speed those queries.
Rook's worker journal bench needs a temporary managed target and was not run.

> Historical research record. Later implementations and measurements supersede several "not done" entries below. Consult `compute-results/contract.md`, `combined-baseline.md`, and `results/native-1f0e952.md` for the current contract, integrated baseline, and native CI evidence. Historical speedup claims are not fresh measurements.

# Compute architecture: decision and evidence

## Decision

**A CPU-only engine for x86-64 and AArch64: a small typed plan over precompiled SIMD kernels, growing by kernel family, with no
executable memory required. Do not adopt CubeCL. Do not build a JIT now: on a transformer-style pass matmul is about 90% of the work
and elementwise chains about 1%, and a generated loop gains at most about 1.35x in cycles on the elementwise work it can touch. See
"Direction" below.**

The first prototype's problem was granularity, not the absence of a compiler. It dispatched six
instructions for each output element, which cost 10 ns per element and kept the compiler from
vectorizing anything. Moving dispatch up to one per 1,024 elements gets within 1.4x of a handwritten
AVX2 loop at 65,536 elements, using fixed Rust loops with no run-time code generation, no new
dependency and almost no `unsafe`.

```mermaid
flowchart LR
    JSL[JSL builds typed nodes] --> Plan[Program: steps, signature, stage plan]
    Plan --> CPU[Chunked AVX2 or baseline kernels]
    Plan --> Shader[Generated JSL for Jess's shader frontend]
    Plan -.comparator.-> Luau[Generated --!native Luau]
    Plan -.oracle.-> Ref[Per-element reference interpreter]
    Buffers[Luau buffers, one call] --> CPU
    Owners[Immutable tensor owners, including host storage] --> CPU
    Shader --> GPU[Jess devices, buffers and dispatch]
```

## Performance

The timing tables in this section predate tiled kernels and the matmul microkernel; they describe the first chunked
revision and are kept as the record of how the interpreter was replaced. Current comparisons are in instructions, in
the CubeCL section.

Intel i7-10870H (8 cores, 32 KiB L1d, 256 KiB L2 per core, AVX2, no AVX-512), Linux 7.2, rustc 1.99.1
(LLVM 23.1.1), Clang 23.1.2, release with cross-language LTO. Three fresh processes, each row 31
samples of at least 3 ms from Rust or 2 ms from JSL. A value is the middle of the three process medians;
across those processes the medians moved by up to about 10%, more in rows marked below. Other work
was running on the machine (load average 3 to 8 of 16 threads). Raw reports with min and p90 are
in `results/compute-{1,2,3}.txt`.

### `relu(a+b+2)`: one contract, preallocated output, microseconds

| n | Handwritten fused AVX2 | Chunked | Ratio | Per-element reference | Speedup over reference |
| ---: | ---: | ---: | ---: | ---: | ---: |
| 0 | 0.002 | 0.075 | fixed cost | 0.018 | slower |
| 1 | 0.003 | 0.099 | fixed cost | 0.028 | slower |
| 16 | 0.005 | 0.113 | fixed cost | 0.173 | 1.5x |
| 1,024 | 0.062 | 0.241 | **3.9x** | 10.58 | 44x |
| 65,536 | 12.4 | 17.6 | **1.4x** | 682 | 39x |
| 1,048,576 | 253 | 333 | **1.3x** | 11,193 | 34x |

The 2-3x stretch target holds at 65,536 and 1,048,576 and **is missed at 1,024**, where everything
stays in L1 and the fused loop's single pass has no memory bottleneck to share with the kernels' three.
Sizes in between were not measured. There the gap is 0.18 us, less than one call from JSL costs
(next table, n=0).

### `relu(a+b+2)` from JSL, end to end, microseconds

Allocation and storage contracts differ by column and are stated.

| n | `runInto`: borrowed buffers, preallocated | `run`: fresh native result | `dream.data` 3 passes, preallocated | Generated Luau JIT, strict, preallocated | Handwritten Luau JIT, f64, preallocated | Handwritten, interpreter | Reference, fresh result |
| ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| 0 | 0.23 | 0.39 | 0.24 | 0.07 | 0.03 | 0.04 | 0.33 |
| 1 | 0.28 | 0.49 | 0.27 | 0.08 | 0.04 | 0.06 | 0.38 |
| 16 | 0.27 | 0.48 | 0.38 | 0.21 | 0.11 | 0.49 | 0.55 |
| 1,024 | 0.45 | 0.94 | 6.3 | 8.2 | 7.0 | 28 | 12.0 |
| 65,536 | 18.9 | 35.6 (p90 up to 74: GC) | 392 | 566 | 458 | 1,678 | 736 |
| 1,048,576 | 358 | 601 (p90 up to 1,588: GC) | 6,705 | | | | |

Luau is faster at 16 elements and below, where the call dominates. The handoff's 16.4 us Rust loop was
the default-target (SSE2) loop; this report compares like with like on AVX2 and shows both.

### Costs other than the kernel, from JSL at n=65,536

| Phase | Time | Notes |
| --- | ---: | --- |
| Build the graph | 5 to 7 us | Three typed nodes plus slots. Under 1 us in Rust. |
| Validate and plan | 5 to 7 us | Under 1 us in Rust. **No code is generated.** |
| Load and JIT the generated Luau | 270 to 390 us | The `program:native()` comparator; per program, per VM. |
| Bind inputs | at most 0.08 us | Bounded by the whole fixed cost in Rust at n=0. Zero copies. |
| Import two inputs as tensors | 40 to 90 us | One explicit copy each; only needed to retain them. |
| Result allocation | ~5 us | Rust: 22.2 fresh vs 17.6 preallocated. |
| Readback to a new Luau buffer | 26 us | One explicit copy. |
| First `runInto` | 22 to 80 us | Fresh pages. Warm: 19 us. |

Rust heap per library call, not counting Luau's wrapper objects: `run_into` 1 allocation of 128 bytes
(the bound sources), `run` 4 allocations including the result. 64 independent programs: 64 us to compile in total, then 16 us to run all of
them over 1,024 elements each. Peak budget in the JSL bench reached its 64 MiB limit by design: unreferenced
results accumulate until the budget forces a collection. Whole-process peak RSS was 94 to 98 MiB
for the complete bench, including all fixtures at n=1,048,576.

### Strided and broadcast inputs (preallocated, n=65,536)

| Input | Chunked | Reference | Speedup |
| --- | ---: | ---: | ---: |
| Contiguous | 17.6 | 682 | 39x |
| Padded (stride 8) | 57 | 705 | 12x |
| Transposed square | 87 | 738 | 8.5x |
| Column broadcast, 256 columns | 18.5 | 672 | 36x |
| Column broadcast, 2 columns | 68 | 736 | 11x |

Gathering is a separate loop, per chunk, so a strided input costs its walk and nothing else.

### Matrix multiplication (square, us; "scalar ops" counts multiply-adds; reference allocates, the rest are preallocated)

| Side | Scalar ops | Chunked AVX2 | Chunked baseline | Reference | Speedup | Handwritten Luau JIT |
| ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| 16 | 4,096 | 0.51 | 0.57 | 8.4 | 16x | 18 |
| 64 | 262,144 | 13.4 | 21 | 415 | 31x | 1,069 |
| 128 | 2,097,152 | 94 | 155 | 3,298 | 35x | |
| 256 | 16,777,216 | 796 | 1,207 | 37,811 | 47x | 68,781 |
| 400 | 64,000,000 | 2,668 | 4,634 | 102,681 | 38x | |

About 24 multiply-adds per nanosecond at the top end (0.042 ns each), single-threaded and strictly
ordered. Results are bit-identical to the reference on every row, asserted during the bench.

## The questions

**1. Why was the executor so much slower than a native loop?** Per-element dynamic dispatch through a
checked, strided read: 10.3 ns per element against 0.2.

**2. How much does each cost?** The ladder, for `relu(a+b+2)` at n=65,536, is below. Costs are not
additive, because the first thing dispatch or a checked read destroys is vectorization.

| Step, each measured on its own | us | ns/element |
| --- | ---: | ---: |
| Fused loop over `f32` slices, AVX2 | 13.8 | 0.21 |
| Fused, over `[u8; 4]` little-endian elements, AVX2 (**byte conversion: no cost**) | 13.9 | 0.21 |
| Fused, default target (SSE2) | 16.0 | 0.24 |
| Three monomorphic passes, AVX2 (**no fusion, cache traffic: 1.7x**) | 23.5 | 0.36 |
| Chunked kernels, AVX2 | 17.7 | 0.27 |
| Fused, through the checked strided read, no dispatch (**indexing: 5.6x**) | 78 | 1.18 |
| Per-element dispatch over direct slices, no checked read (**dispatch: 30x**) | 411 | 6.3 |
| Reference: dispatch and checked read | 679 | 10.4 |
| Reference with result allocation (**allocation: lost in the noise**) | 673 | 10.3 |

Three more passes through `dream.data` take 392 us, against 23.5 for the same three passes written
monomorphically. The difference is a per-element match on the element kind and an f64 round trip,
not the number of passes.

**3. Can L3i's native lowering execute tensor operations?** Not usefully. Luau's IR has scalar buffer
loads with a bounds check per access (`results/luau-relu-sum-ir-and-x64.txt`), no vector loads from a
buffer, and no way to read external storage. Reusing the host hooks would still mean a scalar loop per
element, and each program costs about 300 us to load and JIT per VM.

**4. Can Luau's JIT optimize structured numerical loops?** Yes, as scalar code. Native execution is about 4x
the interpreter's speed, and 7 ns per element is good for what it does. It remains 24x slower than the
kernels at 65,536 because it does not vectorize. It stays the right tool for irregular per-element
logic that bulk kernels cannot express. All of this requires a sandboxed VM: without one, the same
loop measures 110 ns per element.

**5. Is a small compute IR justified?** Yes, a very small one. The plan is about 220 lines of planner
and six instruction kinds. An eager API cannot supply what it provides: signature checks, register
reuse, and one description for CPU and GPU. Anything larger is not justified by these numbers.

**6. Is fusion necessary, and at what level?** Chunk-level fusion is worth having but is not the main
effect. It saves about 25% against whole-array passes at 65,536 (17.7 vs 23.5) and removes
array-sized temporaries. Element-level fusion, the only thing generated code could add, buys about 1.4x on a short expression beyond L1,
and about 1.5 to 2.6x on a 16-operation chain beyond L1 (see the CubeCL section; an earlier version of this answer
said 1.4x and was too low). Inside L1 it buys up to 3.9x, at most 0.18 us per call at n=1,024. Whether that justifies
per-architecture code generators, executable memory and a code budget is open. A cheaper candidate to measure first
is a set of fused kernels for common shapes such as `relu(a*b+c)`, which remove stores without generating code.

**7. Should elementwise work use `dream.data`?** Its explicit calls should stay: they are eager, cover eight
element kinds and define ordered overlap, which this design rejects. But they could be far faster
under the same contract. Monomorphizing the contiguous `f32` loops, with an overlap check to keep the
ordered semantics, has about 16x available (392 to at best 23.5). That is a separate production change
and has not been made here.

**8. Which ownership abstractions are reusable?** `Storage` + `Tensor` + `View` (immutable owners,
checked views), `Budget` and `Domain` (host-owned accounting). They carried over with minimal change,
and removing captured inputs made them simpler. `View` is the one thing kernels read.

**9. Would CubeCL provide something unavailable?** On CPU, faster long expressions over large arrays, but fused
kernels or generated code of our own could offer the same. GPU and CubeK are unmeasured. See below.

**10. Compiler, scheduling, or both?** First an execution-granularity problem, which took most of the gain. What
remains on long expressions is a fusion problem, so partly a compiler one. Scheduling arrives with multi-threading,
and that belongs to Jess.

## Direction: a CPU-only engine, with JIT optional and never required

Decided after the experiments below. L3i's numerical backend targets **x86-64 and AArch64 CPUs**, with GPU computation left to
Jess. Every program must run with no executable memory, because consoles and other restricted platforms forbid it, and
Luau's own native code is optional for the same reason. Precompiled kernels are the engine; anything generated at run time
is an optional tier that must first earn its place.

**The no-JIT configuration is now the default and is tested.** `luau-jit` is an opt-in feature used only by the generated-Luau
comparator. Without it, the JSL test also asserts that no anonymous executable memory is mapped. AArch64 has not been compiled, run or
measured for this crate, and a dedicated NEON backend does not exist yet.

### Counters, not clocks

`compute_cubecl` counts retired instructions, cycles, L1 data misses and last-level misses in a disposable container, with no wall
clock anywhere. Cycles expose the memory stalls instructions cannot, but they are sensitive to load: these runs had a load average
up to 17 from other work, so cycle figures are the best of three passes and differences under about 1.2x are noise. Instruction
counts repeat almost exactly. Cycles sum all threads, so CubeCL's include its threads' waiting and should not be read as its speed.

### What a JIT could buy: the ceiling

`benches/compute/cubecl/jit.rs` is a research-only generator: any elementwise program from the validated plan becomes one AVX2 loop. It
is under 400 lines including a small VEX encoder, compiles in 7,000 to 29,000 instructions into 72 to 168 bytes, and matched the reference
bitwise on all 393 random graphs it accepted (it refused 7 that needed more than sixteen registers). It is not on any production path.

One thread, strict, tiles against one generated loop. Thousands of instructions, then thousands of cycles.

| Workload | n | Tiles, instructions | Generated, instructions | Tiles, cycles | Generated, cycles |
| --- | ---: | ---: | ---: | ---: | ---: |
| `relu(a+b+2)` | 65,536 | 65 | 74 | 65 | 64 |
| | 1,048,576 | 1,013 | 1,180 | 1,273 | 1,240 |
| 16-operation chain | 65,536 | 360 | 221 | 152 | 197 |
| | 1,048,576 | 5,718 | 3,539 | 2,891 | 3,379 |
| Polynomial | 65,536 | 302 | 172 | 158 | 139 |
| | 1,048,576 | 4,793 | 2,753 | 3,109 | 2,496 |
| Residual DAG | 65,536 | 203 | 131 | 110 | 88 |
| | 1,048,576 | 3,214 | 2,097 | 2,191 | 1,616 |

**A generated loop retires up to 1.7x fewer instructions but takes at most about 1.35x fewer cycles, and on the 16-operation chain
takes more.** Tiles have LLVM's unrolling and scheduling, and the prototype emits one unoptimized vector per iteration, so this
understates what a mature generator could do. It still sets the scale: the prize is tens of percent on elementwise work.

The compact vector interpreter proposed as a third option is rejected: 66 instructions per element on the chain against 5.5 for
tiles, and about 10x the cycles.

### Where a real workload spends its work

`benches/compute/cubecl/model.rs` runs a transformer-style forward pass (64 tokens, width 128, two blocks, one head, synthetic weights)
with layer normalization, scaled dot-product attention, residuals and a feed-forward layer. Operations the plan cannot express yet
are shared native loops, labelled below. Share of the fast mode's median pass:

| Family | Instructions | Cycles |
| --- | ---: | ---: |
| **Matmul** | **94.2%** | **85.0%** |
| Normalization (native) | 2.4% | 6.3% |
| Softmax (native) | 1.3% | 3.8% |
| Elementwise (plan) | 1.1% | 2.9% |
| Transpose (native) | 1.0% | 1.9% |

**Elementwise fusion is about 1% of this workload, so a JIT for elementwise chains could not move it.** It is the wrong problem for
inference, though it still matters for simulation-style work, which should be measured on its own terms. Matmul is the right one.

Against a packed BLAS-style baseline (`matrixmultiply`, FMA) in the same pass, this revision's kernel measures:

| Matmul, total for one pass | Instructions | Best cycles | L1 misses |
| --- | ---: | ---: | ---: |
| Ours, strict | 6,218,000 | 2,593,000 | 264,700 |
| Ours, fast | 4,371,000 | 1,741,000 | 271,500 |
| `matrixmultiply` | 4,153,000 | 1,533,000 | 189,500 |

Two changes got it this close, with strict results still bit-identical to the triple loop: packing `b` per block, and a one-row
register kernel for rows left over from the six-row block, which took the fused pass from 5.36M to 4.37M instructions. The remaining
gap is about 5% in instructions and about 14% in best cycles, with more L1 misses; packing `a` for all row groups, as BLAS does, is
the obvious next step. Strict mode pays for separate rounding and cannot match an FMA kernel.

This is one small synthetic model, on one Intel CPU, with quantization, large weight streaming and KV caches absent. It shows the
split for this shape, not all inference.

### Conclusions

1. **Do not build a JIT now.** The measured ceiling is modest and does not touch the workload that matters most.
2. **Grow kernel families, not combinations.** The three-step tile alphabet stays as it is; the generic combination count should not
   grow with the operation set. New work goes into matmul, then normalization, softmax and reductions, then quantized forms.
3. **Build a real AArch64 backend and test it on real hardware.** Nothing here says anything about ARM yet.

## CubeCL verdict: not adopted; matmul and short work won, long expressions on large arrays not

`compute_cubecl` runs both engines on the same inputs, in separate processes, inside a disposable container
(`run-in-container.sh`, built on the host, no network, removed on exit). The metric is **retired user-space
instructions across all threads**, from the CPU's counter. Counts on our side repeat to within a few instructions
in 22 million, so unlike the timings they can separate the engines on a shared machine. Raw reports are in
`results/compare-*`.

CubeCL gets its best configuration: `Vector` kernels at 1, 8 and 16 lanes (the table takes its lowest count), the
better per contract of a four-row and a six-row matmul kernel, and the same blocking we use. An earlier revision of
this document called CubeCL 26x slower. That figure compared its old bridge, with copies and a cube size of 256, with
our kernels, not the two engines.

### CubeCL's CPU backend is not strict by default

Its optimizer fuses every `a * b + c` whose product has no other use into an FMA, unconditionally, with no opt-out
(`cubecl-opt`'s `InstCombine`). Its output differs from separately rounded f32 and matches an FMA oracle exactly
(`results/compare-diagnosis.txt`). So there are two contracts to compare. The strict rows give each CubeCL product a
never-taken second use, after which every one matches the strict oracle bitwise, including NaN, signed zero and
subnormal edges. The fast rows compare our explicit `Numeric::Fast` with its default. The never-taken branch might
cost CubeCL a little in the strict rows; we did not inspect its generated code, so the fast rows are the cleaner test.

### Strict against strict, one thread, kernel only (thousands of instructions)

| Workload | n | Ours | CubeCL | Fewer instructions |
| --- | ---: | ---: | ---: | --- |
| `relu(a+b+2)` | 16 | 1.6 | 52.6 | ours, 33x |
| | 1,024 | 2.4 | 49.2 | ours, 20x |
| | 16,384 | 17.2 | 63.0 | ours, 3.7x |
| | 65,536 | 64.7 | 109 | ours, 1.7x |
| | 1,048,576 | 1,013 | 1,001 | tie |
| 16-operation chain | 1,024 | 8.7 | 52.3 | ours, 6x |
| | 16,384 | 92 | 106 | ours, 1.15x |
| | 65,536 | 360 | 281 | **CubeCL, 1.28x** |
| | 1,048,576 | 5,718 | 3,677 | **CubeCL, 1.55x** |
| Degree-8 polynomial | 1,024 | 7.7 | 50.3 | ours, 6.5x |
| | 16,384 | 78 | 92 | ours, 1.18x |
| | 65,536 | 302 | 217 | **CubeCL, 1.39x** |
| | 1,048,576 | 4,793 | 2,777 | **CubeCL, 1.73x** |
| Residual-shaped DAG | 1,024 | 5.3 | 49.4 | ours, 9.4x |
| | 16,384 | 52 | 79 | ours, 1.5x |
| | 65,536 | 203 | 173 | **CubeCL, 1.17x** |
| | 1,048,576 | 3,214 | 2,036 | **CubeCL, 1.58x** |
| Matmul | 240x240 | 5,483 | 7,272 | ours, 1.33x |
| | 384x384 | 22,137 | 29,354 | ours, 1.33x |
| Two-layer network | 240 wide | 11,009 | 17,794 | ours, 1.62x |

### Our fast mode against CubeCL's default, one thread, kernel only (thousands of instructions)

| Workload | n | Ours, `Numeric::Fast` | CubeCL default | Fewer instructions |
| --- | ---: | ---: | ---: | --- |
| 16-operation chain | 16,384 | 82 | 93 | ours, 1.13x |
| | 65,536 | 319 | 226 | **CubeCL, 1.42x** |
| | 1,048,576 | 5,067 | 2,838 | **CubeCL, 1.79x** |
| Degree-8 polynomial | 16,384 | 56 | 73 | ours, 1.29x |
| | 65,536 | 219 | 152 | **CubeCL, 1.44x** |
| | 1,048,576 | 3,469 | 1,694 | **CubeCL, 2.05x** |
| Residual-shaped DAG | 65,536 | 203 | 161 | **CubeCL, 1.26x** |
| | 1,048,576 | 3,214 | 1,813 | **CubeCL, 1.77x** |
| Matmul | 240x240 | 3,753 | 4,418 | ours, 1.18x |
| | 384x384 | 15,056 | 17,674 | ours, 1.17x |
| Two-layer network | 240 wide | 7,550 | 10,765 | ours, 1.43x |

Our fast mode does not fuse the residual DAG: its products are not adjacent to their adds in the plan.

### Eight threads, total work, not speed

These count all threads' instructions including any spinning, so they measure overhead and say nothing about how
soon the work finishes. Our threads are a benchmark-only rayon pool; L3i has no threading of its own.

| Workload | n | Ours | CubeCL strict |
| --- | ---: | ---: | ---: |
| `relu(a+b+2)` | 65,536 | 313 | 360 |
| | 1,048,576 | 1,325 | 1,263 |
| 16-operation chain | 65,536 | 637 | 547 |
| | 1,048,576 | 6,132 | 4,307 |
| Polynomial | 1,048,576 | 5,194 | 2,983 |
| Matmul | 240x240 | 5,845 | 7,565 |
| | 384x384 | 23,025 | 30,403 |

### Compile cost

Building and compiling a graph costs ours 25,000 to 78,000 instructions. A first launch costs CubeCL 17.6 million
on average and up to 148 million, about 225 to 700 times as much, per kernel and width. Its client initialization is
another 0.5 million.

### What this metric cannot see

- **Memory stalls and port pressure.** An instruction that waits on memory counts the same as one that does not, and
  the workloads where CubeCL leads are the ones that exceed cache. Treat large-array ratios as evidence about code,
  not as a promise about wall time.
- **Copies.** `memcpy` retires few instructions per byte, so CubeCL's copy-in and readback rows look nearly free here
  (1.14 million instructions at n=1,048,576, against 1.00 million for the kernel alone). They are not: under the earlier
  timing they cost about 10x.
- **Parallel speedup**, as above.

### What this establishes

We lead on everything small or medium, on every matmul, and on the network. Below about 16,384 elements that is
CubeCL's roughly 50,000-instruction dispatch, a cost of its queue and pool. **On long expressions from somewhere
between 16,384 and 65,536 elements upward, CubeCL still leads, by 1.2x to 2x.** The short expression ties.

That remaining gap is structural. A tile costs a load, a store and loop control per vector, so a 16-operation chain
in six tiles pays those six times, while one generated loop pays them once. More compiled combinations narrow it but
cannot close it: a useful alphabet at three steps is about 2,400 loops per instantiation, with the build time and
binary size that implies, and it would still leave two tiles on the chain. Only generating one loop per expression
removes it, and the ceiling measurement above shows that buys less in cycles than in instructions.

We do not adopt CubeCL because of its compile cost, bundled LLVM, unbounded compile limits, silent FMA and overlap
with Jess's GPU ownership. Not measured: GPU execution, which is CubeCL's main purpose, and CubeK's matmul.

## Correctness

This revision passes debug and release tests. Beyond the targeted tests for each contract in the crate
README:

- **Randomized oracles.** 400 random DAGs over random shapes, broadcasts and layouts (any offset, stride
  zero through overlapping through padded) compare the chunked kernels on both instantiations and four
  chunk sizes against an independent scalar evaluator and the reference, bitwise. 120 random matrix
  products do the same against an independent ordered triple loop.
- **Injected bugs.** `results/mutations.txt`: 24 deliberate defects are all caught, and a comment-only control
  survives. Both rounds were needed: the first round on the original kernels had three survivors (reversed matmul
  order, FMA in matmul, folded `relu(-0)`, hidden because NaN and wild exponents swamp rounding), and the first round on
  tiled kernels had three more (untested fused forms, a multiply fused into a non-add, a strict matmul given the fusing
  instantiation). Each led to a new test.
- **The `runInto` overlap check** is now a byte-range check, tested on sub-ranges with different starts. Start-address
  equality relied on callers passing whole buffers.
- **Generated shader.** `jess run generated-shader.jsl` passes against reference results across three input
  sets, on Jess's existing device, buffers, pipeline and readback. Only the `integer-exact` mode exists;
  `strict` is rejected. GPU throughput has not been benchmarked: this proves lowering and resource reuse.

Teardown: the JSL test runs twice with fresh VMs; everything must return to zero live payload after
collection, and a retained program survives collection and runs on new inputs.

## Not done

- **Multi-threading.** Every number here is one thread. Chunks are independent given their position, so
  a scheduler hook is the natural seam, and it belongs to Jess.
- **A wider operation set and general rank.** Inference needs more: at minimum subtract, divide, min,
  max, exp, reductions, a softmax's worth of ordering rules, and rank above two. Each new elementwise
  operation is a kernel and a numerical contract, not a compiler change.
- **A strict GPU mode**, and any GPU throughput measurement on real hardware.
- **Jess integration**: a `Storage` adapter for `SharedBuffer`, generation-owned programs, and budgets
  from Jess's resource policy. The shader lowering emits JSL source; a production path would lower to
  Jess's typed shader program in-process.
- **A public `Call::collect_garbage`** in L3i, needed before this leaves research.
- **`dream.data` monomorphization** (question 7).
- **Scalar reads of shared immutable buffers from Luau.** Direct, guarded reads of external storage from
  generated Luau remain unexplored. Bulk kernels make it matter less for numerical work; it is still open for scalar code.
- **aarch64 and AVX-512 measurements.** The baseline instantiation compiles for aarch64 but has not been
  run there, and this hardware has no AVX-512.
- **An AArch64 backend**, tested on real hardware.
- **Packing `a` for all row groups in matmul**, then normalization, softmax and reduction kernels.
- **An opt-in FMA mode.** Worth about 10% on matmul and 20 to 30% on the chain here. It changes results, so it
  must be explicit.
- **A packed matmul microkernel.** Both engines here are unpacked.
- **CubeCL on GPU and CubeK**, if a workload makes them relevant.

Compute shares L3i's minimum supported Rust, 1.95, by decision. Measurements above name the rustc they ran on.
