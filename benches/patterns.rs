//! Record binding patterns against the handwritten Luau with the same contract: one holder
//! for the source value, then one ordinary indexed read per field, in source order. Nothing is
//! compared against code that reads fewer fields. With the `jit` feature every chunk is
//! compiled to native code; without it the chunks are interpreted.
//!
//! Each case processes 4,096 records. Retired-instruction counts come from the same
//! `perf_event_open` shim the comprehension bench uses.

use criterion::measurement::WallTime;
use criterion::{BenchmarkGroup, Criterion, Throughput, criterion_group, criterion_main};
use l3i::Runtime;
use l3i::extension::{RuntimePlan, RuntimePolicy};
use l3i::source::LoadScope;

const ITEMS: u64 = 4096;
const NIL_GUARD: &str =
    "if value == nil then error(\"L3i comprehension projection produced nil; filter nil explicitly\") end";

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
#[path = "instructions/counter.rs"]
mod counter;

fn runtime() -> Runtime {
    let policy = RuntimePolicy::new();
    #[cfg(feature = "jit")]
    let policy = policy.native_code(l3i::extension::NativeCodePolicy {
        mode: l3i::native_code::NativeCodeMode::Eager,
        ..l3i::extension::NativeCodePolicy::default()
    });
    let plan = RuntimePlan::builder().policy(policy).finalize().unwrap();
    let runtime = Runtime::from_plan(&plan).unwrap();
    runtime
        .exec(&format!(
            "records = table.create({ITEMS}) proxies = table.create({ITEMS})
            for i = 1, {ITEMS} do
                records[i] = {{ x = i, y = i * 2, z = i * 3, position = {{ x = i, y = -i }} }}
                local fields = records[i]
                proxies[i] = setmetatable({{}}, {{ __index = function(_, key) return fields[key] end }})
            end"
        ))
        .unwrap();
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
            eprintln!("instructions {label}: hardware counter unavailable (permissions or PMU)");
            return;
        };
        let mut rounds = [0u64; 7];
        for round in &mut rounds {
            *round = counter.measure(&mut || {
                for _ in 0..128 {
                    invoke();
                }
            });
        }
        rounds.sort_unstable();
        eprintln!("instructions {label}: per-call min/median {}/{}", rounds[0] / 128, rounds[3] / 128);
    }
    #[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
    {
        let _ = (label, invoke);
        eprintln!("instructions {label}: hardware counter unsupported on this platform");
    }
}

