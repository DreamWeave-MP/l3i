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
use criterion::{BenchmarkGroup, Criterion, Throughput, criterion_group, criterion_main};
use l3i::Runtime;
use l3i::data::DataExtension;
use l3i::extension::{RuntimePlan, RuntimePolicy};
use l3i::source::LoadScope;

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
#[path = "instructions/counter.rs"]
mod counter;

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
    runtime.sandbox_globals();
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
         fbuf = buffer.create(n * 4) for i = 0, n - 1 do buffer.writef32(fbuf, i * 4, ((i * 37) % 251) / 7) end sel = data and data.selection(n)"
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
            "explicit data.count",
            true,
            &setup,
            "return data.count(buf, 'u8', 0, n, 'gt', 127)",
            expected_count(n),
        );
        case(
            &mut group,
            "explicit compare into selection then count",
            true,
            &setup,
            "return data.compare(buf, 'u8', 0, n, 'gt', 127, sel):count()",
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
        let setup = format!("{} perm = buffer.create(n * 4)", setup(n));
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

fn all(c: &mut Criterion) {
    sums(c);
    counts(c);
    movement(c);
}

criterion_group!(benches, all);
criterion_main!(benches);
