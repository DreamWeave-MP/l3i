+++
title = "Rust API"
description = "Every public module, type, function and constant in l3i, page by page, and what the crate root re-exports."
template = "docs/section.html"
page_template = "docs/page.html"
sort_by = "weight"
weight = 200

[extra]
kind = "api"
hide_child_cards = true
+++

```toml
[dependencies]
l3i = "1"
```

The crate is `l3i`. Its modules are public and are reached by their module path
(`l3i::runtime::Runtime`, `l3i::userdata::tagged::register`); only the items below are also at
the root.

| Feature | Adds | Module |
|---|---|---|
| `jit` | Luau's code generator, host lowering hooks written in Rust, the IR builder, assembly and IR dumps, the perf log | `native_code` |
| `analysis` | Luau's type checker, linter, autocomplete and parser over a host source provider; `RuntimePlan::check_definitions` | `analysis` |
| `soft-render` | dream-soft-render as the `dream.soft_render` extension and `@dream/soft-render` module | `soft_render` |
| `bytes` | The `dream.bytes` extension and `@dream/bytes` module: searching, record strings, varints, the widths and orders `buffer` lacks, and the `bytes.math()` receiver lowered under `jit` | `bytes`, `bytes::numeric` |
| `bytes-codecs` | `inflate`/`deflate` (zlib, raw, gzip), LZ4 blocks and frames, zstd and LZMA/XZ decoding | `bytes::codecs` |
| `bytes-digests` | CRC-32, Adler-32, FNV-1a, xxHash, MD5, SHA-1, SHA-256, BLAKE3, one-shot and incremental | `bytes::digests` |
| `bytes-text` | Decoding and encoding every WHATWG-labelled text encoding | `bytes::text` |
| `intern` | The `dream.intern` extension and `@dream/intern` module: pools that turn strings and buffer spans into dense number identities | `intern` |

The default feature set is empty. Networking is not a feature: `dream-net` is a dependency and
every runtime plan carries the `dream.net` bridge.

| Page | Covers | Modules |
|---|---|---|
| [Runtime](@/docs/api/runtime.md) | `Runtime`, `RuntimeBuilder`, `CallScope`, `CallContext`, `MemoryCategory`, `Limits`, the profiler statistics, `Sandbox`, `Template`, `InstanceSpec`, `CompileOptions`, the flag policy, `Error`, diagnostics helpers, debug names | `runtime`, `runtime::profiler`, `sandbox`, `source`, `flags`, `error`, `diagnostics`, `debug_name` |
| [Stack and values](@/docs/api/stack.md) | `Stack`, `Frame`, `ValueView`, `TableView`, `Type`, `Scope`, `Value`, `Table`, `Function`, `FunctionView`, `CallResults`, `PushArgs`, every conversion type, `Options` | `stack`, `value`, `call`, `convert`, `options` |
| [Binding and userdata](@/docs/api/bind.md) | `bind_function` shapes, `Call`, `Param`, `Return`, `VarArgs`, `ArgView`, `Overload`, `Variadic`, `ResultOrError`, `NilThen`, `Yield`, `Break`, `Userdata`, `tagged`, `untagged`, `MetatableBuilder`, iterators, `Owned`, `Borrowed`, `Storage`, `StableRef`, `LuauModule`, `ModuleBuilder`, read-only tables, `RequireNavigator`, the standard libraries | `bind`, `userdata`, `module`, `readonly`, `require`, `libraries` |
| [Direct access and the VM](@/docs/api/direct.md) | `AtomCatalogue`, `DirectPlan`, `Registry`, `DirectAccess`, direct fields, `native::enter`, the vector buffer writer, `GcControl`, memory dumps, the buffer cage, embedder GC, `Thread`, the debug API and `RuntimeHooks` | `direct`, `native`, `vector_writer`, `memory`, `thread`, `debug` |
| [Extensions and primitives](@/docs/api/extension.md) | `Extension`, `ExtensionDescriptor`, `UserdataBuilder`, `ModuleDecl`, `TagPolicy`, `CompilerTypePolicy`, `RuntimePlan`, `RuntimePolicy`, `InstallContext`, `PackedScalar`, `Packed`, `BufferPack`, `Sequence`, `Stream`, the `dream.quat`, `dream.raster`, `dream.net` and `dream.soft_render` extensions | `extension`, `packed`, `sequence`, `quat`, `raster`, `net`, `soft_render` |
| [Native code and analysis](@/docs/api/native-code.md) | `NativeCodeGen`, `NativeCodeOptions`, `NativeCodeHooks`, `NativeContext`, `IrBuilder`, `IrCmd`, dumps and execution statistics; `Analysis`, `AnalysisOptions`, `SourceProvider`, `Definitions`, `PlanSources` | `native_code`, `analysis` |
| [The raw C API](@/docs/api/ffi.md) | The `ffi` module, `TAG_LIMIT`, `LUAU_VERSION`, and the rules for hand-written `lua_CFunction`s | `ffi` |

