# Row kernels

Ordered sums, accurate softmax and accurate normalization all run as graph stages, from Rust as `Expr::sum_rows`, `Expr::softmax` and `Expr::normalize`, and from JSL as `sumRows`, `softmax`, `rmsNorm` and `layerNorm`. The primitives in `reduction.rs` are the kernels those stages call. They are research APIs, not a supported tensor API.

## Numerical contracts

These hold in both `Numeric` modes. No row kernel uses a fused multiply-add, and every kernel instantiation returns the same bits, so results do not vary between x86-64 instantiations or, by construction, between architectures. AArch64 agreement with x86-64 has not been compared directly: CI runs the same oracle tests natively on each.

- **Ordered sum.** Starts at +0 and adds columns in increasing order, rounding to f32 after every addition. Empty rows produce +0.
- **Softmax.** Subtracts the row's maximum in f64, takes one exponential per element in f64, accumulates in f64, and rounds each probability to f32.
  - Any NaN poisons the row.
  - Positive infinities share probability equally.
  - An all-negative-infinity row (all masked) produces zeros.
- **Normalization.** Computes the population mean (when centered) and squared deviations in f64, then writes `(x - mean) / sqrt(variance + epsilon)` rounded to f32. RMS normalization is the same with no mean. Nonfinite inputs follow IEEE arithmetic. Epsilon must be finite and strictly positive and is checked when the expression is built. There are no affine weights; gain and bias are ordinary graph arithmetic.

F64 sums accumulate into four totals by index modulo four, then add the totals in order. That order is part of the contract: it is what lets the loops vectorize without reassociating anything, and every instantiation follows it.

### Why the exponential is ours

The first kernel called the platform's `f64::exp` twice per element. That cost about 13 ns per element, and its bits depended on the host's math library, so "accurate softmax" was not one result across platforms.

`reduction::exp_nonpositive` replaces it for `x <= 0`, the only range softmax needs after subtracting the maximum:

- Cody-Waite reduction, a degree-13 Taylor polynomial evaluated by Estrin's scheme, and a scale assembled from the bits of the rounded quotient.
- Branchless and built only from IEEE operations, so it vectorizes at any width and returns the same bits everywhere.
- Inputs below -104 return +0. `exp(-104)` is under half the smallest f32 subnormal and every denominator is at least 1, so such a probability rounds to +0 regardless, and a million such terms cannot move a denominator.

Tests hold it within 1 f64 ulp of an independent double-double oracle on 200,000 random points and at every reduction boundary, and within 3 ulps of the platform's `exp`. Softmax results stay within 1 f32 ulp of the oracle, and fewer than 1 in 1,000 differ from the correctly rounded value.

## Stage execution and resources

Inputs are read in place when a row's columns are adjacent in memory, even if the rows themselves are spread apart. Outputs are always written in place.

| Stage | Scratch charged to the budget | Scratch when columns are spread apart |
| --- | --- | --- |
| Sum rows | None | None (reads through the checked per-element path) |
| Normalize | None | One f32 row |
| Softmax | One f64 row, rounded up to a multiple of 4 elements | Also one f32 row |

Scratch is acquired once per stage, reused across its rows, and released before its charge. A failed charge happens before any write to the final output. A stage whose result has no elements reads nothing, charges nothing, and does not run the stages only it reads.

## Softmax strategies measured

Single row, one thread, preallocated output, on an i7-10870H under mixed load. Run `cargo bench --features compute-testing --bench main -- compute_softmax` to reproduce; raw reports are in `../results/l3i-softmax-strategies*.txt`.

| Strategy | ns per element at 4,096 | Contract |
| --- | ---: | --- |
| A. Platform `exp` twice, no scratch (original) | 10.8 | Accurate, host-dependent bits |
| B. Platform `exp` once, f64 scratch | 5.9 | Accurate, host-dependent bits |
| C. Deterministic `exp` once, f64 scratch, whole program (**kept**) | 2.7 | Accurate, identical bits |
| D. Deterministic `exp` twice, no scratch | 11.6 | Accurate, identical bits |
| E. f32 approximation with FMA | 0.8 | Approximate (below) |

Strategy D was slow because a loop that accumulates while it computes the exponential did not vectorize, so it ran scalar. Strategy C uses separate loops, each of which does.

Strategy E is about 3.4x faster than C, and it stays an experiment in `bench.rs` with no way to reach it from a program:

- Against the f64 oracle, its maximum absolute error is about 3e-7 and its normalization error is about 1e-7.
- Its maximum relative error reaches 1.0: logits more than about 87 below the maximum flush to zero where the accurate result is still nonzero, including the wide and extreme rows measured.
- The large-offset row also reaches relative error 1.0, because the subtraction happens in f32.

That is the failure mode the earlier synthetic transformer experiment found, so a fast path of this kind would need its own contract and a reason. Workspace reuse (the review's strategy C) is covered in the graph-cpu README under resources.

## Testing

`adversarial.rs` compares every stage in four memory layouts and every available instantiation, which must agree to the bit, against double-double oracles that share no code with the kernels. It uses row widths from 1 to 257 and 8 families of adversarial values: moderate, wide, nearly constant, large finite, subnormal, signed zero, large offset and near the cutoff.

Other tests cover special rows sharing a matrix, repeated rows and elements through zero strides, exact budget boundaries and recovery. `research/mutate.jsl` injects bugs one at a time and is the evidence that these tests detect them; its record is `../results/mutations.txt`.

The reference interpreter computes each element independently through `View::at`, but it follows the same contracts and shares `exp_nonpositive`, so it is a check on layouts and row handling and not on the exponential itself. GPU and generated-Luau lowering reject these stages explicitly rather than change their meaning.

## Measurements

Whole program invocations with preallocated output, one thread, median of 31 samples, on the same host under mixed load. Treat differences under about 10% as noise. Before is `0b95ca3`; after is `f8342b3`.

| Stage and shape | Before (us) | After (us) | Ratio |
| --- | ---: | ---: | ---: |
| Softmax 64x128 | 106 (JSL) | 21 | 5.1x |
| Softmax 128x128 | 206 | 42 | 4.9x |
| Softmax 1x4096 | 53 | 11 | 4.8x |
| Layer normalization 64x128 | 27 | 8.1 | 3.4x |
| Sum rows 64x128 | 7.0 | 1.8 | 4.0x |
| Sum rows 1x4096 | 3.9 | 3.7 | 1.0x |

A single long row sum does not improve, and cannot under this contract: one ordered sum waits for each addition's latency. The gain comes from running independent rows side by side.

Very short rows pay a fixed cost per row of about 50 ns, so softmax at 1000x7 costs about 7.7 ns per element against 2.5 for long rows. Single-column rows pay the same fixed cost, though their result is trivial. Nothing here has been measured on AArch64 unless the PR's tables say so.
