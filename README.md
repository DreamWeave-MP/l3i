# l3i

**l3i** is a Rust binder for [Luau](https://luau.org) 0.740, ported from the OpenMW
Luau binder (`components/luau` and `components/lua/bindfunction.hpp`). It owns its Luau build,
declares the C API by hand, and puts a safe, scoped layer over it: frame-scoped stack views,
registry-pinned owned values, a typed function binder that reads arguments straight from stack
slots, tagged and untagged userdata, Luau's direct userdata access, sandboxed script instances,
watchdog and profiler plumbing, and (with the `jit` feature) Luau's native code generator with
lowering hooks written in Rust.

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
l3i = { version = "0.1", features = ["jit"] } # jit is optional
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
handler serves a type that is tag 8 in one VM and tag 17 in another. `direct::Registry` is the
static alternative for hosts whose identities really are compile-time constants. `DirectAccess`
handlers run on both the direct path and the ordinary metamethod path with the original
metamethod retained as the fallback; `direct::field` registers per-field getters that write
straight into the destination register. `Runtime::install_vector_buffer_writer` adds
`vector:writef32x3(buffer, offset)`.

## Native code generation

With the `jit` feature the crate builds Luau's CodeGen library and a small C++ shim
(`csrc/codegen.cpp`) that exposes the C++-only parts of `Luau/CodeGen.h`: shared code
contexts with a size budget, `compile` with `CompilationOptions`, the userdata remapper, block
counters, and an `IrBuilder` C ABI. `native_code::NativeCodeHooks` lets hosts write Luau's
lowering hooks (vector access/namecall, userdata access/metamethod/namecall and their bytecode
type suggestions) in Rust against `native_code::ir::IrBuilder`, with `IrCmd` generated from
the headers of the exact Luau build. `VectorBufferWriter` is the default hook set: it lowers
`vector:writef32x3` to three native f32 stores. Modes: off, annotated (`--!native`), eager.

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
  `SourceProvider`, with diagnostics, spans, per-module strictness, and AST JSON.

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
`LUA_UTAG_LIMIT=254`, Luau's internal assertions in debug builds, CodeGen under `jit`, Analysis
under `analysis`. No network access at build time and no external Lua crate. Hosts may append
compiler flags through `LUAU_CXXFLAGS`. Rust 1.88 or newer.

The toolchain is fixed: **clang++ for the C++ side, lld, and cross-language thin LTO**
(`-Clinker-plugin-lto -Clinker=clang -Clink-arg=-fuse-ld=lld`), with clang and rustc on the
same LLVM major. Measured against gcc, that configuration is the only one that makes the binder
hot paths faster (8 to 15 percent, from the Rust thunks and Luau's API inlining into each
other) and it is also the fastest clean build. `build.rs` refuses other configurations;
`L3I_UNVERIFIED_TOOLCHAIN=1` downgrades that to a warning. This repository's
`.cargo/config.toml` sets everything; a dependent crate copies its `[env]` and `rustflags`
lines. [TOOLCHAIN.md](TOOLCHAIN.md) has the measurements.

## Quality

`cargo test` (and `cargo test --all-features`) run the ported C++ test contracts plus the
runtime, sandbox, watchdog, native code, and analysis suites; `cargo clippy --all-targets --
-D warnings` is clean. The integration tests under `tests/` link as one binary (each binary
pays a full link-time codegen under cross-language LTO), so one file's tests run with
`cargo test <file>::`. `BENCHMARKS.md` holds the Criterion numbers for the hot paths
(`cargo bench --bench hot_paths`, then `python3 scripts/gen_benchmarks.py`).

## License

MIT OR Apache-2.0.
