#!/usr/bin/env bash
# Counts retired instructions for both sides inside a disposable container: built on the host,
# mounted read-only, no network, removed on exit. From the repository root:
#   scripts/compute_cubecl_container.sh <bench-binary> <report-directory> [runs]
# Build the binary first: scripts/compute_compare.sh --no-run
# perf_event_open needs two overrides under rootless podman on an SELinux host: the default
# seccomp profile blocks the syscall, and the container label denies the perf_event class.
set -eu
binary=$(realpath "$1")
reports=$(realpath "$2")
runs=${3:-3}
image=registry.fedoraproject.org/fedora:44
run() {
    podman run --rm --network=none --security-opt label=disable --security-opt seccomp=unconfined \
        -v "$binary:/compare:ro" "$image" /compare compute_cubecl "$1"
}
for pass in $(seq 1 "$runs"); do
    for side in ours cubecl; do
        run "$side" > "$reports/compare-$side-$pass.txt"
    done
done
run diagnose > "$reports/compare-diagnosis.txt"
for pass in $(seq 1 "$runs"); do
    run ceiling > "$reports/ceiling-$pass.txt"
    run model > "$reports/model-$pass.txt"
done
