# Typed tensor graph: research interface proposal

This is a proposed internal contract, not an implemented or approved public API. The isolated
CPU experiment is evidence about one possible backend; its operation string is not this graph
and is not intended as a permanent language. No transformer or model format is assumed.

## Layers

```text
JSL application / policy
        | calls bulk graph builder and submit operations
        v
backend-independent typed tensor graph (shape, dtype, semantics, dataflow)
        | validate, infer shapes, plan allocation and legal fusion
        +--> CPU reference / native bulk executor
        +--> optional CubeCL kernel generation and runtime adapter
        +--> Jess-owned shader IR -> WGSL -> existing GPU submission
```

The tensor graph describes whole operations, not GPU invocations, thread indices, workgroups,
barriers, address spaces, kernel launch dimensions, or Luau native-code instructions. The graph
compiler may partition operations into kernels and choose a low-level representation. CubeCL IR,
Jess shader IR and Luau native-code IR remain separate, backend-specific targets. JSL authors the
model architecture by building or composing tensor operations and can retain ordinary JSL control
flow; arbitrary conversion of Luau function bodies is unnecessary.

## Smallest candidate representation

A graph is an immutable, topologically ordered sequence of nodes with validated integer node IDs.
Each value has a `TensorType { dtype, shape }`. Initially dtype is `f32`; unsupported dtypes return
an explicit error rather than being reinterpreted. A shape is a bounded list of nonnegative
concrete dimensions, with checked rank and checked element/byte products. Rank zero denotes a
scalar and any zero dimension denotes an empty tensor. Symbolic dimensions, dynamic rank and other
dtypes are deferred. Node records, written in Rust-like notation rather than API syntax:

```text
Input { slot, type }
Constant { type, owned_bytes }
Add { lhs, rhs }
Mul { lhs, rhs }
Relu { input }
MatMul2d { lhs, rhs }
Output { value }
```

Inputs are unique slots; constants own immutable validated bytes. Add and Mul accept NumPy-style
right-aligned broadcasting: dimensions must match or one must equal one, and 1 broadcasts to 0
as well as to larger dimensions. All other incompatible shapes fail at validation before any
execution. MatMul2d requires rank-two `[m, k]` and `[k, n]`, produces `[m, n]`, and the zero-k
result contains positive zero. There are no implicit casts, mutations, random operations, or
cross-device transfers. A graph result is an owned value. Mutable operations can be designed later
with explicit alias and version rules rather than inferring exclusivity from a host language value.

The complete graph identity must include node order, opcodes, all attributes, tensor types,
constant bit patterns and numerical mode; compiled-kernel cache keys must additionally include
backend, target capabilities and relevant layout/specialization properties. Implementations must
compare canonical keys, not rely only on a hash. Limits on graph size, total constant bytes,
shape products, execution allocation, compilation concurrency, number of specializations and
cache lifetime are host policy inputs. Limits must apply across runtime recreation and concurrent
requests. The experiment's process-wide specialization cap is only a first count bound; it does
not limit compiler time, pool memory or execution time and cannot contain native panics.

## Numerical contract

The initial conservative mode is f32 operations with round-to-nearest ties-to-even after each
specified arithmetic operation. Add and Mul have ordered operands; Mul followed by Add is not
silently contracted into an FMA. MatMul2d accumulates f32 products in increasing k order, with a
rounding point after each multiplication and addition, beginning with positive zero. ReLU is
specified as `if x <= +0 then +0 else x`: NaN is preserved and either signed zero becomes +0.
Backends must demonstrate that they honor this mode, including NaNs, infinities, signed zero,
subnormals, and rounding; otherwise they must reject the graph or explicitly offer a separately
named relaxed mode. NaN payload preservation is not promised; the result must remain NaN. A
backend's ordinary WGSL or CubeCL math mode must not be assumed to meet this strict contract.
Optimization and fusion are permitted only where they preserve the selected mode.

## Storage and execution

A tensor handle names host-owned or backend-owned storage plus validated dtype, byte offset,
shape and nonnegative byte strides. Bounds are checked using overflow-safe arithmetic; zero
strides are read-only broadcasting. Empty views do not dereference storage. A host buffer may
alias other handles; no Rust slice or Luau pointer may be retained across callbacks, asynchronous
work or runtime teardown. For a first implementation, import is an explicit copy into owned
storage, and output is fresh storage; in-place writes and concurrent mutation are unsupported.
More efficient shared storage needs explicit borrowing, hazards, lifetime and release contracts.

The host supplies an executor/service and the authority to use it. Compile is explicit and may
fail without publishing a partial executable. Submit returns an owned pending result or error;
completion and readback are explicit, and cancellation/teardown must define resource release.
CPU execution can implement a completed result synchronously. Jess remains responsible for GPU
capabilities, device choice, scheduling, generation ownership and ordering with its deferred
commits. A backend must not acquire a GPU itself or bypass those rules.

## Jess shader participation

Jess's existing validated `Program` can express compute invocations, storage accesses and
control flow, so a Jess-side lowering can generate a shader for a supported graph partition and
use its existing WGSL, pipeline, bind group and dispatch path. This does not require L3i to depend
on Jess. Equivalence must be proved for type conversion, bounds, data layout, floating-point mode,
workgroup and synchronization semantics before accepting a graph. Operations Jess cannot express
should fail or fall back, not silently change meaning. Importing CubeCL's wgpu setup requires Jess
internals, matching versions and feature negotiation; shared device handles do not prove shared
buffers or ordering with Jess commits. See `README.md` for the current source audit.

## Small next experiments

1. Implement a standalone CPU reference for this graph with shape and numerical tests, then test
   end-to-end JSL graph construction without a production extension.
2. Add persistent owned CPU buffers and compare fused Add/Mul/ReLU against scalar Rust and
   `dream.data` compositions, including cold compilation, warm execution, allocations and copies.
3. Prove one graph partition can lower to existing Jess shader IR with equivalent results and
   its normal generation and commit lifecycle, including software GPU tests.
4. Evaluate a CubeCL adapter only against that graph contract, with bounded cache, failure
   isolation and host-controlled runtime. Evaluate 0.11 against the updated Rust 1.99.1 toolchain and the cross-language LLVM contract.
5. Extend matrix and tensor workloads only after those contracts and measurements are established;
   model loading, transformer architecture and token policy remain later JSL/application work.
