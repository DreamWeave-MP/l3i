#!/usr/bin/env python3
"""
Generate BENCHMARKS.md from Criterion output.

Run `cargo bench --bench hot_paths` first, then `python3 scripts/gen_benchmarks.py`.
Reads target/criterion/**/new/{benchmark,estimates}.json; writes BENCHMARKS.md.

Groups whose benchmark runs a Lua loop of 1000 calls per sample (throughput = 1000 elements)
also get a per-call column: the loop time divided by 1000.
"""

import json
import math
import platform
from collections import defaultdict
from datetime import date
from pathlib import Path

CRITERION_DIR = Path("target/criterion")
OUTPUT = Path("BENCHMARKS.md")
GROUP_ORDER = ["rust_to_luau_call", "luau_to_rust_call", "method_call", "property_get", "iterator", "host_side"]
GROUP_NOTES = {
    "rust_to_luau_call": "`Function::invoke` from the host, per call.",
    "luau_to_rust_call": "A Lua loop calling the bound function 1000 times; per call = loop / 1000.",
    "method_call": "`obj:get()` 1000 times; per call = loop / 1000.",
    "property_get": "`obj.value` 1000 times; per access = loop / 1000.",
    "iterator": "A generic `for` over a 100-element array iterator, 1000 loops; per element = loop / 100000.",
    "host_side": "Host-side operations, per call.",
}
PER_ITEM_DIVISOR = {"luau_to_rust_call": 1000, "method_call": 1000, "property_get": 1000, "iterator": 100_000}


def load():
    records = defaultdict(list)
    for estimates in sorted(CRITERION_DIR.rglob("new/estimates.json")):
        meta_path = estimates.parent / "benchmark.json"
        if not meta_path.exists():
            continue
        meta = json.loads(meta_path.read_text())
        est = json.loads(estimates.read_text())
        name = meta.get("function_id") or meta.get("value_str") or meta["group_id"]
        records[meta["group_id"]].append(
            (name, est["mean"]["point_estimate"], est["std_dev"]["point_estimate"], meta.get("throughput"))
        )
    return records


def unit_for(ns):
    if ns < 1_000:
        return 1.0, "ns"
    if ns < 1_000_000:
        return 1_000.0, "µs"
    return 1_000_000.0, "ms"


def fmt(ns):
    div, unit = unit_for(ns)
    value = ns / div
    return f"{value:.1f} {unit}" if value >= 100 else f"{value:.2f} {unit}"


def nice_ceil(v):
    if v <= 0:
        return 1.0
    mag = 10 ** math.floor(math.log10(v))
    for step in (1, 2, 5, 10):
        if step * mag >= v:
            return step * mag
    return 10 * mag


def chart(title, labels, values_ns):
    div, unit = unit_for(max(values_ns))
    scaled = [v / div for v in values_ns]
    x_axis = ", ".join(f'"{label}"' for label in labels)
    data = ", ".join(f"{v:.2f}" for v in scaled)
    return "\n".join(
        [
            "```mermaid",
            "xychart-beta",
            f'    title "{title}"',
            f"    x-axis [{x_axis}]",
            f'    y-axis "time ({unit})" 0 --> {nice_ceil(max(scaled) * 1.2):.2f}',
            f"    bar [{data}]",
            "```",
        ]
    )


def main():
    records = load()
    if not records:
        raise SystemExit("no Criterion output under target/criterion; run cargo bench --bench hot_paths")
    machine = f"{platform.machine()}, {platform.system()} {platform.release()}"
    cpu = ""
    try:
        for line in Path("/proc/cpuinfo").read_text().splitlines():
            if line.startswith("model name"):
                cpu = line.split(":", 1)[1].strip()
                break
    except OSError:
        pass
    lines = [
        "# Benchmarks",
        "",
        f"> Generated {date.today()} · `cargo bench --bench hot_paths` → Criterion → "
        f"[scripts/gen_benchmarks.py](scripts/gen_benchmarks.py) · {cpu or machine}",
        "",
        "All times are wall-clock means measured by [Criterion.rs](https://github.com/bheisler/criterion.rs)"
        " (95 % confidence interval). Debug assertions off, default features (interpreter only).",
        "",
    ]
    for group in GROUP_ORDER + sorted(set(records) - set(GROUP_ORDER)):
        if group not in records:
            continue
        rows = sorted(records[group], key=lambda row: row[1])
        divisor = PER_ITEM_DIVISOR.get(group)
        lines += [f"## {group}", "", GROUP_NOTES.get(group, ""), ""]
        header = "| Variant | Mean | ± Std Dev |" + (" Per item |" if divisor else "")
        lines += [header, "|---|---:|---:|" + ("---:|" if divisor else "")]
        for name, mean, std, _ in rows:
            row = f"| {name} | {fmt(mean)} | {fmt(std)} |"
            if divisor:
                row += f" {fmt(mean / divisor)} |"
            lines.append(row)
        lines += ["", chart(group, [name for name, *_ in rows], [mean / (divisor or 1) for _, mean, *_ in rows]), ""]
    OUTPUT.write_text("\n".join(lines))
    print(f"wrote {OUTPUT}")


if __name__ == "__main__":
    main()
