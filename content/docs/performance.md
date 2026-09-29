+++
title = "Compatibility and performance"
description = "What the version promises, the supported Rust, the pinned dependencies, the license, what is tested, and what each path through the binder costs in nanoseconds, instructions and bytes."
weight = 105

[extra]
kind = "reference"
+++

## Versions

l3i is at 1.0.0: releases in 1.x keep the Rust API compatible, as Cargo's semver rules expect.
The Luau it embeds is release 0.740, the commit OpenMW pins, as the `luau/` submodule; bumping it
is a deliberate change that re-audits the hand-declared C API.

## Rust and dependencies

- **Rust 1.92** or newer, declared as `rust-version` and checked in CI. Edition 2024. The optional `dream-soft-render` dependency declares the same.
- `unsafe` is used, at the FFI boundary and in the value-layout reads; every block states the invariant it relies on (see [Safety](@/docs/safety.md)).
- Three optional features, `jit`, `analysis` and `soft-render`, all off by default (see [Building](@/docs/building.md)).

| Dependency | Version | For |
|---|---|---|
| `dream-net` | `=1.1.0` | The `dream.net` bridge every plan carries |
| `dream-soft-render` | `=1.0.0`, optional (`soft-render`) | The `dream.soft_render` extension |
| `cc` | `1`, `parallel` feature, build only | Compiling the Luau submodule and `csrc/` |
| `criterion` | `0.8.2`, dev only | The benchmarks |

The two DreamWeave crates are pinned exactly: they define what goes on the wire and what a pixel
is, and the byte-identical tests hold against those versions.

## License

l3i is **MIT OR Apache-2.0**, at your option.

Luau is Roblox's, under the MIT license; its notice ships in the package as `luau/LICENSE.txt`
and `luau/lua_LICENSE.txt` and travels with any product that ships l3i.

## What is tested

