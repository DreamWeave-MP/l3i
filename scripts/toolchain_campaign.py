#!/usr/bin/env python3
"""
Toolchain campaign: measure clean compile times and hot-path runtime for one toolchain
configuration, writing a JSON record.

    python3 scripts/toolchain_campaign.py <label> [--build-only] [KEY=VALUE ...]

KEY=VALUE pairs are environment variables for cargo (CXX, CXXFLAGS, RUSTFLAGS,
CARGO_PROFILE_BENCH_LTO, ...). Steps:
  1. `cargo clean`, then the release library build (Luau + crate) -> build_release_s
  2. bench build on top of it (--no-run)                          -> build_bench_s
  3. unless --build-only: a five-case hot-path subset, ns/call    -> bench_ns{}
--build-only is for variants that cannot change generated code (linker choice, cc
parallelism). Writes /tmp/claude-1000/campaign/<label>.json (or $CAMPAIGN_DIR).
"""

import json
import os
import re
import shutil
import subprocess
import sys
import time
from pathlib import Path

BENCH_FILTER = (
    "luau_to_rust_call/typed binder|luau_to_rust_call/hand-written|luau_to_rust_call/Vector3"
    "|method_call/tagged generated|property_get/tagged direct index|rust_to_luau_call/scalar"
)
PER_ITEM = {"luau_to_rust_call": 1000, "method_call": 1000, "property_get": 1000, "plan_dispatch": 1000}


def run(cmd, env, timeout=3600):
    start = time.perf_counter()
    result = subprocess.run(cmd, env=env, capture_output=True, text=True, timeout=timeout)
    elapsed = time.perf_counter() - start
    if result.returncode != 0:
        sys.stderr.write(result.stderr[-4000:])
        raise SystemExit(f"failed: {' '.join(cmd)}")
    return elapsed


def clean(env):
    subprocess.run(["cargo", "clean"], env=env, check=True, capture_output=True)


def bench_means():
    means = {}
    for estimates in Path("target/criterion").rglob("new/estimates.json"):
        meta_path = estimates.parent / "benchmark.json"
        if not meta_path.exists():
            continue
        meta = json.loads(meta_path.read_text())
        est = json.loads(estimates.read_text())
        group = meta["group_id"]
        name = f"{group}/{meta.get('function_id') or meta.get('value_str') or ''}"
        if not re.search(BENCH_FILTER, name):
            continue
        means[name] = round(est["mean"]["point_estimate"] / PER_ITEM.get(group, 1), 2)
    return dict(sorted(means.items()))


def main():
    label = sys.argv[1]
    build_only = "--build-only" in sys.argv[2:]
    env = dict(os.environ)
    for pair in sys.argv[2:]:
        if pair.startswith("--"):
            continue
        key, value = pair.split("=", 1)
        env[key] = value
    record = {"label": label, "env": {k: env[k] for k in ("CXX", "CXXFLAGS", "RUSTFLAGS", "CARGO_PROFILE_BENCH_LTO", "CARGO_PROFILE_BENCH_CODEGEN_UNITS", "CARGO_BUILD_JOBS") if k in env}}
    clean(env)
    record["build_release_s"] = round(run(["cargo", "build", "--release", "--lib"], env), 1)
    record["build_bench_s"] = round(run(["cargo", "bench", "--bench", "hot_paths", "--no-run"], env), 1)
    if not build_only:
        if Path("target/criterion").exists():
            shutil.rmtree("target/criterion")
        run(["cargo", "bench", "--bench", "hot_paths", "--", BENCH_FILTER, "--measurement-time", "2", "--warm-up-time", "0.5"], env)
        record["bench_ns"] = bench_means()
    out = Path(os.environ.get("CAMPAIGN_DIR", "/tmp/claude-1000/campaign")) / f"{label}.json"
    out.parent.mkdir(parents=True, exist_ok=True)
    out.write_text(json.dumps(record, indent=2))
    print(json.dumps(record, indent=2))


if __name__ == "__main__":
    main()
