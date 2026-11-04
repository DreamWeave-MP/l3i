# Experimental row kernels

`reduction` contains allocation-free primitives, not yet graph instructions or JSL operations. They are a correctness foundation, not a supported tensor API.

- `sum`: starts at positive zero, adds in increasing index order with f32 rounding. No reassociation or FMA.
- `softmax`: subtracts the row maximum, uses standard f64 exponential and f64 accumulation, rounds final probabilities to f32. This accurate contract differs from strict-f32 elementary arithmetic. Any NaN poisons the row; positive infinities split probability equally; an all-negative-infinity row produces zeros. Empty rows are accepted. Equal input/output lengths are required; Rust borrows prevent overlap.
- `normalize`: population layer normalization when centered, RMS normalization otherwise; f64 mean and squared deviations, f64 square root, final f32 output. Finite positive epsilon and matching lengths are checked before output writes. No affine weights yet. Finite f32 extremes do not overflow f64 statistics. Nonfinite inputs follow IEEE arithmetic.

Tests cover analytic normalization, independent unshifted softmax probabilities, translation invariance, long rows, infinities, NaNs, extreme logits, zero dimensions, cancellation, subnormals and validation before mutation. More randomized validation, mutation testing, graph integration and JSL measurements remain outstanding.

Kernel-only baseline: results/l3i-row-baseline.txt and l3i-row-rss.txt in the parent research directory. Preallocated outputs, single thread, standard repository release/LTO configuration; 31 timed samples per row on the same i7-10870H host. At n=1024: ordered sum 0.997 us, softmax 14.176 us, RMS norm 1.767 us, layer norm 2.714 us. These are initial observations under variable load, not improvements or end-to-end claims. Later portions of the mixed report overlap verification compilation; use the initial row-kernel section for these observations.

Softmax currently recomputes exponentials to avoid scratch and intermediate f32 rounding. Compare a charged f64 workspace before changing this accuracy/storage tradeoff. Normalization deliberately uses two-pass centered variance rather than subtracting nearly equal second moments.
