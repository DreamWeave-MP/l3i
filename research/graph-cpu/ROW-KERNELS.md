# Experimental row kernels

`reduction` contains allocation-free primitives, not yet graph instructions or JSL operations. They are a correctness foundation, not a supported tensor API.

- `sum`: starts at positive zero, adds in increasing index order with f32 rounding. No reassociation or FMA.
- `softmax`: subtracts the row maximum, uses standard f64 exponential and f64 accumulation, rounds final probabilities to f32. This accurate contract differs from strict-f32 elementary arithmetic. Any NaN poisons the row; positive infinities split probability equally; an all-negative-infinity row produces zeros. Empty rows are accepted. Equal input/output lengths are required; Rust borrows prevent overlap.
- `normalize`: population layer normalization when centered, RMS normalization otherwise; f64 mean and squared deviations, f64 square root, final f32 output. Finite positive epsilon and matching lengths are checked before output writes. No affine weights yet. Finite f32 extremes do not overflow f64 statistics. Nonfinite inputs follow IEEE arithmetic.

Tests cover analytic normalization, independent unshifted softmax probabilities, translation invariance, long rows, infinities, NaNs, extreme logits, zero dimensions, cancellation, subnormals and validation before mutation. More randomized validation, mutation testing, graph integration and JSL measurements remain outstanding.

Kernel-only baseline: results/l3i-row-baseline.txt and l3i-row-rss.txt in the parent research directory. Preallocated outputs, single thread, standard repository release/LTO configuration; 31 timed samples per row on the same i7-10870H host. At n=1024: ordered sum 0.997 us, softmax 14.176 us, RMS norm 1.767 us, layer norm 2.714 us. These are initial observations under variable load, not improvements or end-to-end claims. Later portions of the mixed report overlap verification compilation; use the initial row-kernel section for these observations.

Softmax currently recomputes exponentials to avoid scratch and intermediate f32 rounding. Compare a charged f64 workspace before changing this accuracy/storage tradeoff. Normalization deliberately uses two-pass centered variance rather than subtracting nearly equal second moments.

## Ordered sum graph integration

`Expr::sum_rows()` / JSL `expression:sumRows()` is now a materialized typed stage. It produces `(rows, 1)`, starts each sum at +0 and visits columns in order in both numerical modes. Empty rows produce +0. Input views remain borrowed, including strided views; stages participate in existing work bounds and temporary lifetime accounting. The reference interpreter evaluates its own ordered loop. GPU and generated-Luau lowering reject this new stage explicitly.

`--bench-rows` measures ordered sums at kernel, program and actual JSL borrowed-buffer levels. The initial full-invocation JSL medians were 1.271 us at 1,024 elements, 71.858 us at 65,536, and 1,149.856 us at 1,048,576. Raw results: ../results/l3i-sum-levels*.txt. Normalization and softmax remain standalone primitives pending stage integration; the introductory limitation applies to those operations.

## Accurate softmax graph integration

`Expr::softmax()` / JSL `expression:softmax()` now preserves shape and normalizes each row. Its accurate f64-statistics contract applies in both graph numerical modes. The stage shares the standalone kernel, accepts strided views, and charges two f32 conversion rows (8 bytes per column) before allocation. Zero-row inputs require no scratch. Storage is reused across rows within the call and released before its budget charge. Allocation failure occurs before final output writes.

The reference interpreter computes each result independently from the row, without calling the production kernel. Tests include transposed random inputs, NaNs, positive infinities, all-masked rows, extreme finite logits, empty dimensions, composition with ordered sums, JSL invocation and insufficient scratch budgets. Reference execution is intentionally quadratic in row width and is not a performance path. GPU/generated-Luau lowering rejects softmax rather than silently changing semantics.

Initial program-versus-kernel medians: 13.427 versus 11.282 us at 1,024 elements, 891.255 versus 772.061 us at 65,536. See ../results/l3i-softmax-levels*.txt. This report includes sum JSL timings, not softmax JSL timings; no softmax end-to-end claim follows. Softmax is not yet optimized: two exponential evaluations per element and conversion-row allocation remain visible costs. Normalization is still standalone pending graph integration.
