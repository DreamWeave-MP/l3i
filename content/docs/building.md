+++
title = "Building"
description = "The Luau submodule, what build.rs compiles and defines, the features, the clang and cross-language LTO toolchain rule with its Apple exception, and what a dependent crate copies into its own Cargo configuration."
weight = 100

[extra]
kind = "guide"
+++

l3i owns its Luau build. Luau is the `luau/` git submodule, pinned at release 0.740, the commit
OpenMW pins; `build.rs` compiles it and the binder's C++ additions with `cc`, in parallel, from
the checkout alone. There is no network access at build time and no external Lua crate. Rust 1.92
or newer, edition 2024.

```sh
git clone --recurse-submodules https://github.com/DreamWeave-MP/l3i
# or, in an existing checkout
git submodule update --init
cargo build
```

`build.rs` fails early with that `git submodule` line when `luau/VM/include/lua.h` is missing.

## What build.rs compiles

Every Luau component the crate uses is one `cc` library: Common, Ast, Bytecode, Inliner,
Compiler, Config, Require and VM always; CodeGen under `jit`; Analysis under `analysis`. The
binder's own C additions are `csrc/extra.cpp`; the shims over the C++-only APIs are
`csrc/codegen.cpp` (`jit`) and `csrc/analysis.cpp` (`analysis`).

| Define or flag | Value | Why |
|---|---|---|
| `LUAI_MAXCSTACK` | `8000` | Luau's default, OpenMW's too; `LUA_REGISTRYINDEX` in the FFI derives from it |
| `LUA_VECTOR_SIZE` | `3` | Three-component vectors, as OpenMW |
| `LUA_UTAG_LIMIT` | `254` | Every tag Luau can address; `l3i::TAG_LIMIT` mirrors it, and `LUAU_CXXFLAGS` cannot lower it |
| `LUA_API`, `LUALIB_API`, and the per-library `*_API` | `extern "C"` | The C ABI Rust declares by hand in `src/raw/ffi.rs` |
| `LUAU_ENABLE_ASSERT` | Set when Rust's debug assertions are on | Luau's internal `api_check` assertions in debug builds; a release build with debuginfo keeps the release VM |
| `-fno-math-errno` | Release only | Lets the compiler lower `sqrt` to one instruction |
| `-std=c++17`, Cargo's optimisation level | `-O3` in release, no `-march` | The binary stays portable |
| `-fexceptions -fwasm-exceptions` | emscripten | `cc` adds `-fno-exceptions` for wasm32; Luau needs exceptions in the ABI Rust uses |
| `/EHs` | MSVC targets | Luau raises errors as C++ exceptions out of its `extern "C"` API too |
| `-flto=thin` | The verified toolchain, except Apple | Bitcode objects, so the linker's LTO sees Luau and the Rust thunks as one module |

Hosts may append compiler flags through `LUAU_CXXFLAGS`. Under `jit`, `build.rs` also parses
`Luau/IrData.h` and `Luau/CodeGenOptions.h` and writes `ir_enums.rs` into `OUT_DIR`, the mirrors
of `IrCmd`, `IrCondition`, `IrBlockKind` and `HostMetamethod` for that exact Luau build. It exports
`LUAU_VERSION` (`0.740`) and `L3I_TAG_LIMIT` to the crate.

Compiled chunks default to optimisation level 2 and debug level 1, and runtime plans turn type
information on so native code sees the userdata types.

### The value-layout mirror

The binder reads argument slots straight from Luau's 16-byte `TValue` layout. `csrc/extra.cpp`
pins every offset at compile time:

```text
static_assert(sizeof(TValue) == 16, "l3i mirrors a 16-byte TValue");
static_assert(offsetof(TValue, value) == 0, "l3i mirrors the value union at offset 0");
static_assert(offsetof(TValue, extra) == 8, "l3i mirrors extra at offset 8");
static_assert(offsetof(TValue, tt) == 12, "l3i mirrors the tag at offset 12");
static_assert(LUA_VECTOR_SIZE == 3, "l3i mirrors three-component vectors");
static_assert(offsetof(Udata, tag) == 3, "l3i mirrors the userdata tag at offset 3");
static_assert(offsetof(Udata, data) == 16, "l3i mirrors the userdata payload at offset 16");
```

