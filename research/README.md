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

The proposed high-level representation and its boundary from kernel IR are specified in
[`typed-graph-proposal.md`](typed-graph-proposal.md). It is not a production API.

The isolated CPU bridge limits each input and output to 1,048,576 f32 elements, matrix products
to 16,777,216 scalar multiply operations, and admissions to 16 distinct kernel specializations
per process. A process-wide gate serializes the synchronous kernel launches, limiting the bridge
to one active compilation or execution at a time. Existing admitted variants remain usable when
that budget is exhausted. Matrix output sizing is checked even when the inner dimension is zero, and zero-filled output uses a
fallible reservation. Invalid shapes and budget exhaustion return script errors; tests cover the
allocation boundary, overflow, exhaustion, failure recovery and a second L3i runtime. These are
prototype limits, not a complete process memory quota: CubeCL's allocator, worker threads and LLVM
compiler remain outside it, and compiler/backend panics and hard compile-time deadlines are not
contained. A production executor needs host-controlled limits and failure isolation.

The prototype remains outside the existing library CI. Run `bash research/verify.sh` from the
repository root to execute its locked checks and the repository gates with the active, matched
Rust/Clang toolchain. No separate CI workflow is added.

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
package. L3i now declares Rust 1.99.1 as its MSRV at the user's direction, matching the updated stable
toolchain used for these experiments. The earlier Rust 1.92 observations below are historical.

After the toolchain and MSRV update, the pinned 0.11.3 experiment was rebuilt with rustc
1.99.1 (LLVM 23.1.1) and clang 23.1.2, linking an L3i runtime and executing its Luau assertion
in the same process. Its predefined kernel verified 65,536 f32 outputs. One debug run measured
initialization 1,455 us, upload/allocation 628 us, first dispatch plus readback 48,155 us, then
422, 459 and 437 us. This is not a JSL-to-0.11 IR adapter or an optimized comparison.

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
new owned storage. ReLU preserves NaN and maps nonpositive values to positive zero. The
`run_strided` research binding accepts byte offsets and nonnegative byte strides; a zero stride
broadcasts one element. It validates the complete view, copies into owned contiguous buffers, and
never writes the inputs. It caps the view at 1,048,576 elements. It has no in-place output,
multidimensional broadcasting, async operation, or backend abstraction. An independent
predefined matrix kernel accepts checked contiguous 2-D shapes. Its sum is f32 in loop order;
the expression frontend does not compile matrix multiplication.

The script verifies two expressions, empty inputs, invalid opcodes and lengths, aliasing the two
inputs, repetition, NaNs/infinities/zero, a 2x3 by 3x2 product against a JSL scalar reference,
zero inner dimensions, and rejected shape overflow. The test runs synchronously and readback
finishes before returning. It also checks strided reads, zero-stride broadcast, invalid view bounds and unchanged aliased
inputs. It does not establish general failure recovery, safe use of arbitrary CubeCL kernels,
multidimensional broadcasting, or resource teardown under
failure. `cubecl-cpu` is feature-enabled for compiler dumps; to inspect CubeCL IR and MLIR,
create `target/cubecl-ir` and run with `CUBECL_DEBUG_MLIR=target/cubecl-ir`. Files for the same
kernel name are overwritten by later compilations. `CUBECL_DEBUG_LOG=stdout` also enables per-dispatch profiling and changes timing; its
reported durations must not be treated as compilation times. Distinct operation sequences are
included in the macro-generated kernel cache identity and produce different CubeCL IR. The adapter relies on the
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

The strided-copy revision added separate boundary and host-copy probes. Two fresh-process release
runs, ten warm calls per size, measured microseconds:

| Elements | CubeCL first | CubeCL warm | `dream.data` | Two host input copies | Strided pack and CubeCL |
| ---: | ---: | ---: | ---: | ---: | ---: |
| 1,024 | 36,069.325 | 495.325 | 2.929 | 0.345 | 472.341 |
| 65,536 | 2,149.624 | 1,072.756 | 142.692 | 26.597 | 1,126.451 |
| 1,024 | 34,663.323 | 559.782 | 2.903 | 0.334 | 539.558 |
| 65,536 | 2,148.037 | 1,195.549 | 145.327 | 31.584 | 1,043.875 |