## The root

Re-exported from their modules:

```text
Error, Result          error
Runtime                runtime
```

Declared at the root:

```text
pub mod ffi                          the raw Luau C API (raw::ffi, re-exported)
pub const TAG_LIMIT: u8              254: valid userdata tags are 1..TAG_LIMIT
pub const LUAU_VERSION: &str         "0.740"
pub const VERIFIED_TOOLCHAIN: bool   whether build.rs found the verified toolchain
```

`Result<T>` is `std::result::Result<T, l3i::Error>` and is what every fallible function in the
crate returns. [The raw C API](@/docs/api/ffi.md) covers `ffi` and the two VM constants; [Building](@/docs/building.md#the-toolchain-rule) covers `VERIFIED_TOOLCHAIN`.

## Conventions

- **Errors.** A function returns `Err` for a host logic error (`Error::Logic`: misuse of the
  API, a registration conflict, a value from another VM) or for a Lua error that a protected
  call caught (`Error::Runtime`). Inside a bound function nothing raises on its own: returning
  `Err` from the callable, or a `Result<T>` whose `Err` the binder pushes, raises the message
  as the Lua error the script sees. `Error::LuaErrorOnStack` re-raises the error object already
  on top instead of replacing it with a message. `Error::Permission` is a capability the
  runtime policy did not grant, kept apart from `Runtime` so a denial never reads as an
  operating-system failure. Host-level calls that could raise (a `__index` metamethod, a store
  into a read-only table) run under `lua_pcall` and come back as `Err`; the same call inside a
  native call raises straight through to Luau's `pcall`.
- **Frames.** `Stack` is the exclusive handle to a thread's stack and never pops. Temporaries go
  through a `Frame`, and every view a frame returns borrows the frame. A scope has at most one
  open child frame: opening a sibling panics at its opening line. Inside a bound function read
  the arguments before opening a frame, or open the frame from the `Call`. `Runtime::stack`
  leases the root stack; a second root while one is alive and not suspended in a Lua call
  panics. Helpers that take `&Runtime` lease the root themselves, so call them between root
  stacks.
- **Receivers are `&T`.** A userdata can appear in several argument slots of one call, so no
  method receives `&mut T`. Mutation goes through interior mutability in the payload;
  `tagged::test_mut` is the `unsafe` escape hatch. Bound callables are `Fn`, not `FnMut`, for
  the same reason: a binding can re-enter itself through Lua.
- **`Result<T>` from a bound function raises.** `Ok(value)` pushes `value`'s results; `Err`
  raises the message with the caller's location, in `luaL_error` wording where the C++ binder
  had it (`invalid argument #2 to 'name' (number expected, got string)`).
- **Integers.** Luau 0.740 integers (`42i`) compare with `==` only; `<` between two integers
  raises, and an integer never equals a number. l3i pushes identities (peer, event, channel and
  client ids, hashes, handles, packed scalars) as Luau integers through `convert::Integer`,
  `Bits64` and `Packed<T>`, and everything scripts threshold or count (sizes, lengths, counters)
  as plain numbers: `i64`, `u64` and the other Rust integers push numbers. Reading a plain Rust
  integer from a Lua number rounds half away from zero (OpenMW's rule); `convert::Exact<T>`
  refuses a fractional or out-of-range value.
- **Time.** Watchdog and profiler times are `std::time::Duration` on the way in and
  milliseconds as `f64` in the statistics. The network bridge's transport clock is `f64`
  seconds, monotonic, read from the plan's clock or the host's `network_clock`, never from a
  script. `Runtime::clock` is Luau's own high-resolution clock in seconds.
- **Names are host data.** Every native function and userdata type is named under one of the
  runtime's debug roots (`dreamweave` by default); tags, atoms and packed kinds are chosen by
  the host or the planner, never by the crate.
