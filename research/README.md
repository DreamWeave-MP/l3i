# Accelerated compute: preliminary experiments

This is an incomplete research record, not a supported L3i API or a proposed dependency.
The standalone crates have their own Cargo workspaces and do not affect L3i's normal build.
The baseline kernels are predefined Rust. A separate, deliberately limited dynamic experiment builds CubeCL IR from a JSL-authored operation sequence.

## Existing boundaries

* L3i `src/data/mod.rs` supplies bulk typed spans, strided fields, selections and reductions.
  Its overlapping span operations have sequential semantics; reductions accumulate in f64.
  `src/data/lowering.rs` specializes recognized operations into Luau native-code IR.
* `src/convert/buffer.rs` deliberately copies on safe access because buffers can alias.
  An asynchronous backend must not retain these borrowed views: own storage or arrange an
  independently validated lifetime, and specify copy and synchronization semantics.
* L3i `src/extension/` composes extensions and `src/runtime/` owns VM lifetime. Its Luau
  native-code IR and CubeCL kernel IR have different execution models.
* Jess `src/lua/shader.rs` constructs `src/shader/`'s validated program, lowered to WGSL.
  `src/gpu/` and `src/lua/gpu/` own and expose dispatch, buffers and device policy. Jess's
  `src/runtime/capability.rs`, `resources.rs`, and `service.rs` govern authority and lifecycle.
  Its deferred changes are committed in order before operations that wait on the GPU.
* Existing APIs and their numeric/aliasing behavior must remain intact. A future numerical
  representation can be additive. Jess can adapt an appropriate subset of its shader IR, but
  L3i must not acquire Jess devices or depend on Jess. Any CubeCL GPU adapter must establish
  ordering with Jess commits and resource generations, beyond merely sharing a wgpu device.

## Capability and ownership audit

| Area | Existing contract | Research gap / additive direction |
| --- | --- | --- |
| `src/data/mod.rs`, `tests/data.rs` | Eight scalar kinds, contiguous or record-strided spans, reductions and bulk operations, caller-provided output and specified overlap order | No general multidimensional shape, broadcasting or fused expression graph. Keep existing semantics and cost unchanged. |
| `src/data/lowering.rs`, `src/native_code/` | Luau JIT hooks lower recognized operations into Luau native IR with binder fallback | This is a VM-specific IR, not a SIMT kernel IR. Reuse for suitable CPU loops; benchmark before translating or extending it. |
| `src/convert/buffer.rs`, `src/packed.rs` | Scoped, bounds-checked byte copies and separate packed scalars | Tensor ownership needs independent storage, validated views, explicit transfer and synchronization. Do not retain borrowed buffer pointers. |
| `src/extension/`, `src/runtime/` | Additive module/userdata descriptors, plan composition and VM lifecycle | A future compute extension can be composed through a plan; the experimental globals here are not a public extension. |
| Jess `src/shader/`, `src/lua/shader.rs` | Typed symbolic expressions, places and statements; validation and deterministic WGSL lowering; JSL compute construction | Reuse this frontend for shader-compatible work. An adapter to a non-WGSL backend needs a defined subset and tests for control flow, storage, atomics and numeric semantics. |
| Jess `src/gpu/`, `src/lua/gpu/` | Jess-owned devices, buffers, pipelines, deferred writes, commits and dispatch | Do not introduce another script-facing GPU namespace or acquire devices in L3i. CubeCL and Jess both provide compiler, scheduler and buffer-management machinery, so GPU adoption risks duplication. |
| Jess `src/runtime/capability.rs`, `resources.rs`, `service.rs`, `src/lua/plan.rs` | Scoped authority, generation-owned resources, services and L3i composition | Any future adapter belongs under Jess's existing authority and generation lifecycle. It must order CubeCL work with Jess commits, in-flight uses and teardown. |

