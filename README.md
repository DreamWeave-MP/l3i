# l3i

**l3i** is a Rust binder for [Luau](https://luau.org) 0.740, ported from the OpenMW
Luau binder (`components/luau` and `components/lua/bindfunction.hpp`). It owns its Luau build,
declares the C API by hand, and puts a safe, scoped layer over it: frame-scoped stack views,
registry-pinned owned values, a typed function binder that reads arguments straight from stack
slots, tagged and untagged userdata, Luau's direct userdata access, sandboxed script instances,
watchdog and profiler plumbing, (with the `jit` feature) Luau's native code generator with
lowering hooks written in Rust, and (with the `soft-render` feature) a CPU rasterizer as a
Luau extension.

No `mlua`. Tags, atoms, type names and debug-name roots are host data: the crate ships the
mechanism, never a catalogue.

- [Quick start](#quick-start)
- [The three tiers](#the-three-tiers)
- [Userdata](#userdata)
- [Modules and sandboxes](#modules-and-sandboxes)
- [Runtime options](#runtime-options)
- [Direct access and atoms](#direct-access-and-atoms)
- [Native code generation](#native-code-generation)
- [Safety model](#safety-model)
- [Building](#building)
- [Quality](#quality)
- [License](#license)

## Quick start

```toml
[dependencies]
l3i = { version = "0.1", features = ["jit"] } # jit, analysis, and soft-render are optional
```

```rust
use l3i::Runtime;
use l3i::userdata::{Owned, Userdata, tagged};

struct Vec3 { x: f32, y: f32, z: f32 }

unsafe impl Userdata for Vec3 {
    const NAME: &'static str = "dreamweave.Vec3";
}

fn main() -> l3i::Result<()> {
    let runtime = Runtime::new()?;
    // The host picks the tag, per runtime, at registration.
    tagged::register::<Vec3>(&runtime, 10, |ty| {
        ty.property("x", |v: &Vec3| v.x)?;
        ty.method("length", |v: &Vec3| (v.x * v.x + v.y * v.y + v.z * v.z).sqrt())
    })?;
    let make = runtime.bind_function("dreamweave.vec3", |x: f32, y: f32, z: f32| Owned(Vec3 { x, y, z }))?;
    runtime.set_global("vec3", &make)?;
    runtime.exec("local v = vec3(3, 4, 0) assert(v.x == 3 and v:length() == 5)")?;
    Ok(())
}
```

`examples/openmw_shapes.rs` runs every binding shape the OpenMW engine uses on toy types.

## The three tiers

| Tier | Type | Cost | Use |
| --- | --- | --- | --- |
| Borrowed | `ValueView`, `TableView`, `FunctionView` | none: a stack index bound to a `Frame` | hot paths, arguments, results |
| Owned | `Value`, `Table`, `Function` | one registry pin (`lua_ref`) | values that outlive a frame |
| Typed binder | `Runtime::bind_function`, `MetatableBuilder` | conversion from stack slots, no pins | exposing Rust to scripts |

`Stack` is the exclusive handle to a thread's stack; `Frame` owns temporaries and restores the
height when dropped. Frames nest strictly (siblings panic), `with_frame` closures are
higher-ranked so views cannot escape, and the runtime leases exactly one root stack at a time.

The typed binder accepts converted scalars (`i32`, `f64`, `Integer`, `bool`, `String`,
`&str`, `Vector3`, `BufferView`), borrowed views, `&T` userdata receivers, injected `&Call`,
`Option<T>` (with OpenMW's middle-optional rules), `VarArgs<T>`, `ArgView`, `Overload<(..)>`, and
returns unit, scalars, `Option`, tuples, `Variadic`, `ResultOrError`, `NilThen`, `Owned<T>`,
`Borrowed<T>`, `StackResults`, and `Result<T>` (an `Err` raises a Lua error with the exact
`luaL_typeerror` wording).

## Userdata

A type implements `Userdata` for identity and script name only. How it is exposed is decided
per runtime:

- `tagged::register::<T>(&runtime, tag, configure)`: inline payload, one Luau tag, checks are one
  tag read and one `TypeId` compare against the runtime's tag plan. Scarce (tags `1..254`);
  for hot types.
- `untagged::register::<T>(&runtime, configure)`: exact metatable identity, `Storage<T>` owned or
  a `StableRef<T>` borrow of an engine object, per-instance destructors. No tag consumed.

The same Rust type may be tag 8 in one runtime, 17 in another, and untagged in a third.
`MetatableBuilder` gives methods, properties, read/write properties, unbound methods,
metamethods, native method tables, and array/keyed/cursor `__iter` factories, with OpenMW's
conflict rules and the plain-table → generated `__index`/`__namecall` phase machine. The
generated dispatchers run bound members directly on their own stack instead of `lua_call`ing
the member closure, so a generated method call or property read costs one Luau call frame,
not two. Untagged receiver checks compare against a per-VM cached metatable identity rather
than reading the registry each time.

## Modules and sandboxes

`LuauModule` + `ModuleBuilder` build one frozen package table (`dreamweave.assets` style
debug names, `__tostring`, userdata registration). `readonly` provides frozen tables, strict
tables, and read-only views that iterate their backing table without exposing it.

`Runtime::sandbox` installs OpenMW's prelude (compatibility `pairs`/`ipairs` honouring
`__pairs`/`__ipairs`, optional LuaJIT-style `string.format` `%s`, optional neutered
`math.randomseed`) and builds the frozen base environment. `Sandbox::new_instance` makes a
per-script environment (`_G`, named `print`, `loaded` packages, `require`), and
`Sandbox::load_template` compiles a chunk once on an isolated loader thread so
`Sandbox::instantiate` can clone it per script with `lua_clonefunction` + `lua_setfenv`.
`load_template_in`/`instantiate_in` do the same on a bound function's own scope, for `require`
loaders written in Rust.

## Runtime options

`Runtime::builder()` configures debug-name roots, pointer-encoding seed, deferred standard
libraries, the atom catalogue, and:

- **Watchdog**: `execution_time_limit` (per outermost script call) and `memory_limit` (a heap
  ceiling polled every 64th safepoint, not an allocator cap), raising Lua errors like OpenMW.
- **Memory categories**: `MemoryCategory(u8)`, `total_bytes_in`, per-call switching.
- **Call scopes**: `Runtime::call_scope(context, kind)` (script call, initialization, host
  interface) with nested self-time accounting.
- **Collector**: `gc(GcControl::...)` reads the heap size and sets Luau's goal, step
  multiplier, and step size (the defaults, a 200 percent goal, multiplier 200, and 1 KB
  steps, are kept), or drives the collector by hand with `Step` and `Collect`; Luau has no
  `collectgarbage`, so a host that wants collection at frame boundaries steps it there.
- **Profiler**: `profiler(true)` records call time and allocation activity per context;
  `set_sampled_context` samples one context every 32nd safepoint into `source:line` and
  per-function counters; `gc_step_timed`, `caller_location`, and the pure statistics helpers in
  `runtime::profiler` (30-frame rolling averages, decaying peaks) cover the generic half of
  OpenMW's profiler. Report text and UI stay in the host.

## Direct access and atoms

Luau can call a native callback straight from `GETTABLEKS`/`SETTABLEKS`/`NAMECALL` for a tagged
userdata when the key has an *atom*. Each runtime carries its own `AtomCatalogue`
(`RuntimeBuilder::atom_catalogue`). `direct::plan::DirectPlan` is the normal path: built per
runtime from the tags and atoms that VM actually assigned, it maps `(tag, kind, atom)` to the
host's slot ids and validates Luau's per-instruction cache in O(1) before trusting it, so one
handler serves a type that is tag 8 in one VM and tag 17 in another. A runtime publishes one
plan: `finish` refuses a second, because the slot ids are the host's dispatch protocol and
Luau's inline caches hold them. `direct::Registry` is the
static alternative for hosts whose identities really are compile-time constants. `DirectAccess`
handlers run on both the direct path and the ordinary metamethod path with the original
metamethod retained as the fallback; `direct::field` registers per-field getters that write
straight into the destination register. `Runtime::install_vector_buffer_writer` adds
`vector:writef32x3(buffer, offset)`.

## Extensions and runtime plans

A native crate exposes its Luau surface as an [`extension::Extension`]: `describe` declares
identity (`dream.archive`), dependencies, modules (`@dream/archive`, frozen by default) with
each member's kind, signature, and doc (`function("open", open).signature("(path: string) ->
dream_archive_Archive")`, `constant`, or `installed("client")` for a value only a live VM can
provide), userdata types under stable string keys with a `TagPolicy` and each member with its
callable (`method("read", |a: &Archive, path: &str| ..)`), services, capabilities, packed
kinds, and memory categories, all without touching a VM. Types are never accidental: every
member carries a signature or is marked `untyped()`, and a plan with neither fails to
finalize. The vocabulary follows the runtime, where Luau's checker keeps `integer` and
`number` apart: a packed value, a `Bits64`, or an `Integer` result is `integer`; a count, a
size, or an `f64` is `number`; a class is its generated name (`dream_net_Client`). Callables
are `Clone` because one plan binds them in every runtime it creates. `install` is optional and
runs per runtime for what needs the live VM or the resolved policy: it fills the module
members declared `installed` (a policy-gated function, a userdata instance) and cannot add a
name the plan does not know, so the plan's type definitions and compiler metadata describe the
whole API before any runtime exists; services and runtime-owned state live there too
(`InstallContext::insert_state` is per extension, `Runtime::host_state` the host's own). The planned dispatch costs 429 instructions per
method call against 368 for the VM's own typed direct handler (`benches/instructions.rs`):
Luau's inline cache and the member entry are one indexed load in a fused slot table, the plan
is read without a refcount or a borrow flag, a type's own dispatchers vouch for the receiver
so the bound member skips its type check, and one panic guard covers the whole call.
`RuntimePlan::builder().policy(..).service(..)
.extension(..).finalize()` orders extensions by their dependency graph (deterministically),
merges owners with augmenters into one type per key, assigns tags (pinned, then `Required`,
then `Preferred` while tags last), allocates Luau's 32 compiler userdata type slots the same
way (`CompilerTypePolicy`: a type whose methods lower natively declares `Required` and the plan
fails rather than leave that path interpreted), assigns atoms densely, lays out direct slots (a direct field
whose name is a method or property elsewhere in the plan is served through a slot instead of
Luau's field table, since the atom rewrite would bypass that table), resolves memory
categories, and checks services and capabilities. `Runtime::from_plan(&plan)` then builds a VM,
registers metatables with the merged members, wires the planned direct members to one set of
generic VM callbacks, opens the declared modules, runs every extension's `install` in order,
freezes modules, registers them for `require`, derives compiler-known library metadata for
compat globals, and publishes. A plan is
immutable and instantiates any number of runtimes; each gets its own tags, atoms, and direct
plan, and its shape is frozen: a planned runtime refuses `register_packed` and
`set_compile_options`, which stay for hand-assembled runtimes. Finalize also rejects two Rust
types sharing one `Userdata::NAME` and any member, global, key, or path spelled in a way the
generated definitions or the VM would choke on, so `Runtime::from_plan` has nothing left to
discover. `RuntimePlan::type_definitions()` renders the `.d.luau` for the composition in
Luau's `declare extern type` grammar, and `RuntimePlan::analysis_sources(inner)` serves each
module's canonical path (`require("@dream/quat")`) to the analysis frontend as a strict stub
returning the module's declared type. `RuntimePlan::check_definitions()` (feature `analysis`)
is the gate every extension crate's tests run: it loads the definitions into Luau's frontend
and type checks a strict script requiring every module, so a signature string that is not
Luau, or one naming a type that does not exist, fails with the frontend's diagnostics attributed
to the declaration. `tests/typed_definitions.rs` runs that gate and strict scripts against every
built-in module through `require`, with no compatibility global, so the declared API and the
runtime cannot drift apart. One distinction to keep: the compiler's known-library metadata
(folded constants, member types) reaches only modules exposed as compatibility globals, since
Luau's mechanism keys on a global name; a module reached through `require` is typed by the
analyzer but not folded by the compiler. Runtime-owned extension state drops before the VM
closes.

## Extension primitives

The shapes the migration audits asked for, all allocation-free at the boundary:
`convert::BytesView` accepts a Lua string or a Luau buffer without normalising;
`BufferView` reads and writes through bounds-checked copies (`read`, `write`, `fill`, `range`,
scalar helpers), never a safe slice, because a script can pass one buffer to two parameters;
the zero-copy slices are `unsafe fn bytes_unchecked`/`bytes_mut_unchecked` for trusted code
that proves nothing writes the buffer meanwhile, the rules `lua_tobuffer` imposes on C;
`convert::Exact<T>` reads an integer that never rounds (a Luau integer or an integer-valued
number, in range) for indices, offsets, counts, sizes, and ids, where the plain Rust integer
conversions keep OpenMW's rounding for compatibility; it is input only, results use a plain
Rust integer (an exact `number`), `Integer` (a Luau `integer`), or `Bits64`, which carries an opaque
64-bit pattern (a hash, a peer id) through a Luau integer with no numeric meaning, so a numeric
`u64` stays within what an integer or an exact number holds and nothing silently reinterprets;
`packed::BufferPack` reads and writes fixed layouts through a copy in one bounds check, and `packed::PackedScalar` puts a semantic value into one Luau integer (4-bit
kind, 4 flag bits, 56-bit payload) with the kind checked on every read. Kinds are a registry:
1 to 4 are l3i's own and fixed for good (a packed integer is a file and wire format), 5 to 15
are the application's, declared per extension (`ExtensionDescriptor::packed`) or per runtime
(`Runtime::register_packed`); a plan with two types on one number does not finalize, and a
`Packed<T>` crossing a VM where `T` is not the kind's registered owner is a logic error, and
an encoding whose kind, flags, or payload overflow its fields is an error rather than a
truncated integer; `options::Options`
reads camelCase option tables strictly (unknown keys are errors, required keys and field paths
are named); `sequence::Sequence` and `sequence::Stream` show a Rust collection to scripts as
`#items`, `items[i]`, `for item in items`, and `items:toTable()` (or `for` only, with a private
cursor per loop) without materialising it, declared through the planner like any userdata.

`raster::RasterExtension` (`dream.raster`, module `@dream/raster`) provides `raster::Color`
(kind 3: RGBA8 in the low 32 bits, red in bits 0 to 7, the same four bytes a vertex or a texel
holds, every `u32` valid) and `raster::ClipRect` (kind 4: four 14-bit pixel coordinates, so at
most 16383 on an axis; a deliberate limit of the packed form, not of the renderer, which takes
`u32` coordinates: a surface past 16K on an axis needs a clip type of its own), with `rgba8`, `rgb8`, `channels`, `packed`,
`withAlpha`, `lerp`, `mul`, `add`, `scale`, `premultiply`, `clip`, `clipBounds`, and folded
constants. Channel meaning is the consumer's: the renderer below reads colors as premultiplied.
Color arithmetic for GUI and shader-style scripts goes through `raster.math()`, a receiver
whose methods (`rgba8`, `rgb8`, `red`/`green`/`blue`/`alpha`, `channels`, `withAlpha`, `lerp`,
`mul`, `add`, `scale`, `premultiply`) lower to native code under `jit` when annotated
(`local C: dream_raster_Math = raster.math()`): shifts and masks to unpack, double arithmetic,
clamp, round, one integer store. Shader semantics: inputs clamp, results round to nearest, NaN
gives channel 0, and the interpreter path computes the same formulas. Measured per call
(`benches/raster.rs`): `rgba8` 71 ns through the module against 2.6 ns lowered, `lerp` 72 ns
against 15 ns, `mul` 61 ns against 17 ns, `premultiply` 52 ns against 16 ns.
`raster::Color16` is the wide form for formats that require 16 bits per channel: red in bits 0
to 15 through alpha in bits 48 to 63, the little-endian `u64` being an RGBA16 pixel. It fills
the whole Luau integer and so has **no kind nibble**: any integer is accepted as a `Color16`,
and nothing at runtime distinguishes it from an RGBA8 color or an id. That is the deliberate
price of exact interchange. `widen` (`x * 257`) and `narrow` (`round(x / 257)`) convert
exactly, every method has a `16` form on the receiver and the module, and the lowering is
shared: `lerp16` 69 ns against 14 ns, `narrow` 48 ns against 3 ns.

`quat::QuatExtension` (`dream.quat`, module `@dream/quat`; `axisAngle` and `fromXYZW` refuse a
zero or non-finite axis, angle, or quaternion, `slerp` refuses a non-finite weight on the bound
and the lowered path alike, and `key` takes the low four bits of an exact integer) is the first
packed kind: a unit
rotation compressed smallest-three into one Luau integer (18 bits per component, exact
identity, 1.6e-5 rad worst case), with `axisAngle`, `fromXYZW`/`toXYZW`, `mul`, `inverse`,
`slerp`, `rotate`, `angleTo`, the compiler-folded constant `IDENTITY`, `quat::AnimationKey`
(kind 2: the rotation plus four opaque flag bits, `key`/`keyRotation`/`keyFlags`), and, under `jit`,
`quat.math()`: a tagged receiver whose `rotate`, `mul`, `slerp`, `key`, `keyRotation`, and
`keyFlags` lower to IR when the script annotates it (`local Q: dream_quat_Math = quat.math()`):
rotate 21 ns and mul 45 ns per call in native code against 46 ns and 111 ns for an f32
quaternion userdata and 99 ns and 150 ns through the binder; slerp 86 ns against 167 ns and
214 ns, with polynomial trigonometry (acos to 2e-8, sine to 6e-8) shared by both paths so they
agree exactly; key plus keyRotation 5 ns against 220 ns. Only single-result, fixed-arity call
sites lower: bind a nested call's result to a local first, and an argument that is itself an
`if` expression or an `and`/`or` chain makes the compiler order the call differently, so bind
that to a local too. The packed form is storage and transport; long-lived rotation state stays
`quat::Quat` on the host, because re-encoding every blend step accumulates quantisation error.

## Networking

dream-net is runtime infrastructure, not a feature: l3i depends on it and owns the Luau bridge,
extension `dream.net`, module `@dream/net`, and every `RuntimePlan` carries l3i's own bridge:
the planner adds it, the id is reserved (an extension claiming `dream.net` fails the plan), so
no runtime lacks the network and the policy's capabilities decide what scripts may do with it.
Scripts build a frozen wire
schema from a strict option table (`net.schema{ version, channels, events }`), the host creates
`dream_net::Server`s in Rust and hands them over as `net::Server` handles (the private key never
reaches Luau), and scripts may create `net.client{ schema }` only when the policy grants the
`network.transport` capability. The hot calls are `update()` (reading the plan's clock, a
monotonic one by default or the host's through `RuntimePlanBuilder::network_clock`, so scripts
cannot spoof transport time and simulations can drive it), `pollInto(buffer)` returning `kind, peer, a, b, c` with one
payload copy into the caller's buffer and no allocation, `sendEvent(peer, eventId, bytes,
offset?, length?)` copying out of a buffer or string before it returns, and `flush()`. Ids are
integer64, sizes and counters plain numbers, connection stats direct fields on the client and
per-peer methods on the server. Nothing calls into Luau from inside the transport; the host
drives one network phase per frame. `benches/net.rs` measures the bridge over localhost UDP.

## Software rendering

With the `soft-render` feature, `soft_render::SoftRenderExtension` (`dream.soft_render`,
module `@dream/soft-render`, requires `dream.raster`) binds
[dream-soft-render](https://github.com/DreamWeave-MP/dream-soft-render) as a small software
rendering device. Layering rule: dream-net (runtime infrastructure) and dream-soft-render (an
experimental rendering primitive whose lowering lives next to the raster kinds) are the two
explicit l3i integrations, named here so they stay exceptions; every other DreamWeave crate
owns its l3i extension and depends upward on l3i, never the other way round. The device: `soft.renderer()`, `renderer:beginFrame(w, h)`, `frame:clear`, `frame:rect`,
`frame:image`, `frame:mesh(vertexBuffer, indexBuffer, texture?, clip)`, `frame:finish()`,
`renderer:createTexture(w, h, pixels)`, `texture:update(...)`, `renderer:readInto(buffer)`.
Draws rasterize immediately in call order, as the crate does; colors are `raster` integers the
renderer reads as premultiplied (`soft.premultiply` converts straight alpha); meshes are Luau
buffers of 20-byte vertices and `u32` indices borrowed for one call and never kept, validated
for stride and alignment here and for indices and finiteness by the crate. A scene drawn from
Luau is byte-identical to the same scene drawn from Rust (`tests/soft_render.rs` compares every
pixel), malformed input is an error in the renderer's own words, textures free their storage
when collected, and the extension is a plain domain extension: l3i does not depend on the
renderer unless the feature is on. `soft.vertices()` returns a writer whose
`write(buffer, offset, pos, uv, color)` packs a vertex with one bounds check and returns the
next offset; under `jit` it lowers to native buffer stores. Measured (`benches/soft_render.rs`,
640x480): a frame of 48 panels costs 363 µs from Luau against 353 µs native, 3200 glyph quads
5.1 ms against 4.8 ms, the 256-triangle fan 4.7 ms either way; an offscreen `frame:rect` call
is 213 ns against 102 ns native, of which the bound call itself (the namecall plus its four
arguments) is 84 ns; writing the fan's 258 vertices takes 506 ns per vertex with five
`buffer.write*` calls, 330 ns through the bound writer, and 70 ns through the lowered writer
(cos, sin, and `vector.create` included).

## Native code generation

With the `jit` feature the crate builds Luau's CodeGen library and a small C++ shim
(`csrc/codegen.cpp`) that exposes the C++-only parts of `Luau/CodeGen.h`: shared code
contexts with a size budget, `compile` with `CompilationOptions`, the userdata remapper, block
counters, and an `IrBuilder` C ABI. `native_code::NativeCodeHooks` lets hosts write Luau's
lowering hooks (vector access/namecall, userdata access/metamethod/namecall and their bytecode
type suggestions) in Rust against `native_code::ir::IrBuilder`, with `IrCmd` generated from
the headers of the exact Luau build. `VectorBufferWriter` is the default hook set: it lowers
`vector:writef32x3` to three native f32 stores; `quat::lowering::Lowering` (integer unpacking,
vector stores, no C call) and `soft_render::lowering::VertexWriter` (one bounds check and five
buffer stores) are the larger examples. Hooks learn the VM's real tags
and the compiler's userdata type indices from `NativeContext` (`tag_of`, `userdata_type_of`)
rather than assuming them; extensions register hook sets with `ExtensionDescriptor::native_hooks`.
Modes: off, annotated (`--!native`), eager.

Everything the shim needs is in the tree: Luau is the `luau/` submodule, so the build is the
same on every target and needs nothing outside the checkout.

## Coroutines, debugging, and the rest of the VM

- `thread::Thread`: host-driven coroutines (`start`, `resume`, `resume_with_error`, `reset`,
  status queries, sandboxed globals, thread data); bound functions yield with `bind::Yield` and
  request a debugger stop with `bind::Break`.
- `debug`: activation records, locals, arguments, upvalues, tracebacks, single stepping,
  breakpoints (`DebugAction::Break` stops a host-driven thread), coverage, and `RuntimeHooks`
  for every remaining `lua_Callbacks` slot.
- `memory`: `lua_gc` controls, allocation rate, memory and heap dumps, the buffer cage,
  userdata marks and embedder GC with weak references, light userdata with tags and names,
  coroutine finalizers, and fast-flag introspection.
- `libraries`: opening standard libraries one at a time, `luaL_sandbox`, `luaL_register`,
  `luaL_findtable`, table clone and clear, `concat`, `equal`, `less_than`, the `luaL_Strbuf`
  string builder, `load_with_env`, compile-time library members and constants, and the inliner.
- `require`: Luau's require-by-string runtime over a host `RequireNavigator`, with caching,
  proxy requires, registered modules, and cyclic-require placeholders.
- `native_code` (`jit`): also assembly and IR dumps for any target and the perf log.
- `analysis` (feature): Luau's type checker, linter, autocomplete, and parser over a
  `SourceProvider`, with diagnostics, spans, per-module strictness, AST JSON, definition
  files (`AnalysisOptions::definitions`, a plan's `.d.luau` for one) whose own errors fail
  `Analysis::new` with their text (`new_reporting` for them as values), and `PlanSources`,
  the provider that resolves a plan's module paths to typed stubs.

Every function in `lua.h`, `lualib.h`, `luacode.h`, `luacodegen.h`, `luajitinliner.h`, and
`Require.h` is declared in `raw::ffi`; the only exception is the varargs `lua_pushvfstring`.

## Safety model

- Luau is built with C++ exceptions. A Lua error inside a bound function unwinds through Rust
  frames (destructors run) to Luau's `pcall`; nothing puts `catch_unwind` on that path. Panics
  in native code abort. Host-level raising calls run under `lua_pcall` (`raw::protect`).
- Every `unsafe` block states the invariant it relies on. GC destructors never call the Lua API.
- `Value` holds a weak handle to its runtime's lifetime token: a value that outlives its
  `Runtime` becomes invalid instead of touching a closed VM.
- `Runtime::stack` leases the root stack; a second root while one is alive and not suspended in
  a Lua call panics, so no two frames can alias one stack region. Native-call stacks only arise
  from Luau calling into Rust.
- Untagged type identity is a leaked per-type address recorded by exact `TypeId`, never a hash.
- `build.rs` owns `-DLUA_UTAG_LIMIT=254`; `l3i::TAG_LIMIT` mirrors it. Tag 0 is Luau's
  untagged default; nothing else is reserved.
- Fast flags follow OpenMW's policy (plus `LuauExperimentalIfLocalSyntax`), frozen before the
  first VM or compile.

`QUESTIONABLE.md` lists C++ behaviours ported faithfully but flagged, and the deliberate
divergences.

## Building

Luau is a git submodule (`luau/`, pinned at release 0.740, the commit OpenMW pins); clone with
`--recurse-submodules` or run `git submodule update --init`. `build.rs` compiles it and the
binder's C++ additions with `cc` (in parallel): `LUAI_MAXCSTACK=8000`, three-component vectors,
`LUA_UTAG_LIMIT=254`, `-fno-math-errno`, Cargo's optimisation level (`-O3` in release) with
no `-march` so the binary stays portable, Luau's internal assertions whenever Rust's debug
assertions are on (a failed one prints its location before trapping; a release build with
debuginfo keeps the release VM), CodeGen under `jit`, Analysis under `analysis`; `soft-render`
adds the dream-soft-render dependency. Compiled chunks default to optimisation level 2 and
debug level 1, and runtime plans turn type information on so native code sees the userdata
types. The binder reads argument slots straight from Luau's 16-byte
value layout; `csrc/extra.cpp` pins every offset at compile time and each runtime proves the
mirror against the API once at creation, so a Luau bump that moves a byte fails at once. No network access at build time and no external Lua crate. Hosts may append
compiler flags through `LUAU_CXXFLAGS`. Rust 1.88 or newer.

The toolchain is fixed: **clang++ for the C++ side, lld, and cross-language thin LTO**
(`-Clinker-plugin-lto -Clinker=clang -Clink-arg=-fuse-ld=lld`), with clang and rustc on the
same LLVM major. Measured against gcc, that configuration is the only one that makes the binder
hot paths faster (8 to 15 percent, from the Rust thunks and Luau's API inlining into each
other) and it is also the fastest clean build. `build.rs` refuses other configurations;
`L3I_UNVERIFIED_TOOLCHAIN=1` downgrades that to a warning, and docs.rs is exempt. This
repository's `.cargo/config.toml` sets everything for Linux, macOS, and MSVC targets; a
dependent crate copies its `[env]` and `rustflags` lines. [TOOLCHAIN.md](TOOLCHAIN.md) has the
measurements.

## Quality

`cargo test` (and `cargo test --all-features`) run the ported C++ test contracts plus the
runtime, sandbox, watchdog, native code, analysis, and renderer suites; `cargo clippy --all-targets --
-D warnings` is clean. The integration tests under `tests/` link as one binary (each binary
pays a full link-time codegen under cross-language LTO), so one file's tests run with
`cargo test <file>::`. `BENCHMARKS.md` holds the Criterion numbers for the hot paths
(`cargo bench --bench hot_paths`, then `python3 scripts/gen_benchmarks.py`).

Past a few nanoseconds wall time is noise, so the binder's own cost is tracked in retired
instructions and cycles per call, read from the CPU's counters by `cargo bench --bench
instructions` (Linux, `perf_event_paranoid` at 2 or lower, no `perf` binary needed). The loop
is subtracted, the minimum over seven rounds is reported, and the floors are measured the same
way: a hand-written `lua_CFunction` and a typed direct handler. On the pinned Luau 0.740:

| Per call | Instructions | Cycles |
|---|---:|---:|
| hand-written `lua_CFunction (f64, f64)` | 277 | 69 |
| bound `() -> f64` | 292 | 73 |
| bound `(f64, f64) -> f64` | 369 | 89 |
| bound `(Vector3) -> f64` | 332 | 85 |
| bound `(Packed<Color>) -> f64` | 364 | 89 |
| typed direct namecall, the VM's leanest method path | 368 | 98 |
| planned method `() -> f64` | 431 | 107 |
| planned getter | 397 | 101 |
| planned direct field | 78 | 12 |

A bound call is within about forty instructions of a bare C function; a planned method is
within sixty of the typed direct handler. The rest is Luau's own call and return machinery.
A packed argument pays eleven instructions for the kind registry check (one owner load and a
type compare) on top of its bit decode.

The same harness reads the L1 data, L1 instruction, last-level cache, data and instruction
TLB, and branch-miss counters. Every scenario reports fewer than 0.005 of each per call: the
whole path, Luau's and the binder's, stays in L1 and predicts. The binder's side of a call
touches four lines of its own, the slot row (rows are 32 bytes, so a row never straddles a
line), the member's context, the thread record, and the shared block's first line, which
holds the slot table, the plan, and the profiler switch (`#[repr(C, align(64))]`).

Memory per runtime, after a full collection: a bare VM's heap is 64 KiB, with `dream.quat`
and `dream.raster` 84 KiB, with `dream.soft_render` as well 96 KiB. The binder's own state is
a 600-byte shared block, 32 bytes per plan slot, 64 bytes per plan entry, and the dense
`(tag, kind, atom)` table, sized by the tags and atoms the plan uses (1.4 KB for five tags and
39 atoms).

## License

MIT OR Apache-2.0.
