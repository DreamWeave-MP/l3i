//! Execution, not compilation, of eager projections, filtered projections and fused lengths.
//! Inputs are dense non-nil arrays; the even predicate accepts half the input. Unchecked
//! loops are best-obvious baselines for this data, not the full comprehension nil contract.
//! Callback cases use the same pure pred/transform and filter *before* transforming.
//! Two-pass filter-then-map changes callback ordering: equivalent for these pure callbacks,
//! not arbitrary side effects. No transform-then-filter comparison is made.

use criterion::measurement::WallTime;
use criterion::{BenchmarkGroup, Criterion, Throughput, criterion_group, criterion_main};
use l3i::Runtime;
use l3i::value::Table;

const ITEMS: u64 = 4096;
const NIL_GUARD: &str =
    "if value == nil then error(\"L3i comprehension projection produced nil; filter nil explicitly\") end";

// The shared perf shim currently declares the x86-64 Linux syscall number.
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
#[path = "instructions/counter.rs"]
mod counter;

fn retired_instructions(label: &str, runtime: &Runtime, invoke: &mut dyn FnMut()) {
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    {
        use counter::{Counter, PERF_COUNT_HW_INSTRUCTIONS, PERF_TYPE_HARDWARE};
        let Some(counter) = Counter::open(PERF_TYPE_HARDWARE, PERF_COUNT_HW_INSTRUCTIONS) else {
            eprintln!("instructions {label}: hardware counter unavailable (permissions or PMU)");
            return;
        };
        let mut rounds = [0u64; 7];
        for round in &mut rounds {
            runtime.collect_garbage();
            *round = counter.measure(&mut || {
                for _ in 0..128 {
                    invoke();
                }
            });
        }
        rounds.sort_unstable();
        eprintln!(
            "instructions {label}: per-call min/median/max {}/{}/{} (128 calls/round, host boundary and incremental GC included)",
            rounds[0] / 128,
            rounds[3] / 128,
            rounds[6] / 128
        );
    }
    #[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
    {
        let _ = (runtime, invoke);
        eprintln!("instructions {label}: hardware counter unsupported on this platform");
    }
}

#[derive(Clone, Copy)]
enum Output {
    Dense,
    Filtered,
    Count,
}

