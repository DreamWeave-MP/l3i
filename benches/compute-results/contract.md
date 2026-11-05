# Reusable f32 computations over chunked native kernels

The `@dream/compute` extension (`src/compute`, feature `compute`). Focused native x86-64 and AArch64 CI tests it. The decision and its evidence are in
[`../compute-results.md`](../compute-results.md); this page is the contract. From the repository root, so the
verified Clang/LTO configuration applies:

```sh
cargo test --release --features compute-testing compute::
cargo test --release --features compute-testing,jit compute::
```

## From JSL

```luau
local compute = require("@dream/compute")
local x, y = compute.slot(0, 1, n), compute.slot(1, 1, n)
local program = x:add(y):add(compute.constant(2)):relu():compile()

program:runInto(out, a, b)        -- Luau buffers, borrowed for this call: no copy, no result object
local r = program:run(ta, tb)     -- immutable tensors: retained owners, a fresh immutable result
local bytes = r:read()            -- explicit copy into a new Luau buffer
```

A program holds typed structure and nothing else. Inputs bind by position on every call, so one
compiled program runs against any compatible inputs, retains none of them, and can run on several
threads at once (`Program: Send + Sync`).

## What executes

| Layer | Runs as |
| --- | --- |
| Building and compiling a graph | JSL calling bound Rust: 4 to 7 us each from JSL, under 1 us from Rust. No code generation. |
| Elementwise stage | Rust kernels compiled ahead of time, chosen once per run (AVX2 or baseline), dispatched **once per tile per 1,024-element chunk** |
| Matrix multiply | One ahead-of-time Rust kernel: 6x16 and 1x16 output blocks held in registers over packed right-hand panels and directly broadcast left-hand values, in ascending blocks of `k` |
| `program:native()` | Generated `--!native` Luau, run by Luau's JIT: scalar, Luau buffers only. A comparator, not the execution path. |
| `program:shader_source('integer-exact')` | Generated JSL for Jess's existing shader frontend, device and dispatch |
| `program:runReference` | The first prototype's per-element interpreter: oracle and baseline only |

Nothing generates machine code at run time, so there is no code cache to bound and no W^X policy. No-JIT is the default
configuration, and its test asserts no anonymous executable memory is mapped; `luau-jit` enables Luau's generator for the
generated-Luau comparator only.
Disassembly in [`kernel-instructions.txt`](kernel-instructions.txt) shows
`vaddps`, `vmulps` and `vcmpnleps` on `ymm` registers, and no fused multiply-add.

## How a graph becomes a plan

`Expr::compile` validates the DAG (shared nodes stay shared; a 200-level doubling chain is 201
steps, not 2^200) and splits it at matrix multiplications. Each elementwise run becomes a list of
tiles, each one compiled loop that holds a running value in a SIMD register across up to three steps, where an
operand is a constant, a register or an input. A value with one reader merges into its reader's tile:

```text
relu(a + b + 2):
  output <- input0, Add input1, Add 2, Relu
```

Constants fold using the operation the kernel would perform. Registers are reused once their last
reader has run, and a destination is never one of its own operands. Temporaries between stages are
charged to the budget and released after their last reader.

Each input resolves at bind time to one of three forms, against the output's shape:

- **Direct**: its elements are already in output order, so a kernel reads them in place.
- **Splat**: one value for every output element.
- **Walk**: anything else (padding, a transpose, a broadcast along a varying row), gathered into a
  scratch register per chunk.

Elements are `[u8; 4]`, so inputs may sit at any byte offset and the same safe code reads registers,
Luau buffers and host storage. LLVM vectorizes through the byte conversion: it costs nothing
measurable. The `unsafe` in the kernels is calling a SIMD instantiation after detecting it, and one unchecked index in the fused
loops, whose slices are asserted to the output's length, because a checked index forces a scalar tail on every chunk.
The host adds its `Userdata` declarations and one-call Luau buffer borrow, using L3i public garbage collection.

## Numerical contract

Each add and multiply rounds to f32 separately. The kernels use only Rust's `+`, `*` and `<=`, which
grant LLVM no contraction or reassociation, on either instantiation. ReLU is `x <= 0 ? +0 : x`: NaN
stays NaN, both zeros become +0. Subnormals are preserved; nothing sets flush-to-zero. NaN payload
bits are not promised. Matmul starts each element at +0 and adds in increasing `k`. Its `i-k-j` loop
and block order preserve that per element, and it never skips a zero, since `0 * inf` is NaN.