Each runtime proves the mirror against the API once at creation, so a Luau bump that moves a
byte fails at once rather than misreading slots. Bumping the submodule means bumping
`LUAU_VERSION` in `build.rs`, re-auditing `src/raw/ffi.rs` against the new headers, and
re-checking the flag policy.

## Features

```toml
[dependencies]
l3i = { version = "1.0", features = ["jit"] }
```

| Feature | Default | Adds |
|---|---|---|
| `jit` | off | Luau's CodeGen library and `csrc/codegen.cpp`: `l3i::native_code`, the lowering hooks, `quat.math()`, and the lowered paths of `raster.math()`, `soft.vertices()`, `bytes.math()` and an annotated intern `Pool`'s `intern` and `find`. Not supported on emscripten |
| `analysis` | off | Luau's Analysis library and `csrc/analysis.cpp`: `l3i::analysis` (type checker, linter, autocomplete, parser) and `RuntimePlan::check_definitions` |
| `soft-render` | off | The `dream-soft-render` dependency as the `dream.soft_render` extension |
| `bytes` | off | The `dream.bytes` extension (`l3i::bytes`) and `memchr` for its searches |
| `bytes-codecs` | off | `miniz_oxide`, `lz4_flex`, `ruzstd`, `lzma-rs` and `crc32fast`: `inflate`, `deflate`, LZ4, zstd, LZMA/XZ |
| `bytes-digests` | off | `crc32fast`, `adler2`, `xxhash-rust`, `md-5`, `sha1`, `sha2`, `blake3`: the checksums and digests |
| `bytes-text` | off | `encoding_rs`: text decoding and encoding for every WHATWG label |
| `bytes-regex` | off | `regex`: `bytes.regex`, regular expressions over bytes, many ranges of a buffer in one call |
| `intern` | off | The `dream.intern` extension (`l3i::intern`); no dependencies |
| `syntax` | off | The `dream.luau` extension (`l3i::syntax`) and `csrc/syntax.cpp`, which builds Luau's parse tree as tables; Luau's Ast library is in every build, so no dependencies |

Networking is not a feature: `dream-net` is a normal dependency and the `dream.net` bridge is in
every plan.

## The toolchain rule

l3i builds only with clang, cross-language thin LTO, and lld. `build.rs` refuses anything else,
naming the missing piece, so a partial configuration fails up front instead of at the final link
or by building silently slow.

| Side | Requirement | Where it is set |
|---|---|---|
| C++ (Luau, `csrc/`) | `clang++`; `build.rs` adds `-flto=thin` itself | `CXX=clang++` (`.cargo/config.toml` `[env]`) |
| Rust | `-Clinker-plugin-lto -Clinker=clang -Clink-arg=-fuse-ld=lld`, all three checked by `build.rs` | `.cargo/config.toml` `[target.*] rustflags` |
| Both | clang and rustc on the same LLVM major (`clang++ --version`, `rustc -vV`) | checked by `build.rs` |
| MSVC targets | `clang-cl` compiles (`CXX_x86_64-pc-windows-msvc`), rustc links with `-Clinker=lld-link` | `.cargo/config.toml` |
| `cc` crate | `parallel` feature | `Cargo.toml` |

The lld flag is not optional: without it clang hands the bitcode objects to `ld.bfd`, which fails
with `bad -plugin-opt option`. `-Clinker-plugin-lto` without a clang-built Luau links fine but
forfeits the gain, and plain clang without the LTO half is slower than gcc at runtime, so the rule
enforces both. Linux is the measured configuration; the Windows entries follow the same shape and
are what CI runs.