fn runtime() -> Runtime {
    let runtime = Runtime::new().unwrap();
    runtime
        .exec(&format!(
            r"values = table.create({ITEMS})
            for i = 1, {ITEMS} do values[i] = i end
            function pred(x) return x % 2 == 0 end
            function transform(x) return x * 2 end
            function filter(src, predicate)
                local out = table.create(#src) local j = 0
                for i = 1, #src do
                    local x = src[i]
                    if predicate(x) then j += 1 out[j] = x end
                end
                return out
            end
            function map(src, project)
                local out = table.create(#src)
                for i = 1, #src do
                    local value = project(src[i]) {NIL_GUARD} out[i] = value
                end
                return out
            end
            function map_filtered(src, predicate, project)
                local out = table.create(#src) local j = 0
                for i = 1, #src do
                    local x = src[i]
                    if predicate(x) then
                        local value = project(x) {NIL_GUARD} j += 1 out[j] = value
                    end
                end
                return out
            end"
        ))
        .unwrap();
    runtime
}

fn bench_case(group: &mut BenchmarkGroup<'_, WallTime>, label: &str, body: &str, output: Output) {
    // Each candidate owns a VM; other candidates cannot leave garbage or roots in its heap.
    let runtime = runtime();
    let function = runtime.load_function(&format!("return function() {body} end")).unwrap();
    let expected_len = match output {
        Output::Dense => ITEMS,
        Output::Filtered | Output::Count => ITEMS / 2,
    };
    let validation = match output {
        Output::Count => format!("assert(result == {expected_len})"),
        Output::Dense | Output::Filtered => {
            let scale = if matches!(output, Output::Dense) { 2 } else { 4 };
            format!(
                "assert(#result == {expected_len}) \
                 for i = 1, {expected_len} do assert(result[i] == i * {scale}) end"
            )
        }
    };
    // Validate every output element outside timing, using exactly the benchmark body.
    runtime.exec(&format!("local f = (function() {body} end) local result = f() {validation}")).unwrap();

    // Runtime loading/exec helpers acquire their own root stack. Open ours only afterwards.
    let stack = runtime.stack();
    let top = stack.top();
    let mut invoke_and_drop = || match output {
        Output::Count => {
            std::hint::black_box(function.invoke::<f64, _>(&stack, ()).unwrap());
        }
        Output::Dense | Output::Filtered => {
            drop(std::hint::black_box(function.invoke::<Table, _>(&stack, ()).unwrap()));
        }
    };
    invoke_and_drop(); // Prime the result registry slot before checking retention.
    runtime.collect_garbage();
    let before = runtime.total_bytes();
    for _ in 0..128 {
        invoke_and_drop();
        assert_eq!(stack.top(), top, "{label}: call frame leaked stack slots");
    }
    runtime.collect_garbage();
    let after = runtime.total_bytes();
    eprintln!("harness {label}: stack {top}->{}, post-GC bytes {before}->{after}", stack.top());
    retired_instructions(label, &runtime, &mut invoke_and_drop);

    group.bench_function(label, |b| {
        // Full GC is outside each timing batch, including warmup. Normal incremental GC
        // stays enabled INSIDE: allocation-heavy cases pay their steady-state cost.
        // Criterion iter drops each returned Table in the timer, releasing its registry
        // pin. Lua frees the table later via GC. iter_with_large_drop would retain all
        // those pins, changing heap pressure and liveness, so deliberately do not use it.
        runtime.collect_garbage();
        match output {
            Output::Count => b.iter(|| function.invoke::<f64, _>(&stack, ()).unwrap()),
            Output::Dense | Output::Filtered => b.iter(|| function.invoke::<Table, _>(&stack, ()).unwrap()),
        }
        assert_eq!(stack.top(), top);
    });
}

fn eager_list_comprehensions(c: &mut Criterion) {
    dense_comparisons(c);
    filtered_comparisons(c);
    count_comparisons(c);
    callback_comparisons(c);
}

fn dense_comparisons(c: &mut Criterion) {
    let mut dense = c.benchmark_group("comprehension_dense");
    dense.throughput(Throughput::Elements(ITEMS));
    bench_case(&mut dense, "l3i projection nil-checked", "return [for x in values => x * 2]", Output::Dense);
    bench_case(
        &mut dense,
        "handwritten preallocated unchecked",
        "local src = values local n = #src local out = table.create(n) \
         for i = 1, n do local x = src[i] out[i] = x * 2 end return out",
        Output::Dense,
    );
    bench_case(
        &mut dense,
        "handwritten preallocated nil-checked",
        &format!(
            "local src = values local n = #src local out = table.create(n) \
                  for i = 1, n do local x = src[i] local value = x * 2 {NIL_GUARD} out[i] = value end return out"
        ),
        Output::Dense,
    );
    dense.finish();
}

fn filtered_comparisons(c: &mut Criterion) {
    let mut filtered = c.benchmark_group("comprehension_filtered");
    filtered.throughput(Throughput::Elements(ITEMS));
    bench_case(
        &mut filtered,
        "l3i fused nil-checked",
        "return [for x in values if x % 2 == 0 => x * 2]",
        Output::Filtered,
    );
    for (label, guard) in [("handwritten fused unchecked", ""), ("handwritten fused nil-checked", NIL_GUARD)] {
        bench_case(
            &mut filtered,
            label,
            &format!(
                "local src = values local n = #src local out = table.create(n) local j = 0 \
                      for i = 1, n do local x = src[i] if x % 2 == 0 then \
                      local value = x * 2 {guard} j += 1 out[j] = value end end return out"
            ),
            Output::Filtered,
        );
    }
    bench_case(
        &mut filtered,
        "table.insert growing unchecked",
        "local src = values local out = {} \
         for i = 1, #src do local x = src[i] if x % 2 == 0 then table.insert(out, x * 2) end end return out",
        Output::Filtered,
    );
    filtered.finish();
}

fn count_comparisons(c: &mut Criterion) {
    let mut count = c.benchmark_group("comprehension_count");
    count.throughput(Throughput::Elements(ITEMS));
    bench_case(
        &mut count,
        "l3i fused length nil-checked",
        "return #[for x in values if x % 2 == 0 => x * 2]",
        Output::Count,
    );
    // The local binding deliberately prevents syntactic #comprehension fusion.
    bench_case(
        &mut count,
        "l3i materialized then length",
        "local out = [for x in values if x % 2 == 0 => x * 2] return #out",
        Output::Count,
    );
    bench_case(
        &mut count,
        "handwritten fused count nil-checked",
        &format!(
            "local src = values local n = 0 \
                  for i = 1, #src do local x = src[i] if x % 2 == 0 then \
                  local value = x * 2 {NIL_GUARD} n += 1 end end return n"
        ),
        Output::Count,
    );
    count.finish();
}

fn callback_comparisons(c: &mut Criterion) {
    let mut callbacks = c.benchmark_group("comprehension_filtered_callbacks");
    callbacks.throughput(Throughput::Elements(ITEMS));
    bench_case(
        &mut callbacks,
        "l3i fused nil-checked",
        "local pred, transform = pred, transform return [for x in values if pred(x) => transform(x)]",
        Output::Filtered,
    );
    bench_case(
        &mut callbacks,
        "handwritten fused nil-checked",
        &format!(
            "local pred, transform = pred, transform local src = values \
                  local out = table.create(#src) local j = 0 \
                  for i = 1, #src do local x = src[i] if pred(x) then \
                  local value = transform(x) {NIL_GUARD} j += 1 out[j] = value end end return out"
        ),
        Output::Filtered,
    );
    bench_case(
        &mut callbacks,
        "map_filtered fused helper nil-checked",
        "return map_filtered(values, pred, transform)",
        Output::Filtered,
    );
    bench_case(
        &mut callbacks,
        "filter then map two-pass nil-checked",
        "return map(filter(values, pred), transform)",
        Output::Filtered,
    );
    callbacks.finish();
}

criterion_group!(benches, eager_list_comprehensions);
criterion_main!(benches);