Tests compare every kernel instantiation bitwise (any NaN equals any NaN) against a scalar evaluator
written independently in the tests and against the reference interpreter, across 400 random
graphs and 120 random matrix products. See "Correctness" in the findings for what those tests were
shown to catch.

`native_source` matches this contract through a typed `vector` temporary per operation: an f64 add or
multiply rounded to f32 equals the f32 operation, because f64 has more than `2p + 2` bits, so the
double rounding is innocuous. The WGSL path does not
claim strictness: `integer-exact` proves by interval arithmetic that every value stays an exact f32
integer, checks every input at run time, and rejects everything else.

`Numeric::Fast`, chosen explicitly at compile time (`compileFast` from JSL), fuses a multiply into its add where the
host has FMA. Results then depend on the host and on the plan's tiling, and a fast program without FMA runs its strict
tiles. Everything else on this page describes the strict default.

## Ownership and resources

- Luau buffers are never retained. `runInto` borrows them for one call in which no Lua runs, and
  rejects an output whose byte range overlaps any input's. Inputs may alias each other.
- `Tensor::from_storage` retains an existing immutable owner without a copy; this is the seam for a
  Jess `SharedBuffer` adapter. Derived views keep the owner alive and may address only complete elements authorized by the parent, including for padded and transposed parents.
- `Domain` bounds live nodes and plans, `Budget` bounds live native payload including temporaries and
  scratch. Both belong to the host; the prototype's process-wide counters are gone.
- A program is bounded at compile time: 1,024 nodes (mechanical: plans number values in `u16`) and, by default, 67,108,864 scalar operations per run. A failed `runInto` leaves its output as it was: only the last stage writes it, and that stage's fallible work comes first.
- `Limits` holds what a host allows: elements per tensor (default 1,048,576) and work per run. `ComputeLimits::graph`, `Domain::with_limits` and `Budget::with_elements` raise them. The only limits a host can't raise are mechanical: `isize::MAX / 64` elements, and work that can be counted in a `usize`.
- Scratch is allocated for the active stage, charged for its full requested payload, and freed before its budget guard drops. No thread-local scratch survives a call.

Native payload looks small to Luau's collector, so unreachable results can hold the whole budget
without triggering a cycle. The host's calls collect once and retry when the budget refuses them, and
a test runs 320 MiB of unreferenced results through a 64 MiB budget. The host uses public `Call::collect_garbage`; it declares no private GC FFI.

**A host must sandbox its globals** (`Runtime::sandbox_globals`). Luau's builtin fast paths and JIT
guards require a safe environment. The first version of this host did not, which made
`buffer.readf32` loops about 15x slower and made native code look useless.

## Prepared weights

`tensor:prepareRight()` retains a budget-charged immutable right-hand packing; `prepareLeft()` prepares column panels for single-column products. `program:runTensorsInto(out, tensors...)` uses those owners while borrowing the output for one call. Packings remain charged until their last owner drops. Derived views do not inherit packings. Preparation is explicit: ordinary execution remains available without persistent duplicate storage.

## Operations and limits

Scalar/rank-two f32 graphs support add, multiply, ReLU, matmul, transpose, ordered row sums, accurate softmax, RMS normalization and layer normalization. See [row contracts](ROW-KERNELS.md) for statistics and approximation semantics; not every row operation is strict-f32 arithmetic. GPU/generated-Luau lowering rejects unsupported stages.

There is no executor-owned thread pool. Concurrent immutable programs are supported, and a host can lend workers through the `Executor` trait (`Program::run_into_parallel`, `ComputeExtension::with_executor`): matrix products split by rows and elementwise stages by chunks, with the same bits, each share's scratch charged and held before any writes. Row stages stay on the calling thread. A standalone [int8 research primitive](INT8.md) investigates exact quantized matrix-vector arithmetic; it is not a graph or JSL API. Ranks above two are not implemented.

Native x86-64 and AArch64 correctness and benchmark jobs passed at `1f0e952`; [the evidence](native-1f0e952.md) records exact runs and raw artifacts. AArch64 uses compiler-vectorized baseline kernels and its architectural FMA support in fast mode; x86 selects AVX2/FMA at runtime. Hosted-runner timing is not controlled hardware benchmarking. AVX-512 remains untested.
