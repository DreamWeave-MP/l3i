//! The data plane against the best obvious Luau, across sizes: `sum` and a filtered count as
//! recognized JSL, as the JSL scalar fallback (no extension), as the explicit `@dream/data`
//! call, and as a handwritten `buffer.readu8` loop; then gather and a stable argsort against
//! their obvious Luau forms. With the `jit` feature every chunk is compiled to native code, so
//! the loop baselines are Luau `CodeGen` loops; without it they are interpreted.
//!
//! Retired-instruction counts come from the same `perf_event_open` shim the comprehension bench
//! uses; the small-N rows locate the crossover where one host call stops paying for itself.

#![allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::too_many_lines,
    clippy::items_after_statements,
    clippy::needless_pass_by_value
)]

use criterion::measurement::WallTime;
use criterion::{BenchmarkGroup, Criterion, Throughput, criterion_group};
use l3i::Runtime;
use l3i::data::DataExtension;
use l3i::extension::{RuntimePlan, RuntimePolicy};
use l3i::source::LoadScope;

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
use crate::counter;

fn policy(with_data: bool) -> RuntimePolicy {
    let policy = RuntimePolicy::new();
    let policy = if with_data { policy.compat_global("@dream/data", "data") } else { policy };
    #[cfg(feature = "jit")]
    let policy = policy.native_code(l3i::extension::NativeCodePolicy {
        mode: l3i::native_code::NativeCodeMode::Eager,
        ..l3i::extension::NativeCodePolicy::default()
    });
    policy
}

/// A runtime with the data plane, or the same policy without it (the JSL scalar fallback).
fn runtime(with_data: bool, setup: &str) -> Runtime {
    let builder = RuntimePlan::builder().policy(policy(with_data));
    let plan = if with_data { builder.extension(DataExtension) } else { builder }.finalize().unwrap();
    let runtime = Runtime::from_plan(&plan).unwrap();
    runtime.exec(setup).unwrap();
    // Native code exits to the interpreter on a global access unless the environment is
    // marked safe; the inputs are globals, so sandbox them once they exist.
    runtime.sandbox_globals().unwrap();
    runtime
}

fn retired_instructions(label: &str, invoke: &mut dyn FnMut()) {
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    {
        use counter::{Counter, PERF_COUNT_HW_INSTRUCTIONS, PERF_TYPE_HARDWARE};
        let Some(counter) = Counter::open(PERF_TYPE_HARDWARE, PERF_COUNT_HW_INSTRUCTIONS) else {
            eprintln!("instructions {label}: hardware counter unavailable");
            return;
        };
        let mut rounds = [0u64; 5];
        for round in &mut rounds {
            *round = counter.measure(&mut || {
                for _ in 0..64 {
                    invoke();
                }
            });
        }
        rounds.sort_unstable();
        eprintln!("instructions {label}: per-call min {}", rounds[0] / 64);
    }
    #[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
    {
        let _ = (label, invoke);
    }
}

/// Benchmarks `body` (a function body returning a number) after asserting it returns `expected`.
fn case(
    group: &mut BenchmarkGroup<'_, WallTime>,
    label: &str,
    with_data: bool,
    setup: &str,
    body: &str,
    expected: f64,
) {
    let runtime = runtime(with_data, setup);
    let stack = runtime.stack();
    // Loaded through LoadScope so that, under jit, the chunk and its function compile natively.
    let chunk = stack
        .load_source("=case", &format!("--!native\nreturn function() {body} end"), &runtime.compile_options())
        .unwrap();
    let function = chunk.invoke::<l3i::value::Function, _>(&stack, ()).unwrap();
    let actual = function.invoke::<f64, _>(&stack, ()).unwrap();
    assert!((actual - expected).abs() < 1e-9, "{label}: {actual} vs {expected}");
    retired_instructions(label, &mut || {
        std::hint::black_box(function.invoke::<f64, _>(&stack, ()).unwrap());
    });
    group.bench_function(label, |b| b.iter(|| function.invoke::<f64, _>(&stack, ()).unwrap()));
}

