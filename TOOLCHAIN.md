# Toolchain

l3i builds only with **clang, cross-language thin LTO, and lld**. `build.rs` refuses anything
else; `L3I_UNVERIFIED_TOOLCHAIN=1` turns the refusal into a warning. This page records why.

## The rule

| Side | Requirement | Where it is set |
|---|---|---|
| C++ (Luau, `csrc/`) | `clang++`; `build.rs` adds `-flto=thin` itself | `CXX=clang++` (`.cargo/config.toml` `[env]`) |
| Rust | `-Clinker-plugin-lto -Clinker=clang -Clink-arg=-fuse-ld=lld`, all three checked by `build.rs` | `.cargo/config.toml` `[target.*] rustflags` |
| Both | clang and rustc on the same LLVM major (`clang++ --version`, `rustc -vV`) | checked by `build.rs` |
| `cc` crate | `parallel` feature | `Cargo.toml` |

A crate that depends on l3i copies the `[env]` and `rustflags` lines into its own
`.cargo/config.toml`: Cargo does not inherit a dependency's config. Fedora: `dnf install clang
lld`; Debian/Ubuntu: `apt install clang-<N> lld-<N>` where `<N>` is rustc's LLVM major. Apple's
clang is not upstream LLVM; use a Homebrew or nightly LLVM whose major matches.

The lld flag is not optional: without it clang hands the bitcode objects to `ld.bfd`, which
fails with `bad -plugin-opt option`. `-Clinker-plugin-lto` without a clang-built Luau links fine
but forfeits the gain. `build.rs` checks every half of the chain (clang++ as the C++ compiler,
`-Clinker-plugin-lto`, a clang linker, `-fuse-ld=lld`, matching LLVM majors) and only then emits
`-flto=thin`, so a partial configuration is refused up front with the missing piece named
instead of failing at the final link or building silently slow.

## Why: the 2026-09-27 campaign

Measured with `scripts/toolchain_campaign.py` (clean `cargo build --release --lib`, then
`cargo bench --bench hot_paths --no-run`, then a five-case subset at 2 s per case) on an
i7-10870H in a Fedora 44 toolbox: gcc 16.2.1, clang/lld 22.1.8, mold 2.40.4, rustc 1.98.1
(LLVM 22). Each contender was run three times, interleaved. Nanoseconds per call.

| Variant | Clean release build | Bench build | typed binder `(f64, f64) -> f64` | tagged generated `__namecall` | tagged direct index | Rust to Luau scalar | raw `lua_CFunction` |
|---|---:|---:|---:|---:|---:|---:|---:|
| gcc, serial `cc` | 68.1 s | 13.8 s | | | | | |
| gcc, parallel `cc` (3 runs) | 37.3 to 40.7 s | 13.7 s | 53.7 to 54.7 | 95.7 to 101.2 | 35.2 to 35.4 | 76.0 to 82.8 | 35.9 to 39.3 |
| gcc plus mold | 38.0 s | 13.7 s | | | | | |
| clang, no LTO | 21.1 s | 13.5 s | 58.7 | 113.6 | 37.8 | 82.2 | 39.4 |
| **clang plus cross-language thin LTO** (3 runs) | **16.3 to 17.8 s** | 18.7 to 20.3 s | **45.3 to 50.2** | **83.0 to 89.3** | **29.2 to 31.6** | **64.8 to 72.8** | 34.1 to 40.3 |
| gcc plus Rust fat LTO, 1 codegen unit | 41.3 s | 69.2 s | 52.9 | 99.2 | 27.6 | 78.6 | 40.3 |
| clang LTO plus Rust fat LTO | 15.9 s | 54.3 s | 51.9 | 92.8 | 27.4 | 82.9 | 38.6 |

Readings:

- **Cross-language LTO is the whole story.** The hand-written `lua_CFunction` does not move
  between compilers, so Luau's interpreter is equally fast under gcc and clang. Every binder
  path gains 8 to 15 percent because the Rust thunk and the Luau API calls it makes
  (`lua_tonumber`, `lua_pushnumber`, the tagged userdata read) inline into one another.
- **Plain clang is slower than gcc at runtime** on every binder case. The compiler switch is
  justified only together with the LTO half, hence the rule enforces both.
- **Rust-only fat LTO** buys nothing on the binder paths (the direct-index case improves, the
  Vector3 case regresses) and quadruples the bench build. Stacked on cross-language LTO it is
  neutral at best. Not adopted.
- **`cc` parallel** halves the clean build with no runtime effect. Adopted.
- **mold** is a no-op here: the link is a small fraction of a build dominated by compiling
  Luau. The self-contained lld rustc ships is not used with a custom `-Clinker`, so system lld
  is the linker.
- **Compile time**: the verified configuration is also the fastest clean build (16 to 18 s),
  four times faster than the gcc serial baseline.

`BENCHMARKS.md` is regenerated under this configuration.