The host-copy probe includes a JSL/native call and allocates and drops two vectors; it does not
isolate CubeCL upload or readback. The strided probe used stride 4 (contiguous) and includes
bounds checks, packing, transfers, execution and readback. Differences between close figures are
noise, not evidence that packing accelerates execution. A 10,000-iteration no-op host call plus
JSL loop measured 0.0205 and 0.0216 microseconds per iteration; it is a lower-bound proxy, not a
standalone crossing measurement. The first process peaked at 98,768 KiB RSS. The bridge allocates
two host input vectors, three logical CubeCL buffers, readback storage and a returned Luau buffer;
CubeCL's pool and internal allocation count were not measured. It has no in-place operation:
its fresh result and input copies preserve aliased source storage. Repeated L3i runtime creation
is tested, but failure of the CubeCL worker or JIT can still panic and is not safely translated
into a script error by this prototype.

## Fused and persistent-buffer measurements

Two fresh-process release runs compared `relu(a + b + 2)` on finite integer-valued f32 inputs.
The JSL benchmark verified every result against three preallocated `dream.data` passes. The CubeCL
bridge includes input copies, fresh output, dispatch, synchronization and readback; `dream.data`
uses existing output and constant buffers. Each warm number averages ten calls, in microseconds:

| Elements | CubeCL first | CubeCL warm | `dream.data` three passes |
| ---: | ---: | ---: | ---: |
| 1,024 | 26,331.946 | 491.047 | 7.760 |
| 65,536 | 1,281.292 | 917.584 | 527.737 |
| 1,024 | 26,162.902 | 527.682 | 7.660 |
| 65,536 | 1,889.490 | 1,103.540 | 452.917 |

The 65,536-element call reuses the specialization compiled by the 1,024-element call. First-call
measurements include compilation, initialization and transfers; they are not isolated JIT times.

A separate **predefined Rust kernel**, not the JSL expression compiler, retains CubeCL input and
output handles across calls and synchronizes each call without reading back until the end. It
computes the same operation on a separately generated finite input set and checks the whole result
against a preallocated Rust loop. Twenty warm calls are averaged; two fresh-process runs:

| Elements | Allocation/upload | First dispatch + sync | Warm dispatch + sync | Final readback | Rust preallocated |
| ---: | ---: | ---: | ---: | ---: | ---: |
| 1,024 | 13 | 68,038 | 503 | 13 | <1 |
| 65,536 | 511 | 954 | 496 | 15 | 21 |
| 1,024 | 18 | 39,917 | 921 | 16 | <1 |
| 65,536 | 535 | 1,098 | 1,457 | 21 | 25 |

All entries are microseconds, rounded down by `Duration::as_micros`; `<1` is below that timer's
printed resolution. The first call includes compilation; the 65,536-element run reuses the
compiled kernel. One process peaked at 90,012 KiB RSS. Runtime initialization is outside the
setup timing in this experiment. Runs have substantial variance and differ in storage contract,
input values, optimization opportunities and measurement boundaries; they establish costs on this
machine, not a backend ranking for all fused workloads. The CPU path does not currently justify
replacing `dream.data`, even when persistent backend buffers avoid repeated upload and readback.

## Still required

Implement and validate a backend-independent tensor graph and persistent ownership contract;
the experimental operation string is not a proposed language. Prove strict numerical semantics,
shape inference and multidimensional broadcasting across supported backends. Contain backend
failures, enforce hard compilation time and memory limits, and test teardown under failure.
Measure persistent JSL-owned tensor storage rather than inferring its costs from the separate Rust
benchmark. Prove how a Jess-owned shader or CubeCL adapter orders work with commits, generation
lifetimes and shared resources. No inference engine, CubeCL GPU integration, public tensor API,
or general-purpose direct IR adapter is claimed here.

## Upstream references

* [CubeCL releases and published versions](https://docs.rs/crate/cubecl/latest); the pinned
  experiments use 0.10.0 and 0.11.3. CubeCL documents its alpha API as subject to breaking changes.
* [CubeCL 0.10 kernel expansion and compilation model](https://docs.rs/crate/cubecl/0.10.0).
* [CubeCL wgpu runtime and existing-device initialization](https://docs.rs/cubecl-wgpu/0.11.3/cubecl_wgpu/).
* [CubeCL CPU runtime](https://docs.rs/crate/cubecl-cpu/latest), [CubeK](https://github.com/tracel-ai/cubek),
  and [Burn](https://github.com/tracel-ai/burn); neither CubeK nor Burn is a runtime dependency here.