fn setup(n: usize) -> String {
    format!(
        "n = {n} buf = buffer.create(n) for i = 0, n - 1 do buffer.writeu8(buf, i, (i * 37) % 251) end \
         fbuf = buffer.create(n * 4) for i = 0, n - 1 do buffer.writef32(fbuf, i * 4, ((i * 37) % 251) / 7) end sel = data and data.selection(n) U8 = data and data.u8()"
    )
}

fn expected_sum(n: usize) -> f64 {
    (0..n).map(|i| ((i * 37) % 251) as f64).sum()
}

fn expected_count(n: usize) -> f64 {
    (0..n).filter(|i| (i * 37) % 251 > 127).count() as f64
}

fn sums(c: &mut Criterion) {
    for n in [16usize, 256, 4096, 65536] {
        let mut group = c.benchmark_group(format!("data_sum_u8/{n}"));
        group.throughput(Throughput::Elements(n as u64));
        let setup = setup(n);
        let jsl = "return sum[for x in buf[1:n] => x]";
        case(&mut group, "jsl recognized (data plane)", true, &setup, jsl, expected_sum(n));
        case(&mut group, "jsl scalar fallback", false, &setup, jsl, expected_sum(n));
        case(&mut group, "explicit data.sum", true, &setup, "return data.sum(buf, 'u8', 0, n)", expected_sum(n));
        case(
            &mut group,
            "handwritten readu8 loop",
            false,
            &setup,
            "local b, total = buf, 0 for i = 0, n - 1 do total += buffer.readu8(b, i) end return total",
            expected_sum(n),
        );
        group.finish();
    }
    for n in [256usize, 65536] {
        let mut group = c.benchmark_group(format!("data_sum_f32/{n}"));
        group.throughput(Throughput::Elements(n as u64));
        let setup = setup(n);
        let expected = (0..n).map(|i| f64::from((((i * 37) % 251) as f32) / 7.0)).sum();
        case(&mut group, "explicit data.sum", true, &setup, "return data.sum(fbuf, 'f32', 0, n)", expected);
        case(
            &mut group,
            "handwritten readf32 loop",
            false,
            &setup,
            "local b, total = fbuf, 0 for i = 0, n - 1 do total += buffer.readf32(b, i * 4) end return total",
            expected,
        );
        group.finish();
    }
}

fn counts(c: &mut Criterion) {
    for n in [16usize, 256, 4096, 65536] {
        let mut group = c.benchmark_group(format!("data_count_gt/{n}"));
        group.throughput(Throughput::Elements(n as u64));
        let setup = setup(n);
        let jsl = "return #[for x in buf[1:n] if x > 127 => x]";
        case(&mut group, "jsl recognized (data plane)", true, &setup, jsl, expected_count(n));
        case(&mut group, "jsl scalar fallback", false, &setup, jsl, expected_count(n));
        case(
            &mut group,
            "explicit data.countWhere",
            true,
            &setup,
            "return data.countWhere(buf, 'u8', 0, n, 'gt', 127)",
            expected_count(n),
        );
        case(
            &mut group,
            "explicit compare into selection then count",
            true,
            &setup,
            "return data.select(buf, 'u8', 0, n, 'gt', 127, sel):count()",
            expected_count(n),
        );
        case(
            &mut group,
            "handwritten readu8 loop",
            false,
            &setup,
            "local b, c = buf, 0 for i = 0, n - 1 do if buffer.readu8(b, i) > 127 then c += 1 end end return c",
            expected_count(n),
        );
        case(
            &mut group,
            "receiver selectGt into a reused selection, then count (IR loop under jit)",
            true,
            &setup,
            "local K: dream_data_Kind_u8 = U8 local s = K:selectGt(buf, 0, n, 127, sel) return s:count()",
            expected_count(n),
        );
        case(
            &mut group,
            "receiver countGt (IR loop under jit)",
            true,
            &setup,
            "local K: dream_data_Kind_u8 = U8 local c = K:countGt(buf, 0, n, 127) return c",
            expected_count(n),
        );
        group.finish();
    }
    for n in [256usize, 65536] {
        let mut group = c.benchmark_group(format!("data_min_u8/{n}"));
        group.throughput(Throughput::Elements(n as u64));
        let setup = setup(n);
        let expected = (0..n).map(|i| ((i * 37) % 251) as f64).fold(f64::INFINITY, f64::min);
        case(&mut group, "explicit data.min", true, &setup, "return data.min(buf, 'u8', 0, n)", expected);
        case(
            &mut group,
            "receiver min (IR loop under jit)",
            true,
            &setup,
            "local K: dream_data_Kind_u8 = U8 local least = K:min(buf, 0, n) return least",
            expected,
        );
        case(
            &mut group,
            "handwritten readu8 loop",
            false,
            &setup,
            "local b, least = buf, nil for i = 0, n - 1 do local v = buffer.readu8(b, i) if least == nil or v < least then least = v end end return least",
            expected,
        );
        group.finish();
    }
}

