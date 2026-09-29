+++
title = "Start here"
description = "Add the crate, set up the clang and lld toolchain it requires, fetch the Luau submodule, and run a first program that exposes a Rust type to a script."
weight = 10

[extra]
kind = "tutorial"
+++

## Add the crate

```toml
[dependencies]
l3i = { version = "1", features = ["jit"] }
```

The crate's name in code is `l3i`. It needs Rust 1.92 or newer and edition 2024. Three features
are optional:

| Feature | Adds |
|---|---|
| `jit` | Luau's CodeGen library and the C++ shim behind `native_code`: native compilation and lowering hooks written in Rust |
| `analysis` | Luau's Analysis library behind a C++ shim: the type checker, linter, autocomplete and parser in `analysis` |
| `soft-render` | dream-soft-render as the `dream.soft_render` extension |
| `bytes` | The `dream.bytes` extension for parsing foreign formats: searching, record strings, varints, big-endian and half-float reads, natively lowered under `jit` |
| `bytes-codecs`, `bytes-digests`, `bytes-text` | Its codecs (DEFLATE, LZ4, zstd, LZMA), digests (CRC-32 to BLAKE3) and text encodings, each pulling in its pure-Rust dependencies |

dream-net is not a feature: l3i depends on it and owns the Luau bridge to it, so every runtime
has the network and the policy decides what scripts may do with it.

## The toolchain

l3i builds only with clang, lld and cross-language thin LTO. `build.rs` compiles Luau with
`clang++` and emits `-flto=thin`, the Rust side emits bitcode, and lld runs one thin LTO over
both languages. That is the configuration under which the Rust thunks and the Luau API calls
they make inline into one another; measured against gcc it makes the binder's hot paths 8 to 15
percent faster and is also the fastest clean build. `build.rs` checks every half of the chain
and refuses anything else, naming the missing piece.

Cargo does not inherit a dependency's `.cargo/config.toml`, so a crate that depends on l3i
copies these `[env]` and `rustflags` lines from l3i's `.cargo/config.toml` into its own:

```toml
[env]
CXX = "clang++"
CXX_x86_64-pc-windows-msvc = "clang-cl"
CXX_aarch64-pc-windows-msvc = "clang-cl"

[target.x86_64-unknown-linux-gnu]
rustflags = ["-Clinker-plugin-lto", "-Clinker=clang", "-Clink-arg=-fuse-ld=lld"]

[target.aarch64-unknown-linux-gnu]
rustflags = ["-Clinker-plugin-lto", "-Clinker=clang", "-Clink-arg=-fuse-ld=lld"]

[target.x86_64-apple-darwin]
rustflags = ["-Clinker=clang", "-Clink-arg=-fuse-ld=lld"]

[target.aarch64-apple-darwin]
rustflags = ["-Clinker=clang", "-Clink-arg=-fuse-ld=lld"]

[target.x86_64-pc-windows-msvc]
rustflags = ["-Clinker-plugin-lto", "-Clinker=lld-link"]

[target.aarch64-pc-windows-msvc]
rustflags = ["-Clinker-plugin-lto", "-Clinker=lld-link"]
```

Environment variables override every entry. Apple targets leave out `-Clinker-plugin-lto`
because rustc passes that flag's GNU `-plugin-opt` arguments to the linker and `ld64.lld`
rejects them; there clang still drives `ld64.lld`, rustc's own release thin LTO covers the Rust
side, and macOS pays a few percent on the hot paths.

clang and rustc must be on the same LLVM major. `rustc -vV` prints rustc's, `clang++ --version`
prints clang's.

```sh
# Fedora
sudo dnf install clang lld
# Debian and Ubuntu, where N is rustc's LLVM major
sudo apt install clang-N lld-N
```

Apple's clang is not upstream LLVM; a Homebrew or nightly LLVM whose major matches is what
works there.

{% callout(kind="note", title="Downgrading the refusal") %}
`L3I_UNVERIFIED_TOOLCHAIN=1` turns the refusal into a warning, for a build that only needs to
compile. docs.rs is exempt automatically, since it only renders documentation. A build without
the lld flag fails at the final link with `bad -plugin-opt option`, because clang hands the
bitcode objects to `ld.bfd`; `-Clinker-plugin-lto` without a clang-built Luau links but forfeits
the gain. Both are why `build.rs` checks up front.
{% end %}

## Building from a checkout

