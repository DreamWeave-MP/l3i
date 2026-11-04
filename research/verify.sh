#!/usr/bin/env bash
# Run from the repository root with a clang/lld toolchain matching rustc's LLVM major.
set -eu
G=research/graph-cpu/Cargo.toml
rustc -vV
clang --version
cargo fmt --all --check
cargo test --all-features
cargo clippy --workspace --all-targets --all-features -- -W clippy::pedantic -D warnings
cargo check --all-features
cargo package --allow-dirty --no-verify
# The research crate is its own workspace. Test optimized numerical behaviour as well.
cargo fmt --manifest-path "$G" --check
cargo test --locked --manifest-path "$G"
cargo test --locked --release --manifest-path "$G"
cargo clippy --locked --manifest-path "$G" --all-targets -- -W clippy::pedantic -D warnings
# The generated Jess shader, when a jess executable is available.
cargo run --locked --release --manifest-path "$G" -- --emit-shader research/graph-cpu/generated-shader.jsl
git diff --exit-code research/graph-cpu/generated-shader.jsl
if command -v jess >/dev/null; then jess run research/graph-cpu/generated-shader.jsl; fi