fn movement(c: &mut Criterion) {
    for n in [256usize, 65536] {
        let mut group = c.benchmark_group(format!("data_gather_f32/{n}"));
        group.throughput(Throughput::Elements(n as u64));
        // Indices reverse the span; the destination is reused across calls.
        let setup = format!(
            "{} idx = buffer.create(n * 4) for i = 0, n - 1 do buffer.writeu32(idx, i * 4, n - 1 - i) end dst = buffer.create(n * 4)",
            setup(n)
        );
        let expected = n as f64;
        case(
            &mut group,
            "explicit data.gather",
            true,
            &setup,
            "return data.gather(fbuf, 'f32', 0, n, idx, dst, 0)",
            expected,
        );
        case(
            &mut group,
            "handwritten indexed copy",
            false,
            &setup,
            "local s, d, x = fbuf, dst, idx for i = 0, n - 1 do buffer.writef32(d, i * 4, buffer.readf32(s, buffer.readu32(x, i * 4) * 4)) end return n",
            expected,
        );
        group.finish();
    }
    for n in [256usize, 4096, 65536] {
        let mut group = c.benchmark_group(format!("data_argsort_f32/{n}"));
        group.throughput(Throughput::Elements(n as u64));
        // Keys repeat every 251 elements: many ties, where stability matters.
        // Sandboxed globals are read-only tables, so the baseline builds its index table per call.
        let setup = format!("{} perm = buffer.create(n * 4) scratch = buffer.create(n * 4)", setup(n));
        let expected = n as f64;
        case(
            &mut group,
            "explicit data.argsort (stable)",
            true,
            &setup,
            "return data.argsort(fbuf, 'f32', 0, n, perm)",
            expected,
        );
        case(
            &mut group,
            "explicit data.argsort with caller scratch (stable, no allocation)",
            true,
            &setup,
            "return data.argsort(fbuf, 'f32', 0, n, perm, 0, scratch)",
            expected,
        );
        case(
            &mut group,
            "table.sort of an index table with a key comparator (unstable)",
            false,
            &setup,
            "local keys, o = fbuf, table.create(n) for i = 1, n do o[i] = i - 1 end \
             table.sort(o, function(a, b) return buffer.readf32(keys, a * 4) < buffer.readf32(keys, b * 4) end) return #o",
            expected,
        );
        group.finish();
    }
}

/// Benchmark-only kernels: what one loop with the contract's arithmetic costs with no binder, no
/// Luau call and slices that promise no aliasing. Nothing a script can call runs these, so they
/// are the ceiling a `dream.data` call could approach, not a measurement of one.
mod reference {
    fn read(bytes: [u8; 4]) -> f64 {
        f64::from(f32::from_ne_bytes(bytes))
    }

    fn write(value: f64) -> [u8; 4] {
        (value as f32).to_ne_bytes()
    }

    #[inline(never)]
    pub fn add(left: &[[u8; 4]], right: &[[u8; 4]], out: &mut [[u8; 4]]) {
        for ((out, left), right) in out.iter_mut().zip(left).zip(right) {
            *out = write(read(*left) + read(*right));
        }
    }