/// Benchmarks `body`, a function body returning a number, after asserting it returns `expected`.
fn case(group: &mut BenchmarkGroup<'_, WallTime>, label: &str, body: &str, expected: f64) {
    let runtime = runtime();
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

/// Sums of `i * k` over the records.
fn total(k: u64) -> f64 {
    #[allow(clippy::cast_precision_loss)]
    let total = (k * ITEMS * (ITEMS + 1) / 2) as f64;
    total
}

fn declarations(c: &mut Criterion) {
    let mut group = c.benchmark_group("patterns_declarations");
    group.throughput(Throughput::Elements(ITEMS));
    let loop_ = |body: &str| format!("local src, total = records, 0 for i = 1, #src do {body} end return total");
    case(&mut group, "jsl two fields", &loop_("local {x, y} = src[i] total += x + y"), total(3));
    case(
        &mut group,
        "luau two fields",
        &loop_("local r = src[i] local x = r.x local y = r.y total += x + y"),
        total(3),
    );
    case(&mut group, "jsl three fields", &loop_("local {x, y, z} = src[i] total += x + y + z"), total(6));
    case(
        &mut group,
        "luau three fields",
        &loop_("local r = src[i] local x = r.x local y = r.y local z = r.z total += x + y + z"),
        total(6),
    );
    case(&mut group, "jsl nested", &loop_("local {position: {x}, z} = src[i] total += x + z"), total(4));
    case(
        &mut group,
        "luau nested",
        &loop_("local r = src[i] local p = r.position local x = p.x local z = r.z total += x + z"),
        total(4),
    );
    let proxies = |body: &str| format!("local src, total = proxies, 0 for i = 1, #src do {body} end return total");
    case(&mut group, "jsl metatable two fields", &proxies("local {x, y} = src[i] total += x + y"), total(3));
    case(
        &mut group,
        "luau metatable two fields",
        &proxies("local r = src[i] local x = r.x local y = r.y total += x + y"),
        total(3),
    );
    group.finish();
}

fn parameters(c: &mut Criterion) {
    let mut group = c.benchmark_group("patterns_parameters");
    group.throughput(Throughput::Elements(ITEMS));
    let call = "local src, total = records, 0 for i = 1, #src do total += f(src[i], 1) end return total";
    case(
        &mut group,
        "jsl typed parameter",
        &format!(
            "type R = {{ x: number, y: number }} local function f({{x, y}}: R, k: number) return (x + y) * k end {call}"
        ),
        total(3),
    );
    case(
        &mut group,
        "luau typed parameter",
        &format!(
            "type R = {{ x: number, y: number }} local function f(r: R, k: number) local x = r.x local y = r.y return (x + y) * k end {call}"
        ),
        total(3),
    );
    group.finish();
}

fn generators(c: &mut Criterion) {
    let mut group = c.benchmark_group("patterns_generators");
    group.throughput(Throughput::Elements(ITEMS));
    case(&mut group, "jsl sum", "return sum[for {x, y} in records => x + y]", total(3));
    case(
        &mut group,
        "luau sum",
        &format!(
            "local src, total = records, 0 for i = 1, #src do local r = src[i] local x = r.x local y = r.y \
             local value = x + y {NIL_GUARD} total += value end return total"
        ),
        total(3),
    );
    // Odd x: the filter keeps half. Fields are read before the filter in both.
    let half = {
        let n = ITEMS / 2;
        #[allow(clippy::cast_precision_loss)]
        let half = (3 * n * n) as f64;
        half
    };
    case(&mut group, "jsl filtered sum", "return sum[for {x, y} in records if x % 2 == 1 => x + y]", half);
    case(
        &mut group,
        "luau filtered sum",
        &format!(
            "local src, total = records, 0 for i = 1, #src do local r = src[i] local x = r.x local y = r.y \
             if x % 2 == 1 then local value = x + y {NIL_GUARD} total += value end end return total"
        ),
        half,
    );
    case(&mut group, "jsl nested count", "return #[for {position: {y}} in records if y < 0 => y]", 4096.0);
    case(
        &mut group,
        "luau nested count",
        &format!(
            "local src, n = records, 0 for i = 1, #src do local r = src[i] local p = r.position local y = p.y \
             if y < 0 then local value = y {NIL_GUARD} n += 1 end end return n"
        ),
        4096.0,
    );
    group.finish();
}

/// A module of 2,000 small functions on one table, with `extra` appended: ordinary Luau when
/// `extra` is empty. `patterned` gives every function a parameter and a declaration pattern.
fn module(patterned: bool, extra: &str) -> String {
    use std::fmt::Write as _;
    let mut source = String::from("type P = { x: number, y: number }\nlocal M = {}\n");
    for i in 0..2000 {
        if patterned {
            writeln!(
                source,
                "function M.f{i}({{x, y}}: P, dt: number)\n  local {{z, w: alias}} = {{ z = x, w = y }}\n  return x + y + z + alias + dt\nend"
            )
        } else {
            writeln!(
                source,
                "function M.f{i}(a: number, b: number)\n  local t = {{ a = a, b = b, list = {{ a, b }} }}\n  return t.a + t.b + t.list[1]\nend"
            )
        }
        .unwrap();
    }
    source.push_str(extra);
    source
}

/// Compilation, not execution: what the frontend, lowering, source map and Luau's parser and
/// compiler cost on the same module with no JSL, one comprehension, a few patterns, or a
/// pattern in every function. With `L3I_PROFILE_CASE=<label>` only that case runs, setup
/// included, so a profiler attached to the bench sees one compilation and nothing else.
fn compilation(c: &mut Criterion) {
    let only = std::env::var("L3I_PROFILE_CASE").ok();
    let mut group = c.benchmark_group("patterns_compile");
    let options = l3i::source::CompileOptions::default();
    let cases = [
        ("plain module", module(false, "")),
        ("one comprehension", module(false, "local doubled = [for v in { 1, 2 } => v * 2]\n")),
        (
            "three patterns",
            module(
                false,
                "local {x, y}: P = { x = 1, y = 2 }\nlocal function g({x}: P) return x end\nlocal {a} = { a = x + y }\n",
            ),
        ),
        ("patterns in every function", module(true, "")),
    ];
    for (label, source) in &cases {
        if only.as_deref().is_some_and(|only| only != *label) {
            continue;
        }
        l3i::source::compile(source, &options).unwrap();
        group.throughput(Throughput::Bytes(source.len() as u64));
        retired_instructions(label, &mut || {
            std::hint::black_box(l3i::source::compile(std::hint::black_box(source), &options).unwrap());
        });
        group.bench_function(*label, |b| {
            b.iter(|| l3i::source::compile(std::hint::black_box(source), &options).unwrap());
        });
    }
    group.finish();
}

/// The execution groups, unless one compilation case is being profiled.
fn execution(c: &mut Criterion) {
    if std::env::var_os("L3I_PROFILE_CASE").is_none() {
        declarations(c);
        parameters(c);
        generators(c);
    }
}

criterion_group!(benches, execution, compilation);
criterion_main!(benches);