Apple targets are the vetted exception: rustc passes `-Clinker-plugin-lto`'s GNU `-plugin-opt`
arguments to the linker and `ld64.lld` rejects them (found in dream-ini's CI). There `clang` still
drives `ld64.lld` and rustc's own release thin LTO covers the Rust side, `build.rs` builds Luau as
plain objects rather than bitcode, and the cross-language inlining is forfeited; macOS pays a few
percent on the binder's hot paths, which is the price of keeping the platform.

Two exemptions:

- `L3I_UNVERIFIED_TOOLCHAIN=1` turns the refusal into a warning, for hosts that cannot meet the requirement; expect slower binder hot paths.
- docs.rs, which sets `DOCS_RS`, is exempt automatically: it only renders documentation and cannot be handed a linker configuration.

`l3i::VERIFIED_TOOLCHAIN` says which kind of build a binary is: `true` exactly when `build.rs`
found nothing missing, whether or not `L3I_UNVERIFIED_TOOLCHAIN` was set. A host that publishes
performance numbers checks it, since an exempted build is slower by no fixed amount.

Installing the tools: Fedora `dnf install clang lld`; Debian and Ubuntu
`apt install clang-<N> lld-<N>` where `<N>` is rustc's LLVM major. Apple's clang is not upstream
LLVM; use a Homebrew or nightly LLVM whose major matches. [TOOLCHAIN.md](https://github.com/DreamWeave-MP/l3i/blob/main/TOOLCHAIN.md)
records the measurements behind the rule; the campaign table is on
[Compatibility and performance](@/docs/performance.md).

## What a dependent crate copies

Cargo does not inherit a dependency's configuration, so a crate that depends on l3i copies the
`[env]` and `rustflags` lines into its own `.cargo/config.toml`. This repository's file, in full:

```toml
# The verified toolchain (TOOLCHAIN.md): clang compiles Luau, the Rust side emits bitcode, and
# lld runs one thin LTO over both languages. build.rs refuses anything else unless
# L3I_UNVERIFIED_TOOLCHAIN is set (docs.rs is exempt). Environment variables override every
# entry here. Linux is the measured configuration; the Windows entries follow the same shape
# (clang-cl plus lld-link) and are what CI runs. Apple targets are the vetted exception below.
[env]
CXX = "clang++"
CXX_x86_64-pc-windows-msvc = "clang-cl"
CXX_aarch64-pc-windows-msvc = "clang-cl"

[target.x86_64-unknown-linux-gnu]
rustflags = ["-Clinker-plugin-lto", "-Clinker=clang", "-Clink-arg=-fuse-ld=lld"]

[target.aarch64-unknown-linux-gnu]
rustflags = ["-Clinker-plugin-lto", "-Clinker=clang", "-Clink-arg=-fuse-ld=lld"]

# No -Clinker-plugin-lto on Apple targets: rustc passes that flag's GNU `-plugin-opt` arguments
# to the linker, and ld64.lld rejects them (vetted in dream-ini's CI). clang and lld still link,
# the release profile's thin LTO runs in rustc instead, and the cross-language inlining the
# binder's hot paths gain from is forfeited there; macOS pays a few percent, portability does not.
[target.x86_64-apple-darwin]
rustflags = ["-Clinker=clang", "-Clink-arg=-fuse-ld=lld"]

[target.aarch64-apple-darwin]
rustflags = ["-Clinker=clang", "-Clink-arg=-fuse-ld=lld"]

[target.x86_64-pc-windows-msvc]
rustflags = ["-Clinker-plugin-lto", "-Clinker=lld-link"]

[target.aarch64-pc-windows-msvc]
rustflags = ["-Clinker-plugin-lto", "-Clinker=lld-link"]
```

Environment variables override every entry, so `CXX` and `RUSTFLAGS` set in a shell take
precedence over the file.

## Checking a build

`cargo test` and `cargo test --all-features` run the ported C++ test contracts plus the runtime,
sandbox, watchdog, native code, analysis and renderer suites. The integration tests under
`tests/` link as one binary, because each binary pays a full link-time codegen under
cross-language LTO, so one file's tests run with `cargo test <file>::`. The clean configuration is
`cargo clippy --all-targets --all-features -- -W clippy::pedantic -D warnings` and
`cargo fmt --check`.

{% callout(kind="note", title="Clippy runs the build script") %}
`cargo clippy` compiles Luau, so it needs the same clang and lld as a build; CI's daily quality
job sets them up before running it.
{% end %}
