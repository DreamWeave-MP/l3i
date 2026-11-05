# Native CI evidence at 1f0e952

Downloaded from research run https://github.com/DreamWeave-MP/l3i/actions/runs/41944639424 . Both ubuntu-24.04 and ubuntu-24.04-arm jobs passed debug no-JIT, release no-JIT, optional Luau JIT release, formatting and all-feature pedantic Clippy. Their native benchmark artifacts are preserved alongside this file without timing edits (trailing whitespace removed).

Production run https://github.com/DreamWeave-MP/l3i/actions/runs/41944640482 passed format/audit/MSRV/packaging, Linux, Windows, Intel macOS, ARM macOS and site verification. These statements apply to 1f0e952f812e4285fa5cc438efbbe98ca4299cc7, not subsequent commits.

The ARM artifact establishes native execution and measurements, not emulation. Shared hosted runners are not controlled performance hardware. Do not compare ARM and x86 absolute timings as architectural speed ratios: they are different CPUs and machines. Consult the run's compiler/CPU step for hardware provenance. Benchmarks are no-JIT; optional Luau JIT is tested separately.

On this ARM runner, the synthetic JSL forward runInto measured 961.128 us fast and 2652.078 us strict. This is the repository's small validated synthetic workload, not representative language-model inference. The raw artifact includes kernel/program/JSL shape measurements and prepared-weight variants. No peak RSS was captured by this CI job; the reported budget peak is domain payload accounting, not process RSS.
