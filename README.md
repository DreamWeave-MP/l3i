# l3i

**The Luau runtime for DreamWeave.** A Rust binder for [Luau](https://luau.org) 0.740 that
owns its own Luau build, plans every VM before it exists, and lowers hot script calls to native
code.

l3i is its author's second Luau binder. The first is C++: `components/luau`, which Dave Corley
(S3kshun8) wrote for OpenMW on the
[`feat/least-mergeable-branch`](https://gitlab.com/magicaldave1/openmw/-/commits/feat/least-mergeable-branch)
branch of his fork. That branch is not in OpenMW's master and may never be: OpenMW's own
scripting is `components/lua`, and OpenMW has no Luau binder. l3i is that binder done again in
Rust, then given what a multi-crate engine needs on top. "The C++ binder" in the documentation
is that one, and every `components/luau/...` file and `testluau*.cpp` test the sources cite is a
file on that branch, at [commit `f69579c8`](https://gitlab.com/magicaldave1/openmw/-/tree/f69579c8d54fb2ae4d7a79a4d7033205effaec0e/components/luau).
[Where the fast paths came from](https://DreamWeave-MP.github.io/l3i/docs/performance/#where-the-fast-paths-came-from) links each optimisation to the C++ lines it
started as. l3i is not affiliated with or endorsed by the OpenMW project.

Documentation: **<https://DreamWeave-MP.github.io/l3i/>**. This file is the short version.

- [What it is](#what-it-is)
- [Quick start](#quick-start)
- [What you get](#what-you-get)
- [Features](#features)
- [The rules](#the-rules)
- [Building](#building)
- [Quality](#quality)
- [License](#license)

## What it is

| | |
|---|---|
| **No `mlua`, no binding crate** | Luau is a git submodule compiled by `build.rs`. The whole C API is declared by hand in `l3i::ffi`, and a safe, scoped layer sits over it. |
| **Arguments read from the value layout** | The typed binder reads stack slots straight from Luau's 16-byte `TValue`. A bound `(f64, f64) -> f64` call retires 369 instructions against 277 for a bare `lua_CFunction`. |
| **Tags, atoms and slots are plan data** | A `RuntimePlan` assigns userdata tags, Luau's 32 compiler type slots, atoms and direct-access slots per VM, so one Rust type can be tag 8 in one runtime and untagged in another. |
| **Typed by construction** | Every module member carries a Luau signature. The plan renders the `.d.luau`, and the `analysis` feature type checks strict scripts against it in the crate's own tests. |
| **The network is not optional** | Every plan carries the `dream.net` bridge; the policy's capabilities decide what a script may do with it. |
| **Lowering hooks in Rust** | With `jit`, hosts write Luau's userdata and vector lowering hooks against an `IrBuilder` C ABI. Quaternions, colours, byte reads and vertex writes compile to IR with no C call. |

Nothing here is a catalogue: tags, atoms, type names and debug-name roots are host data.

## Quick start

```toml
[dependencies]
l3i = { version = "1", features = ["jit"] }   # jit, analysis, bytes, soft-render are optional
```

l3i builds only with **clang, lld and cross-language thin LTO**, and Cargo does not inherit a
dependency's config, so copy the `[env]` and `rustflags` lines from this repository's
[`.cargo/config.toml`](.cargo/config.toml) into your own. `build.rs` refuses anything else and
names the missing piece; `L3I_UNVERIFIED_TOOLCHAIN=1` turns that into a warning.
[Start here](https://DreamWeave-MP.github.io/l3i/docs/start-here/) walks through it.

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

That is the hand-assembled runtime. An engine composes one from extensions instead:

```rust
use l3i::Runtime;
use l3i::extension::{RuntimePlan, RuntimePolicy};
use l3i::quat::QuatExtension;
use l3i::raster::RasterExtension;

let plan = RuntimePlan::builder()
    .policy(RuntimePolicy::new().compat_global("@dream/quat", "quat"))
    .extension(QuatExtension)
    .extension(RasterExtension)
    .finalize()?;
let runtime = Runtime::from_plan(&plan)?;            // any number of these per plan
std::fs::write("dream.d.luau", plan.type_definitions())?;
```

## What you get

Each row is a page of the guide.

| Area | In one line | Read |
|---|---|---|
| **Stack and values** | Borrowed views bound to a `Frame`, owned registry pins, and a typed binder that accepts scalars, views, `&T` receivers, `Option<T>` with OpenMW's rules, `VarArgs`, `Overload`, and returns tuples, `Owned<T>`, `Result<T>` and more. | [Stack and values](https://DreamWeave-MP.github.io/l3i/docs/stack-and-values/) |
| **Userdata** | Tagged (inline payload, one tag read to check) or untagged (exact metatable identity, borrowed engine objects), decided per runtime; `MetatableBuilder` for methods, properties, metamethods and `__iter` factories; generated dispatchers that cost one Luau call frame. | [Userdata](https://DreamWeave-MP.github.io/l3i/docs/userdata/) |
| **Modules and sandboxes** | Frozen package tables, read-only views, OpenMW's prelude, one environment per script instance cloned from a compiled template, `require` over a host navigator. | [Modules and sandboxes](https://DreamWeave-MP.github.io/l3i/docs/modules-and-sandboxes/) |
| **Runtime options** | Watchdog on time and heap, memory categories, call scopes with self-time accounting, collector control, a sampling profiler, frozen fast flags. | [Runtime options](https://DreamWeave-MP.github.io/l3i/docs/runtime-options/) |
| **Direct access** | Atoms let `GETTABLEKS`/`NAMECALL` reach a native callback with no metatable walk; `DirectPlan` validates Luau's inline cache in O(1) and serves a type under any tag. | [Direct access](https://DreamWeave-MP.github.io/l3i/docs/direct-access/) |
| **Extensions and plans** | A crate declares its Luau surface once (`describe`), a plan composes crates and assigns every tag, slot and atom (`finalize`), a runtime is built from it (`from_plan`); the plan renders the `.d.luau` and can type check it. | [Extensions](https://DreamWeave-MP.github.io/l3i/docs/extensions/) |
| **Primitives** | `BytesView`, `BufferView`, `Exact<T>`, `Integer`, `Bits64`, `PackedScalar` with a kind registry, strict `Options` tables, in-place table walks, `Sequence` and `Stream` views. | [Primitives](https://DreamWeave-MP.github.io/l3i/docs/primitives/) |
| **Built-in extensions** | `@dream/net` (in every plan), `@dream/quat`, `@dream/raster`, `@dream/bytes`, `@dream/intern`, `@dream/soft-render`. | [Built-in extensions](https://DreamWeave-MP.github.io/l3i/docs/builtin-extensions/) |
| **Native code** | Luau's CodeGen with lowering hooks written in Rust; which call sites lower and why. | [Native code](https://DreamWeave-MP.github.io/l3i/docs/native-code/) |
| **The rest of the VM** | Coroutines, the debug API, memory and GC controls, libraries, `require`, Luau's analysis frontend. | [Coroutines, debugging and the rest](https://DreamWeave-MP.github.io/l3i/docs/vm/) |
| **Rust API** | Every public module. | [Rust API](https://DreamWeave-MP.github.io/l3i/docs/api/) |

### Built-in extensions

| Extension | Module | What it is |
|---|---|---|
| `dream.net` | `@dream/net` | The dream-net bridge, in every plan: schemas, host-created servers, clients behind the `network.transport` capability, `pollInto` with one payload copy and no allocation. |
| `dream.quat` | `@dream/quat` | Unit rotations packed into one Luau integer (smallest-three, 18 bits per component) and animation keys; `quat.math()` lowers `rotate` to 21 ns native. |
| `dream.raster` | `@dream/raster` | RGBA8 colours, clip rectangles and RGBA16 colours as packed integers; `raster.math()` lowers colour arithmetic. |
| `dream.bytes` (`bytes`) | `@dream/bytes` | For parsing foreign file formats in script: searching, C strings, varints, big-endian and half-float reads lowered natively, plus codecs, digests and text codepages behind `bytes-codecs`, `bytes-digests`, `bytes-text`. |
| `dream.intern` (`intern`) | `@dream/intern` | Textual identity as dense numbers: pools that intern strings or buffer spans under exact or ASCII case-insensitive equality, folding inside the hash, with no Luau string made for a duplicate. |
| `dream.soft_render` (`soft-render`) | `@dream/soft-render` | dream-soft-render as a CPU rendering device, byte-identical from Luau and from Rust. |

## Features

| Feature | Adds | Extra dependencies |
|---|---|---|
| `jit` | Luau's CodeGen, lowering hooks in Rust, IR and assembly dumps | none |
| `analysis` | Luau's type checker, linter, autocomplete and parser; `RuntimePlan::check_definitions` | none |
| `bytes` | The `@dream/bytes` extension | `memchr` |
| `bytes-codecs` | DEFLATE, LZ4, zstd and LZMA/XZ | `miniz_oxide`, `lz4_flex`, `ruzstd`, `lzma-rs` |
| `bytes-digests` | CRC-32 to BLAKE3, one-shot and incremental | `crc32fast`, `xxhash-rust`, RustCrypto, `blake3` |
| `bytes-text` | Every WHATWG text encoding | `encoding_rs` |
| `intern` | The `@dream/intern` extension | none |
| `soft-render` | The `dream.soft_render` extension | `dream-soft-render` |

The default feature set is empty. Networking is not a feature: `dream-net` is a dependency.
Everything optional is pure Rust.

## The rules

The short form of the [safety model](https://DreamWeave-MP.github.io/l3i/docs/safety/), which
also records the Luau facts the ecosystem builds on and the deliberate divergences from the C++
binder.

- Luau is built with C++ exceptions. A Lua error inside a bound function unwinds through Rust
  frames to Luau's `pcall`; nothing puts `catch_unwind` on that path. A panic inside a native
  call aborts. Host-level raising calls run under `lua_pcall`.
- `Stack` is exclusive and never pops; temporaries live in a `Frame`; one open child frame per
  scope; views cannot escape the frame that owns their slot.
- Receivers are `&T`, never `&mut T`, because one userdata can appear in several argument
  slots of one call. Mutation goes through interior mutability.
- Bound callables are `Fn`, not `FnMut`: a binding can re-enter itself through Lua.
- Every `unsafe` block states the invariant it relies on. GC destructors never call the Lua API.
- Tags are `1..254`; tag 0 is Luau's untagged default. Fast flags follow OpenMW's policy (plus
  `LuauExperimentalIfLocalSyntax`) and are frozen before the first VM or compile.
- Luau integers (`42i`) compare with `==` only and never equal a number, so identities (peers,
  events, handles, packed scalars) are integers and everything a script counts or thresholds is
  a plain number.

## Building

| | |
|---|---|
| Luau | git submodule `luau/`, release 0.740 (the commit OpenMW pins); `git clone --recurse-submodules` |
| C++ side | compiled by `build.rs` with `cc`: `LUAI_MAXCSTACK=8000`, three-component vectors, `LUA_UTAG_LIMIT=254`, `-fno-math-errno`, no `-march` |
| Toolchain | clang++, lld, cross-language thin LTO, clang and rustc on the same LLVM major; enforced by `build.rs` |
| Apple targets | clang and lld without `-Clinker-plugin-lto` (ld64.lld rejects it); macOS forfeits the cross-language inlining |
| MSVC targets | `clang-cl` compiles, rustc links with `lld-link` |
| Rust | 1.92 or newer, edition 2024 |
| Network at build time | none |

The value layout the binder reads is pinned by `static_assert`s in `csrc/extra.cpp` and proven
against the API once per runtime, so a Luau bump that moves a byte fails at once. Every Luau
API function in `lua.h`, `lualib.h`, `luacode.h`, `luacodegen.h`, `luajitinliner.h` and
`Require.h` is declared in `l3i::ffi`, except the varargs `lua_pushvfstring`.
[TOOLCHAIN.md](TOOLCHAIN.md) has the measurements behind the rule and
[Building](https://DreamWeave-MP.github.io/l3i/docs/building/) the whole procedure.

## Quality

`cargo test`, `cargo test --all-features`, `cargo clippy --all-targets --all-features -- -W
clippy::pedantic -D warnings` and `cargo fmt --check` are clean on every commit. The integration
tests link as one binary (`cargo test <file>::` runs one file), since each binary pays a full
link-time codegen under cross-language LTO. [`BENCHMARKS.md`](BENCHMARKS.md) holds the Criterion
numbers for the hot paths.

Wall time is noise past a few nanoseconds, so the binder's own cost is tracked in retired
instructions per call from the CPU's counters (`cargo bench --bench instructions`), the loop
subtracted, on the pinned Luau 0.740:

| Per call | Instructions | Cycles |
|---|---:|---:|
| hand-written `lua_CFunction (f64, f64)` | 277 | 69 |
| bound `(f64, f64) -> f64` | 369 | 89 |
| typed direct namecall, the VM's leanest method path | 368 | 98 |
| planned method `() -> f64` | 431 | 107 |
| planned direct field | 78 | 12 |

Every scenario reports fewer than 0.005 cache, TLB and branch misses per call. A bare VM's heap
is 64 KiB after a full collection; the binder's own state is a 600-byte shared block plus 32
bytes per plan slot. [Compatibility and performance](https://DreamWeave-MP.github.io/l3i/docs/performance/)
has every table.

## License

MIT OR Apache-2.0. Luau is Roblox's, under the MIT license; its notice ships in the package as
`luau/LICENSE.txt` and `luau/lua_LICENSE.txt`.