    #[inline(never)]
    pub fn scale(source: &[[u8; 4]], factor: f64, out: &mut [[u8; 4]]) {
        for (out, source) in out.iter_mut().zip(source) {
            *out = write(read(*source) * factor);
        }
    }

    #[inline(never)]
    pub fn clamp(source: &[[u8; 4]], low: f64, high: f64, out: &mut [[u8; 4]]) {
        for (out, source) in out.iter_mut().zip(source) {
            let value = read(*source);
            *out = write(if value < low {
                low
            } else if value > high {
                high
            } else {
                value
            });
        }
    }

    #[inline(never)]
    pub fn fill(out: &mut [[u8; 4]], value: f64) {
        out.fill(write(value));
    }

    #[inline(never)]
    pub fn sum(source: &[[u8; 4]]) -> f64 {
        source.iter().fold(0.0, |total, value| total + read(*value))
    }

    #[inline(never)]
    pub fn min(source: &[[u8; 4]]) -> Option<f64> {
        let (first, rest) = source.split_first()?;
        Some(
            rest.iter()
                .map(|value| read(*value))
                .fold(read(*first), |best, value| if value < best { value } else { best }),
        )
    }

    #[inline(never)]
    pub fn count_gt(source: &[[u8; 4]], threshold: f64) -> usize {
        source.iter().map(|value| usize::from(read(*value) > threshold)).sum()
    }
}

/// Whether Criterion is smoke-running the benches (`cargo test --all-targets`), where a million
/// elements and counter rounds would only slow the gate.
fn smoke() -> bool {
    std::env::args().any(|arg| arg == "--test")
}

/// One raw row per case on stderr, for scripts: per-call instructions, cycles and nanoseconds, each
/// the min, median and max of nine rounds. User-space counts only. Instruction counts repeat to
/// within a few parts per thousand on a busy machine; the cycle and nanosecond spreads say how far
/// to trust a row.
fn f32_row(op: &str, n: usize, variant: &str, invoke: &mut dyn FnMut()) {
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    {
        use counter::{Counter, PERF_COUNT_HW_CPU_CYCLES, PERF_COUNT_HW_INSTRUCTIONS, PERF_TYPE_HARDWARE, measure_all};
        if smoke() {
            return;
        }
        let (Some(instructions), Some(cycles)) = (
            Counter::open(PERF_TYPE_HARDWARE, PERF_COUNT_HW_INSTRUCTIONS),
            Counter::open(PERF_TYPE_HARDWARE, PERF_COUNT_HW_CPU_CYCLES),
        ) else {
            eprintln!("f32row|{op}|{n}|{variant}|hardware counters unavailable");
            return;
        };
        let counters = [instructions, cycles];
        // Roughly two million elements a round, so large and small spans get comparable samples.
        let calls = (2_000_000 / n.max(1)).clamp(1, 2048);
        let mut rows = [[0u64; 3]; 9];
        invoke();
        for row in &mut rows {
            let begin = std::time::Instant::now();
            let counted = measure_all(&counters, &mut || {
                for _ in 0..calls {
                    invoke();
                }
            });
            let nanos = begin.elapsed().as_nanos() as u64;
            *row = [counted[0] / calls as u64, counted[1] / calls as u64, nanos / calls as u64];
        }
        let spread = |column: usize| {
            let mut values = rows.map(|row| row[column]);
            values.sort_unstable();
            format!("{}|{}|{}", values[0], values[4], values[8])
        };
        eprintln!("f32row|{op}|{n}|{variant}|{}|{}|{}", spread(0), spread(1), spread(2));
    }
    #[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
    {
        let _ = (op, n, variant, invoke);
    }
}

