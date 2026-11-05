#!/bin/sh
# Runs cargo without link-time optimization, for the edit-compile-test loop: a one-file change
# relinks in about a second instead of about four. `scripts/quick.sh test --features data data::`
#
# This is not the verified toolchain: nothing measured here means anything, and a passing run says
# nothing about what LLVM does to an optimized loop. Its builds live in target/quick, so switching
# back and forth rebuilds neither side. Before a commit, run the plain cargo commands.
set -eu
export RUSTFLAGS="-Clinker=clang -Clink-arg=-fuse-ld=lld"
export L3I_UNVERIFIED_TOOLCHAIN=1
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-target/quick}"
exec cargo "$@"
