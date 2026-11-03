#!/usr/bin/env bash
# Run from the repository root with a clang/lld toolchain matching rustc's LLVM major.
set -eu
rustc -vV
clang --version
cargo fmt --all --check
cargo fmt --manifest-path research/cubecl-cpu-010/Cargo.toml --check
cargo fmt --manifest-path research/cubecl-cpu/Cargo.toml --check
cargo test --all-features
cargo clippy --workspace --all-targets --all-features -- -W clippy::pedantic -D warnings
cargo test --locked --manifest-path research/cubecl-cpu-010/Cargo.toml --bin dynamic
cargo clippy --locked --manifest-path research/cubecl-cpu-010/Cargo.toml --all-targets -- -W clippy::pedantic -D warnings
cargo run --locked --manifest-path research/cubecl-cpu-010/Cargo.toml --bin persistent
cargo run --locked --manifest-path research/cubecl-cpu/Cargo.toml
cargo clippy --locked --manifest-path research/cubecl-cpu/Cargo.toml --all-targets -- -W clippy::pedantic -D warnings