fn f32_setup(n: usize) -> String {
    format!(
        "n = {n}
         a, b, out = buffer.create((n + 1) * 4), buffer.create((n + 1) * 4), buffer.create((n + 1) * 4)
         ia, ib, iout = buffer.create(n * 4), buffer.create(n * 4), buffer.create(n * 4)
         da, db, dout = buffer.create(n * 8), buffer.create(n * 8), buffer.create(n * 8)
         sa, sout = buffer.create(n * 8 + 8), buffer.create(n * 8 + 8)
         fall = buffer.create(n * 4)
         for i = 0, n - 1 do
             buffer.writef32(fall, i * 4, -i)
             local x, y = ((i * 37) % 251) / 7 - 15, ((i * 53) % 241) / 3 - 40
             buffer.writef32(a, i * 4, x) buffer.writef32(b, i * 4, y)
             buffer.writei32(ia, i * 4, x * 7) buffer.writei32(ib, i * 4, y * 3)
             buffer.writef64(da, i * 8, x) buffer.writef64(db, i * 8, y)
             buffer.writef32(sa, i * 8, x)
         end
         sel = data.selection(n) F = data.f32() I = data.i32() D = data.f64()"
    )
}

/// `(operation, the dream.data call, the handwritten Luau loop)`, contiguous, disjoint spans. A
/// scale factor of 1/3 is not an f32; 0.5 is. `any` and `all` never decide early here: full scans.
const F32_OPS: [(&str, &str, &str); 13] = [
    (
        "add",
        "data.add(a, 'f32', 0, b, 0, out, 0, n) return n",
        "local x, y, o = a, b, out for i = 0, n - 1 do buffer.writef32(o, i * 4, buffer.readf32(x, i * 4) + buffer.readf32(y, i * 4)) end return n",
    ),
    (
        "scale_inexact",
        "data.scale(a, 'f32', 0, n, 1 / 3, out, 0) return n",
        "local x, o, f = a, out, 1 / 3 for i = 0, n - 1 do buffer.writef32(o, i * 4, buffer.readf32(x, i * 4) * f) end return n",
    ),
    (
        "scale_exact",
        "data.scale(a, 'f32', 0, n, 0.5, out, 0) return n",
        "local x, o, f = a, out, 0.5 for i = 0, n - 1 do buffer.writef32(o, i * 4, buffer.readf32(x, i * 4) * f) end return n",
    ),
    (
        "clamp",
        "data.clamp(a, 'f32', 0, n, -5, 5, out, 0) return n",
        "local x, o = a, out for i = 0, n - 1 do buffer.writef32(o, i * 4, math.clamp(buffer.readf32(x, i * 4), -5, 5)) end return n",
    ),
    (
        "fill",
        "data.fill(out, 'f32', 0, n, 1.5) return n",
        "local o = out for i = 0, n - 1 do buffer.writef32(o, i * 4, 1.5) end return n",
    ),
    (
        "sum",
        "return data.sum(a, 'f32', 0, n)",
        "local x, t = a, 0 for i = 0, n - 1 do t += buffer.readf32(x, i * 4) end return t",
    ),
    (
        "sum_filtered",
        "return data.sum(a, 'f32', 0, n, 'gt', 0)",
        "local x, t = a, 0 for i = 0, n - 1 do local v = buffer.readf32(x, i * 4) if v > 0 then t += v end end return t",
    ),
    (
        "min",
        "return data.min(a, 'f32', 0, n) or 0",
        "local x, m = a, nil for i = 0, n - 1 do local v = buffer.readf32(x, i * 4) if m == nil or v < m then m = v end end return m or 0",
    ),
    (
        "argmax",
        "return data.argmax(a, 'f32', 0, n) or 0",
        "local x, m, at = a, nil, 0 for i = 0, n - 1 do local v = buffer.readf32(x, i * 4) if m == nil or v > m then m, at = v, i end end return at",
    ),
    (
        "countWhere",
        "return data.countWhere(a, 'f32', 0, n, 'gt', 0)",
        "local x, c = a, 0 for i = 0, n - 1 do if buffer.readf32(x, i * 4) > 0 then c += 1 end end return c",
    ),
    (
        "any",
        "return if data.any(a, 'f32', 0, n, 'gt', 1000) then 1 else 0",
        "local x = a for i = 0, n - 1 do if buffer.readf32(x, i * 4) > 1000 then return 1 end end return 0",
    ),
    (
        "all",
        "return if data.all(a, 'f32', 0, n, 'lt', 1000) then 1 else 0",
        "local x = a for i = 0, n - 1 do if not (buffer.readf32(x, i * 4) < 1000) then return 0 end end return 1",
    ),
    (
        "select",
        "data.select(a, 'f32', 0, n, 'gt', 0, sel) return n",
        "local x, c = a, 0 for i = 0, n - 1 do if buffer.readf32(x, i * 4) > 0 then c += 1 end end return n",
    ),
];