CubeCL 0.11.3's `WgpuSetup` requires the existing instance, adapter, device, queue and matching
backend; `try_init_device` validates some capabilities, registers a new identity each time, and
requires callers to register once and clone the returned identity. Jess's `Gpu` holds those handles
privately and exposes no wgpu types in its public GPU API. A future integration needs a narrow
internal Jess adapter, compatible feature negotiation, a single registration, and explicit ordering
with its frame lock and deferred commits. We have not established a supported way to import Jess
buffers into CubeCL or safely share allocator ownership; device compatibility is not buffer
interoperability. The version ranges align on wgpu 30 but an actual combined build and execution
have not been tested.

CubeCL 0.10's CPU feature brings a separately bundled LLVM 20 and runtime worker pool; 0.11.3 uses
LLVM 23 and its CPU dependency graph declares Rust 1.95. The crates report MIT OR Apache-2.0;
transitive dependencies and bundled LLVM still require a complete distribution license review.
The isolated experiments use separate Cargo workspaces and do not add a dependency to the L3i
package. The successful 0.10 demonstration gives no reason by itself to raise L3i's Rust 1.92
minimum; the user has authorized considering an upgrade if subsequent evidence supports it.

## Evidence so far

* `jess run --capability gpu.device research/fused-shader.jsl` on jess 0.3.0 executed a
  JSL-authored multiply/add/max expression, constructing Jess shader IR and dispatching WGSL.
  It checked 256 independent scalar results on `llvmpipe (LLVM 22.1.8, 256 bits)`, Vulkan,
  device kind CPU. This path does not involve CubeCL. It exercises the existing API, not a
  new tensor facility, and does not expose generated WGSL to the script.
* `cargo +1.92 check --manifest-path research/cubecl-cpu/Cargo.toml` rejects CubeCL 0.11.3:
  its `cubecl-monitoring` and `cubecl-zspace` 0.11.3 dependencies require Rust 1.95, as do
  selected `hi_sparse_bitset` and `sysinfo` dependencies. CubeCL 0.11.3's wgpu dependency
  is `^30.0.0`, matching Jess's 30.0.1 version range; its existing-setup import does not by
  itself demonstrate buffer ownership or synchronization interoperability.
* `cargo +1.92 check --manifest-path research/cubecl-cpu-010/Cargo.toml` passed for an empty
  host program with the pinned CPU backend. The kernel also ran on Rust 1.92 before adding L3i. This host has rustc 1.92 LLVM 21 and clang 23,
  so that check is not evidence of a verified matching-LLVM L3i build.
* `cargo run --manifest-path research/cubecl-cpu-010/Cargo.toml --bin l3i-cubecl-cpu-010-experiment` using rustc 1.99 / clang 23
  ran 65,536 f32 values and compared every result against a scalar reference. One debug run
  on an Intel i7-10870H measured initialization 3668 us, allocation/upload 577 us, first
  dispatch plus synchronous readback 35665 us, then 870, 762 and 818 us. These are single
  observations, include readback, and are not a comparison against L3i or optimized Rust.
* CubeCL 0.11.3 on rustc 1.99 / clang 23 ran the same numerical workload. One debug run
  measured initialization 1462 us, allocation/upload 599 us, first dispatch and readback
  48512 us, then 397, 373 and 357 us. Different versions and individual runs cannot support
  a performance ranking. Both prototypes use the CPU backend and no GPU. After the measurements above, the 0.10 experiment was extended to link L3i, create its runtime and execute a Luau assertion in the same process; the CubeCL kernel still runs independently of the script.

The dynamic experiment is `cargo run --manifest-path research/cubecl-cpu-010/Cargo.toml --bin dynamic`.
`dynamic.jsl` constructs operation sequences; `run_expression` validates them, copies the two
Luau buffers into owned host vectors and CubeCL buffers, and specializes an f32 CubeCL kernel.
The `#[comptime] Vec<u32>` is unrolled at compilation, so its operations become CubeCL IR;
the generated kernel cache identity includes that compile-time argument. This is a constrained
left-to-right DSL (`*` and `+` use the second input, `2` adds two, `r` applies ReLU), not a
parser for arbitrary Luau functions. It runs no Lua callback per element. Expressions are
limited to 32 operations; only equal-length contiguous f32 inputs are accepted; the result is
new owned storage. ReLU preserves NaN and maps nonpositive values to positive zero. There is no
in-place update, view, broadcasting, async operation, or backend abstraction. An independent
predefined matrix kernel accepts checked contiguous 2-D shapes. Its sum is f32 in loop order;
the expression frontend does not compile matrix multiplication.

