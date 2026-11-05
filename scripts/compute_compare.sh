#!/bin/sh
# Runs compute's benches with their competitors compiled in, from the repository root:
#   scripts/compute_compare.sh compute_matrix
#   scripts/compute_compare.sh compute_cubecl ours|cubecl|diagnose|ceiling|model
#   L3I_BENCH_BLAS=openblas=/usr/lib64/libopenblaso.so.0,blis=/path/libblis.so.4 \
#     scripts/compute_compare.sh compute_matrix
#
# CubeCL, matrixmultiply and rayon are dev-dependencies under `--cfg l3i_compare`, which is not a
# Cargo feature so that `--all-features` cannot reach them: CubeCL's first build downloads a
# prebuilt LLVM and compiles a graphics stack. BLAS libraries load at run time, by name, so one binary
# compares any number of them.
# Builds live in target/compare. `--no-run` builds without running.
set -eu
flags="'--cfg','l3i_compare'"
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-target/compare}"
export OMP_NUM_THREADS="${L3I_BENCH_THREADS:-1}" OPENBLAS_NUM_THREADS="${L3I_BENCH_THREADS:-1}"
cargo_args=""
if [ "${1:-}" = "--no-run" ]; then
    cargo_args="--no-run"
    shift
fi
# shellcheck disable=SC2086 # cargo_args is empty or one flag.
exec cargo --config "target.'cfg(all())'.rustflags=[$flags]" \
    bench --features compute-testing --bench main $cargo_args -- "$@"