/// What the specialization must not slow: other kinds, strided layouts, overlapping and in-place
/// spans, and the receivers (Luau IR loops under jit, bound methods without it).
const F32_CONTROLS: [(&str, &str); 25] = [
    // The block scans' worst cases: every block holds an improvement, or the first element decides.
    ("min strictly falling", "return data.min(fall, 'f32', 0, n) or 0"),
    ("argmin strictly falling", "return data.argmin(fall, 'f32', 0, n) or 0"),
    ("max strictly falling (first wins)", "return data.max(fall, 'f32', 0, n) or 0"),
    ("any decided by the first element", "return if data.any(a, 'f32', 0, n, 'lt', 1000) then 1 else 0"),
    ("all decided by the first element", "return if data.all(a, 'f32', 0, n, 'gt', 1000) then 1 else 0"),
    ("add one element past both", "data.add(a, 'f32', 0, a, 0, a, 4, n) return n"),
    ("add i32", "data.add(ia, 'i32', 0, ib, 0, iout, 0, n) return n"),
    ("add f64", "data.add(da, 'f64', 0, db, 0, dout, 0, n) return n"),
    ("add f32@8", "data.add(sa, 'f32@8', 0, sa, 0, sout, 0, n) return n"),
    ("add out one element past left", "data.add(a, 'f32', 0, b, 0, a, 4, n) return n"),
    ("add out one element before left", "data.add(a, 'f32', 4, b, 0, a, 0, n) return n"),
    ("add in place on left", "data.add(a, 'f32', 0, b, 0, a, 0, n) return n"),
    ("scale i32", "data.scale(ia, 'i32', 0, n, 3, iout, 0) return n"),
    ("scale f32@8", "data.scale(sa, 'f32@8', 0, n, 0.5, sout, 0) return n"),
    ("scale in place", "data.scale(a, 'f32', 0, n, -1, a, 0) return n"),
    ("clamp in place", "data.clamp(a, 'f32', 0, n, -1000, 1000, a, 0) return n"),
    ("fill u8", "data.fill(out, 'u8', 0, n, 7) return n"),
    ("sum i32", "return data.sum(ia, 'i32', 0, n)"),
    ("sum f32@8", "return data.sum(sa, 'f32@8', 0, n)"),
    ("min f64", "return data.min(da, 'f64', 0, n) or 0"),
    ("countWhere i32", "return data.countWhere(ia, 'i32', 0, n, 'gt', 0)"),
    ("receiver f32 sum", "local K: dream_data_Kind_f32 = F local t = K:sum(a, 0, n) return t"),
    ("receiver f32 countGt", "local K: dream_data_Kind_f32 = F local c = K:countGt(a, 0, n, 0) return c"),
    ("receiver f32 min", "local K: dream_data_Kind_f32 = F local m = K:min(a, 0, n) return m or 0"),
    ("jsl recognized f32 sum", "return sum[for v in a[1:n, 'f32'] => v]"),
];