The script verifies two expressions, empty inputs, invalid opcodes and lengths, aliasing the two
inputs, repetition, NaNs/infinities/zero, a 2x3 by 3x2 product against a JSL scalar reference,
zero inner dimensions, and rejected shape overflow. The test runs synchronously and readback
finishes before returning. It does not establish general failure recovery, safe use of arbitrary
CubeCL kernels, noncontiguous views, multidimensional broadcasting, or resource teardown under
failure. `cubecl-cpu` is feature-enabled for compiler dumps; to inspect CubeCL IR and MLIR,
create `target/cubecl-ir` and run with `CUBECL_DEBUG_MLIR=target/cubecl-ir`. Files for the same
kernel name are overwritten by later compilations. `CUBECL_DEBUG_LOG=stdout` reports individual
compilations, showing that distinct expressions specialize separately. The adapter relies on the
alpha macro frontend and its specialization cache; it is research code, not a stable API.

A debug-mode experiment (`cargo run --manifest-path research/cubecl-cpu-010/Cargo.toml --bin
dynamic -- --bench`) on the i7-10870H compared f32 addition. Inputs were allocated before timing;
all measurements include the JSL/native boundary. CubeCL also copies both inputs, allocates device
and return storage, dispatches and synchronously reads back; native Rust copies inputs and allocates
its output; `dream.data.add` writes preallocated output. Ten warm calls were averaged. A single run:

| Elements | First call, us | CubeCL warm, us | Rust copy, us | dream.data preallocated, us |
| ---: | ---: | ---: | ---: | ---: |
| 1,024 | 34,605.838 | 604.997 | 99.690 | 18.699 |
| 65,536 | 2,665.174 | 1,374.948 | 6,054.354 | 1,016.605 |

Only the first 1,024-element call includes compilation; the larger input reuses that kernel. These
are debug builds, single observations, and different storage contracts: they are not an optimized
performance comparison. CPU kernel allocations, transfer time, peak RSS, and boundary time are not
independently isolated. The release measurements below and further workloads inform the backend decision.

Two additional fresh-process release runs, after other builds finished, used the same input and ten
warm invocations per size. Both used rustc 1.99.0 (LLVM 23.1.1), clang 23.1.2, x86-64 Linux,
CubeCL 0.10.0, and Cargo's release profile. Values are microseconds; each row is one process.

| Elements | First call | CubeCL warm | Rust copy | dream.data preallocated |
| ---: | ---: | ---: | ---: | ---: |
| 1,024 | 46,880.572 | 485.479 | 5.499 | 2.725 |
| 65,536 | 2,330.702 | 1,076.695 | 181.264 | 147.243 |
| 1,024 | 32,258.926 | 526.279 | 5.536 | 3.228 |
| 65,536 | 1,869.061 | 1,148.488 | 174.520 | 134.183 |

First-call compilation occurs only in the first size of each process; the second size reuses the
compiled kernel. `/usr/bin/time` reported 97,904 KiB maximum resident memory for the first process,
including L3i, CubeCL, JIT and benchmark setup. These figures support choosing existing `dream.data`
for this simple CPU addition workload under these storage contracts; they do not characterize fused
workloads or GPUs. Input initialization is excluded and output allocation differs as described above.
No allocation count or standalone transfer and boundary timing was measured.

## Still required

Integrate a compatible CPU backend with L3i without weakening its toolchain contract; expand the
JSL-authored expression beyond the prototype and test
shapes, strides, aliasing, floating-point edge cases and teardown; expand the matrix workload; measure optimized cold/warm execution, copies, allocations, RSS and boundary
costs against `dream.data` and native Rust; investigate Jess resource interoperability; and run
all repository gates. No inference engine, CubeCL GPU integration, public tensor API, or general-purpose direct
IR adapter is claimed here.