Every push to `main`, every pull request and every tag runs
[StroggForge](https://github.com/DreamWeave-MP/StroggForge)'s library workflow, whose
`setup-llvm` step gives every job that compiles a clang and lld whose LLVM major matches the
active rustc: the tests on Windows, Linux and macOS; `rustfmt`; Clippy; a check against Rust
1.92 (`msrv: auto`); and a dry run of the crates.io publish. A tag runs the Criterion hot paths
and attaches them to its GitHub release as `BENCHMARKS.md`; publishing to crates.io is off until
the first release.

A daily job on `ubuntu-22.04` runs `cargo fmt --all --check`, `cargo clippy --workspace
--all-targets --all-features -- -W clippy::pedantic -D warnings` (with the LLVM toolchain, since
Clippy runs the build script), and `cargo audit` over a fresh lockfile.

The integration tests link as one binary (`tests/main.rs`), one file's tests run with
`cargo test <file>::`, and `cargo test --all-features` adds the feature-gated ones:

| Suite | Covers |
|---|---|
| `tests/tagged.rs` | One tagged type with a method: instances created from Lua, dropped, collected, exactly one Rust drop per instance |
| `tests/untagged.rs` | Untagged registration, storage, receivers (`testluauregistration.cpp`) |
| `tests/metatable.rs` | Builder conflicts, native methods, dispatch, script-visible identity, metamethod debug names |
| `tests/call.rs` | `testluaucall.cpp`: the call family's success semantics, error parity and stack balance, the result budget, result visitors and argument edges, integer kinds across the call boundary, yields outside a coroutine |
| `tests/iterator.rs` | Stateless, keyed, and cursor iterators |
| `tests/direct.rs` | Direct callbacks shadowing the metamethods for atom-keyed access, the per-instruction cache validated before it is trusted, direct fields bypassing both |
| `tests/module.rs` | The component registration contract and read-only packages |
| `tests/sandbox.rs` | Sandboxes, instances, and templates |
| `tests/watchdog.rs` | Watchdog limits, memory categories, and call scopes |
| `tests/thread.rs` | Coroutines driven from the host, and yields from bound functions |
| `tests/debug.rs` | Activation records, locals and upvalues, breakpoints that stop a host-driven coroutine, single stepping, coverage, lifecycle hooks |
| `tests/memory.rs` | GC controls, dumps, the buffer cage, embedder GC integration, light userdata, finalizers |
| `tests/libraries.rs` | Selective library opening, Luau's own sandbox, registered C libraries, table utilities, metamethod comparison and concatenation, the string builder, chunk environments, compile-time library members, the inliner toggle |
| `tests/require.rs` | Require-by-string over a host navigator: relative paths, caching, proxy requires, registered aliases, cache control |
| `tests/vector_writer.rs` | `vector:writef32x3` on the interpreter path |
| `tests/extension.rs` | The planner's gates: dependency order, composition, direct dispatch, tags and atoms per VM, the stale-cache check, compiler metadata, services, capabilities, state and drop order, every rejected composition, compiler type slots |
| `tests/primitives.rs` | Zero-copy bytes, strict options, sequence and stream views, exact integers and bit patterns, packed scalars |
| `tests/net.rs` | The dream-net bridge over real localhost UDP: a schema from Luau, a host-created server, a script-created client, events both ways, stats, the capability gate |
| `tests/quat.rs` | Packed quaternions against the f32 userdata baseline, kind checks, and (`jit`) the native lowering |
| `tests/raster.rs` | Colors and clip rectangles: construction, kind checks, the byte layout, and (`jit`) the lowered color math |
| `tests/bytes.rs` | `@dream/bytes` (`bytes`): searching, record strings, varints, every width and order against `buffer`, the codecs against Python-made fixtures, the digests against their published vectors, the text codepages, and (`jit`) the lowered integer reads and writes |
| `tests/soft_render.rs` (`soft-render`) | A Luau scene byte-identical to the Rust scene, malformed input, textures freeing themselves, two runtimes with different tags, the vertex writer on all three paths |
| `tests/native_code.rs` (`jit`) | The code generator with the binder's hooks, the `writef32x3` lowering, a userdata field lowering in Rust, modes, module ids, assembly dumps, the perf log |
| `tests/typed_definitions.rs` (`analysis`) | The generated `.d.luau` checked by Luau's frontend, strict scripts against every built-in module through `require`, typed views and forward module references |
| `tests/analysis.rs` (`analysis`) | The type checker, linter, autocomplete, and parser |

## What it costs

Criterion wall-clock means from `cargo bench --bench hot_paths`, generated on 2026-09-28 by
`scripts/gen_benchmarks.py` on an Intel Core i7-10870H at 2.20 GHz, with debug assertions off
and default features (interpreter only), under the verified toolchain. The other suites are
`cargo bench --bench net`, `packed_quat`, `raster`, `soft_render` (feature `soft-render`) and
`instructions`.

### Rust to Luau

`Function::invoke` from the host, per call.

| Variant | Mean | ± Std Dev |
|---|---:|---:|
| scalar (f64, f64) -> f64 | 63.29 ns | 1.95 ns |
| table argument, view result | 69.95 ns | 1.31 ns |
| table argument, pinned result | 107.3 ns | 4.54 ns |

### Luau to Rust

A Lua loop calling the bound function 1000 times; per call is the loop divided by 1000.

| Variant | Mean | ± Std Dev | Per item |
|---|---:|---:|---:|
| hand-written lua_CFunction | 35.73 µs | 1.04 µs | 35.73 ns |
| captured Rust context | 39.97 µs | 1.57 µs | 39.97 ns |
| typed binder (f64, f64) -> f64 | 44.86 µs | 1.94 µs | 44.86 ns |
| Vector3 ingress | 90.73 µs | 1.36 µs | 90.73 ns |

The typed binder is within ten nanoseconds of a bare C function.

### Typed binder variants

The same loop over the binder's argument and result shapes.

| Variant | Mean | ± Std Dev |
|---|---:|---:|
| (f64) -> () | 37.11 µs | 247.7 ns |
| (ValueView) -> f64 | 41.26 µs | 429.0 ns |
| () -> f64 | 43.59 µs | 6.45 µs |
| (f64) -> f64 | 46.18 µs | 2.39 µs |
| (&Call) -> f64 | 47.27 µs | 8.07 µs |
| (f64, f64) -> f64 | 49.92 µs | 1.54 µs |
| (i32, i32) -> i32 | 58.65 µs | 847.4 ns |

### Method calls

`obj:get()` 1000 times.

| Variant | Mean | ± Std Dev | Per item |
|---|---:|---:|---:|
| tagged direct namecall | 32.25 µs | 461.6 ns | 32.25 ns |
| tagged generated __namecall | 52.36 µs | 707.5 ns | 52.36 ns |
| untagged generated __namecall | 72.15 µs | 4.37 µs | 72.15 ns |

The direct path skips the metamethod dispatch; the generated dispatcher costs one Luau call
frame, not two.

### Property reads

`obj.value` 1000 times.

| Variant | Mean | ± Std Dev | Per item |
|---|---:|---:|---:|
| plain table field | 8.58 µs | 419.6 ns | 8.58 ns |
| tagged direct field | 12.48 µs | 170.1 ns | 12.48 ns |
| tagged direct index | 32.56 µs | 418.2 ns | 32.56 ns |
| tagged generated __index | 58.89 µs | 1.96 µs | 58.89 ns |
| untagged generated __index | 73.52 µs | 1.56 µs | 73.52 ns |

A direct field is within four nanoseconds of a table field.

### Plan dispatch

Runtime-resolved `DirectPlan` dispatch with the cache hit path, 1000 accesses of the last member.

| Variant | Mean | ± Std Dev | Per item |
|---|---:|---:|---:|
| cached direct index, 4 members | 40.83 µs | 909.1 ns | 40.83 ns |
| cached direct index, 32 members | 42.48 µs | 2.71 µs | 42.48 ns |
| cached direct namecall, 4 members | 43.35 µs | 1.32 µs | 43.35 ns |
| cached direct namecall, 32 members | 43.42 µs | 431.1 ns | 43.42 ns |
| cached direct index, 128 members | 45.45 µs | 3.59 µs | 45.45 ns |
| cached direct namecall, 128 members | 48.56 µs | 487.3 ns | 48.56 ns |

The member count barely moves the cost: the plan validates Luau's per-instruction cache in O(1).

### Extension dispatch

The same operations on a type declared through the planner.

| Variant | Mean | ± Std Dev |
|---|---:|---:|
| tagged planned direct field | 13.67 µs | 568.0 ns |
| tagged planned direct index | 50.09 µs | 709.2 ns |
| tagged planned cold namecall | 53.51 µs | 453.5 ns |
| tagged planned direct namecall | 54.52 µs | 671.5 ns |
| untagged planned namecall | 69.78 µs | 425.4 ns |
| untagged planned index | 73.23 µs | 1.54 µs |

### Iterators

A generic `for` over a 100-element array iterator, 1000 loops.

| Variant | Mean | ± Std Dev | Per item |
|---|---:|---:|---:|
| array __iter, 100 elements | 4.44 ms | 43.40 µs | 44.38 ns |

### Host side

Host-side operations, per call.

| Variant | Mean | ± Std Dev |
|---|---:|---:|
| tagged receiver check | 19.63 ns | 1.97 ns |
| untagged receiver check | 36.58 ns | 1.66 ns |
| value pin create and drop | 57.87 ns | 20.43 ns |
| owned table field read | 67.74 ns | 2.15 ns |
| borrowed table field read | 71.16 ns | 1.59 ns |

A tagged check is one tag read and one `TypeId` compare; an untagged one compares a per-VM cached
metatable identity.

### The network bridge

Both ends of the `dream.net` bridge driven from Luau.

| Variant | Mean | ± Std Dev |
|---|---:|---:|
| idle update + empty pollInto, both ends | 5.13 µs | 96.54 ns |
| client sendEvent + flush | 58.72 µs | 800.1 ns |
| client send, server update + pollInto | 220.0 µs | 3.26 µs |

### Packed quaternions through Luau

The `dream.quat` module against an f32 quaternion userdata, 1000 calls.

| Variant | Mean | ± Std Dev |
|---|---:|---:|
| userdata rotate vector | 56.72 µs | 2.33 µs |
| packed rotate vector | 76.70 µs | 1.74 µs |
| userdata mul (allocates) | 129.9 µs | 2.21 µs |
| packed mul | 137.9 µs | 10.68 µs |
| userdata slerp (allocates) | 180.3 µs | 1.75 µs |
| packed slerp | 194.0 µs | 7.69 µs |

On the interpreter path the packed form costs the encode and decode and saves the allocation;
the native lowering on [Built-in extensions](@/docs/builtin-extensions.md) is where it pulls ahead.

### Packed quaternions in Rust

The encoding itself, 1000 operations.

| Variant | Mean | ± Std Dev |
|---|---:|---:|
| f64 mul (reference) | 4.19 µs | 368.3 ns |
| decode smallest-three | 8.99 µs | 503.4 ns |
| encode smallest-three | 23.18 µs | 783.3 ns |
| decode, mul, encode | 70.08 µs | 3.12 µs |

The encode's floor is the three float-to-integer conversions.

### Bytes

`cargo bench --bench bytes`, with every `bytes-*` feature and `jit`. Per call from a Luau loop:
`buffer.readu32` plus `bit32.byteswap` 116 ns interpreted and 4.3 ns native; the module's
`readu32be` 65 ns; the receiver's `readu32be` 68 ns interpreted and 5.9 ns lowered, `readi64be`
5.1 ns, `readu24be` 6.5 ns, `writeu32be` 5.3 ns; `readCString` of a 16-byte field 157 ns,
`readVarint` 64 ns. Over one megabyte: `find` 23 GiB/s, `equals` 28 GiB/s, `crc32` 23 GiB/s,
`xxh3` 17 GiB/s, `blake3` 3.7 GiB/s, `sha256` 238 MiB/s, `inflate` 1.6 GiB/s of output,
`deflate` level 1 2.8 GiB/s, LZ4 block decompress 1.1 GiB/s and compress 5.9 GiB/s, `decode`
Windows-1252 1.5 GiB/s.

## Instructions and cycles

Past a few nanoseconds wall time is noise, so the binder's own cost is tracked in retired
instructions and cycles per call, read from the CPU's counters by `cargo bench --bench
instructions` (Linux, `perf_event_paranoid` at 2 or lower, no `perf` binary needed). The loop is
subtracted, the minimum over seven rounds is reported, and the floors are measured the same way:
a hand-written `lua_CFunction` and a typed direct handler. On the pinned Luau 0.740:

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
| planned sequence `[i]` | 547 | 148 |
| planned sequence `#` | 563 | 148 |

A bound call is within about forty instructions of a bare C function; a planned method is within
sixty of the typed direct handler. The rest is Luau's own call and return machinery. A packed
argument pays eleven instructions for the kind registry check (one owner load and a type
compare) on top of its bit decode.

The same harness reads the L1 data, L1 instruction, last-level cache, data and instruction TLB,
and branch-miss counters. Every scenario reports fewer than 0.005 of each per call: the whole
path, Luau's and the binder's, stays in L1 and predicts. The binder's side of a call touches four
lines of its own, the slot row (rows are 32 bytes, so a row never straddles a line), the member's
context, the thread record, and the shared block's first line, which holds the slot table, the
plan, and the profiler switch (`#[repr(C, align(64))]`).

## Memory per runtime

After a full collection, a bare VM's heap is 64 KiB, with `dream.quat` and `dream.raster`
84 KiB, with `dream.soft_render` as well 96 KiB. The binder's own state is a 600-byte shared
block, 32 bytes per plan slot, 64 bytes per plan entry, and the dense `(tag, kind, atom)` table,
sized by the tags and atoms the plan uses (1.4 KB for five tags and 39 atoms).

## The toolchain campaign

The rule on [Building](@/docs/building.md) comes from this measurement, taken on 2026-09-27 with
`scripts/toolchain_campaign.py` (a clean `cargo build --release --lib`, then
`cargo bench --bench hot_paths --no-run`, then a five-case subset at 2 s per case) on an
i7-10870H in a Fedora 44 toolbox: gcc 16.2.1, clang/lld 22.1.8, mold 2.40.4, rustc 1.98.1
(LLVM 22). Each contender ran three times, interleaved. Nanoseconds per call.

| Variant | Clean release build | Bench build | typed binder `(f64, f64) -> f64` | tagged generated `__namecall` | tagged direct index | Rust to Luau scalar | raw `lua_CFunction` |
|---|---:|---:|---:|---:|---:|---:|---:|
| gcc, serial `cc` | 68.1 s | 13.8 s | | | | | |
| gcc, parallel `cc` (3 runs) | 37.3 to 40.7 s | 13.7 s | 53.7 to 54.7 | 95.7 to 101.2 | 35.2 to 35.4 | 76.0 to 82.8 | 35.9 to 39.3 |
| gcc plus mold | 38.0 s | 13.7 s | | | | | |
| clang, no LTO | 21.1 s | 13.5 s | 58.7 | 113.6 | 37.8 | 82.2 | 39.4 |
| **clang plus cross-language thin LTO** (3 runs) | **16.3 to 17.8 s** | 18.7 to 20.3 s | **45.3 to 50.2** | **83.0 to 89.3** | **29.2 to 31.6** | **64.8 to 72.8** | 34.1 to 40.3 |
| gcc plus Rust fat LTO, 1 codegen unit | 41.3 s | 69.2 s | 52.9 | 99.2 | 27.6 | 78.6 | 40.3 |
| clang LTO plus Rust fat LTO | 15.9 s | 54.3 s | 51.9 | 92.8 | 27.4 | 82.9 | 38.6 |

- Cross-language LTO is the whole story. The hand-written `lua_CFunction` does not move between compilers, so Luau's interpreter is equally fast under gcc and clang. Every binder path gains 8 to 15 percent because the Rust thunk and the Luau API calls it makes (`lua_tonumber`, `lua_pushnumber`, the tagged userdata read) inline into one another.
- Plain clang is slower than gcc at runtime on every binder case. The compiler switch is justified only together with the LTO half, hence the rule enforces both.
- Rust-only fat LTO buys nothing on the binder paths (the direct-index case improves, the Vector3 case regresses) and quadruples the bench build. Stacked on cross-language LTO it is neutral at best. Not adopted.
- `cc` parallel halves the clean build with no runtime effect. Adopted.
- mold is a no-op here: the link is a small fraction of a build dominated by compiling Luau. The self-contained lld rustc ships is not used with a custom `-Clinker`, so system lld is the linker.
- The verified configuration is also the fastest clean build, 16 to 18 s, four times faster than the gcc serial baseline.

`BENCHMARKS.md` is regenerated under this configuration.