/// Contiguous f32 operations at every span length: the `dream.data` call, the handwritten Luau
/// loop (Luau `CodeGen` under jit, interpreted without) and, where one exists, the benchmark-only
/// Rust kernel; then the regression controls. `n = 0` is the bare API cost; a large `n` is bulk
/// execution. Criterion reports time; `f32_row` reports counters and the spread of nine rounds.
fn f32_bulk(c: &mut Criterion) {
    use std::hint::black_box;
    let sizes: &[usize] = if smoke() { &[0, 16] } else { &[0, 1, 16, 256, 1024, 16384, 65536, 1_048_576] };
    let word = |bits: f32| bits.to_ne_bytes();
    // The counter rows run outside Criterion's filter, so honor it here: `-- data_f32/65536`.
    let filter = std::env::args().skip(1).find(|arg| !arg.starts_with('-'));
    for &n in sizes {
        let name = format!("data_f32/{n}");
        if filter.as_deref().is_some_and(|filter| !name.contains(filter)) {
            continue;
        }
        let setup = f32_setup(n);
        let runtime = runtime(true, &setup);
        let stack = runtime.stack();
        // Under jit, native: the chunk and the function it returns.
        let compile = |body: &str| {
            let source = format!("--!native\nreturn function() {body} end");
            let chunk = stack.load_source("=case", &source, &runtime.compile_options()).unwrap();
            chunk.invoke::<l3i::value::Function, _>(&stack, ()).unwrap()
        };
        let mut group = c.benchmark_group(name);
        group.throughput(Throughput::Elements(n as u64));
        group.sample_size(15).measurement_time(std::time::Duration::from_millis(400));
        group.warm_up_time(std::time::Duration::from_millis(100));
        for (op, api, luau) in F32_OPS {
            for (variant, body) in [("dream.data", api), ("luau loop", luau)] {
                let function = compile(body);
                f32_row(op, n, variant, &mut || {
                    black_box(function.invoke::<f64, _>(&stack, ()).unwrap());
                });
                group.bench_function(format!("{op} {variant}"), |b| {
                    b.iter(|| function.invoke::<f64, _>(&stack, ()).unwrap());
                });
            }
        }
        // The same values the Luau setup writes, for the kernels no script can reach.
        let left: Vec<[u8; 4]> = (0..n).map(|i| word(((((i * 37) % 251) as f64) / 7.0 - 15.0) as f32)).collect();
        let right: Vec<[u8; 4]> = (0..n).map(|i| word(((((i * 53) % 241) as f64) / 3.0 - 40.0) as f32)).collect();
        let mut sink = vec![[0u8; 4]; n];
        type Kernel<'a> = Box<dyn FnMut(&mut [[u8; 4]]) + 'a>;
        let mut kernels: [(&str, Kernel<'_>); 7] = [
            ("add", Box::new(|out| reference::add(black_box(&left), black_box(&right), out))),
            ("scale_inexact", Box::new(|out| reference::scale(black_box(&left), black_box(1.0 / 3.0), out))),
            ("clamp", Box::new(|out| reference::clamp(black_box(&left), black_box(-5.0), black_box(5.0), out))),
            ("fill", Box::new(|out| reference::fill(out, black_box(1.5)))),
            (
                "sum",
                Box::new(|_| {
                    black_box(reference::sum(black_box(&left)));
                }),
            ),
            (
                "min",
                Box::new(|_| {
                    black_box(reference::min(black_box(&left)));
                }),
            ),
            (
                "countWhere",
                Box::new(|_| {
                    black_box(reference::count_gt(black_box(&left), black_box(0.0)));
                }),
            ),
        ];
        for (op, kernel) in &mut kernels {
            f32_row(op, n, "rust reference", &mut || kernel(black_box(&mut sink)));
            group.bench_function(format!("{op} rust reference"), |b| b.iter(|| kernel(black_box(&mut sink))));
        }
        if matches!(n, 0 | 1 | 16 | 1024 | 65536) {
            for (control, body) in F32_CONTROLS {
                let function = compile(body);
                f32_row(control, n, "control", &mut || {
                    black_box(function.invoke::<f64, _>(&stack, ()).unwrap());
                });
                group.bench_function(format!("control {control}"), |b| {
                    b.iter(|| function.invoke::<f64, _>(&stack, ()).unwrap());
                });
            }
        }
        group.finish();
    }
}

fn all(c: &mut Criterion) {
    sums(c);
    counts(c);
    movement(c);
    f32_bulk(c);
}

criterion_group!(benches, all);