Luau is the `luau/` git submodule, pinned at release 0.740, the commit OpenMW pins. A crates.io
package carries the Luau sources it needs; a checkout has to fetch them:

```sh
git clone --recurse-submodules https://github.com/DreamWeave-MP/l3i
# or, in an existing checkout
git submodule update --init
```

`build.rs` compiles Luau and the binder's C++ additions under `csrc/` with the `cc` crate, in
parallel, with no network access. [Building](@/docs/building.md) lists the defines it owns.

## The whole program

A tagged userdata type with a property and a method, a constructor bound from a Rust closure,
and a script that uses both.

```rust
use l3i::Runtime;
use l3i::userdata::{Owned, Userdata, tagged};

struct Vec3 {
    x: f32,
    y: f32,
    z: f32,
}

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
    let length: f64 = runtime.eval("return vec3(1, 2, 2):length()")?;
    println!("length {length}");
    Ok(())
}
```

It prints:

```text
length 3
```

## What happened

**The runtime.** `Runtime::new()` froze the process-wide fast-flag policy, created a `lua_State`,
seeded Luau's pointer-encoding key from OS entropy, installed the per-VM shared block behind
`lua_Callbacks`, opened the standard libraries, and proved the binder's mirror of Luau's value
layout against this build's API. `Runtime::builder()` makes each of those steps a choice;
[Runtime options](@/docs/runtime-options.md) lists them. Dropping the runtime closes the VM.

**The type.** `Userdata` gives `Vec3` an identity and a script name only. `NAME` is what
`typeof(v)` reports and the root of every debug name the type's members get
(`dreamweave.Vec3.length`, `dreamweave.Vec3.get.x`). The impl is `unsafe` because the type
promises that its `Drop` never touches the Lua API and never panics: Luau runs it while sweeping.
The name has to sit under one of the runtime's debug-name roots, and the default root is
`dreamweave`.

**The registration.** `tagged::register::<Vec3>(&runtime, 10, ..)` gave `Vec3` Luau runtime tag
10 in this VM and recorded `tag 10 -> Vec3` in the runtime's tag plan. Every later check that a
value is a `Vec3` is one tag read and one `TypeId` compare. Inside the closure, `ty` is a
`MetatableBuilder`: `property` registered a getter and `method` a method. The first getter
switched the metatable from a plain methods table to a generated `__index` and `__namecall`, so
`v.x` and `v:length()` run their bound member directly on the dispatcher's stack, one Luau call
frame each. The metatable was frozen and protected (`getmetatable(v)` is `false`) before the
tag was published. [Userdata](@/docs/userdata.md) has the rest of the builder.

**The constructor.** `bind_function` turned a Rust closure into a Lua function. Its three `f32`
parameters are read straight from argument slots 1 to 3, each with one checked conversion, and
counts are validated before any conversion runs: `vec3('x', 0, 0)` fails with
`dreamweave.vec3: bad argument #1 (expected number)`. `Owned(Vec3 {..})` as the return type moves
the value into a new tagged userdata. The closure itself was moved into a Lua-owned userdata and
is dropped by the collector, which is why bound closures are `Fn` and `'static`. The returned
`Function` is a registry pin.

**The global.** `set_global("vec3", &make)` pushed the pinned function and stored it in the
globals table. Anything that implements `convert::Push` can go there: a scalar, a string, a
pinned `Value`, `Table` or `Function`.

**The script.** `exec` compiled the source with the runtime's compile options (optimisation
level 2, debug level 1), loaded the chunk on the main thread, and ran it under `lua_pcall`. A
Lua error comes back as `Err` with the message; the stack is restored. `eval::<R>` does the
same and reads what the chunk returns, `f64` here, a tuple for several results, `()` for none.
`v.length(42)` would have failed with `invalid argument #1 to 'dreamweave.Vec3.length'
(dreamweave.Vec3 expected, got number)`.

## Next

- [Stack and values](@/docs/stack-and-values.md): the layer under `exec`, `eval` and every
  bound function.
- [Userdata](@/docs/userdata.md): when to spend a tag, setters, iterators, metamethods, and
  engine-owned objects.
- [Modules and sandboxes](@/docs/modules-and-sandboxes.md): packaging bindings as a frozen
  module and running scripts in their own environments.
- `examples/openmw_shapes.rs` runs every binding shape the OpenMW engine uses on toy types:
  `cargo run --example openmw_shapes`.
