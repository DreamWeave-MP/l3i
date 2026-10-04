//! Eager comprehension lowering versus the handwritten loop shape it is intended to match.
//! The comprehension syntax is rewritten by L3i before Luau sees the source.

use criterion::{Criterion, Throughput, criterion_group, criterion_main};
use l3i::Runtime;
use l3i::value::Table;

const ITEMS: u64 = 4096;

fn runtime() -> Runtime {
    let runtime = Runtime::new().unwrap();
    runtime
        .exec(&format!(
            "values = table.create({ITEMS}) for i = 1, {ITEMS} do values[i] = i end"
        ))
        .unwrap();
    runtime
}

fn eager_list_comprehensions(c: &mut Criterion) {
    let runtime = runtime();
    let stack = runtime.stack();

    let dense_comprehension = runtime
        .load_function("return function() return [for x in values => x * 2] end")
        .unwrap();
    let dense_handwritten = runtime
        .load_function(
            "return function() local src = values local n = #src local out = table.create(n) \
             for i = 1, n do local x = src[i] out[i] = x * 2 end return out end",
        )
        .unwrap();

    let filtered_comprehension = runtime
        .load_function("return function() return [for x in values if x % 2 == 0 => x * 2] end")
        .unwrap();
    let filtered_handwritten = runtime
        .load_function(
            "return function() local src = values local n = #src local out = table.create(n) local j = 0 \
             for i = 1, n do local x = src[i] if x % 2 == 0 then j += 1 out[j] = x * 2 end end return out end",
        )
        .unwrap();
    let filtered_insert = runtime
        .load_function(
            "return function() local src = values local out = {} \
             for i = 1, #src do local x = src[i] if x % 2 == 0 then table.insert(out, x * 2) end end return out end",
        )
        .unwrap();

    let count_comprehension = runtime
        .load_function("return function() return #[for x in values if x % 2 == 0 => x * 2] end")
        .unwrap();
    let count_handwritten = runtime
        .load_function(
            "return function() local src = values local n = 0 \
             for i = 1, #src do local x = src[i] if x % 2 == 0 then local value = x * 2 \
             if value == nil then error('nil') end n += 1 end end return n end",
        )
        .unwrap();

    let mut dense = c.benchmark_group("comprehension_dense");
    dense.throughput(Throughput::Elements(ITEMS));
    dense.bench_function("l3i comprehension", |b| {
        b.iter(|| dense_comprehension.invoke::<Table, _>(&stack, ()).unwrap())
    });
    dense.bench_function("handwritten preallocated loop", |b| {
        b.iter(|| dense_handwritten.invoke::<Table, _>(&stack, ()).unwrap())
    });
    dense.finish();

    let mut filtered = c.benchmark_group("comprehension_filtered");
    filtered.throughput(Throughput::Elements(ITEMS));
    filtered.bench_function("l3i fused comprehension", |b| {
        b.iter(|| filtered_comprehension.invoke::<Table, _>(&stack, ()).unwrap())
    });
    filtered.bench_function("handwritten fused loop", |b| {
        b.iter(|| filtered_handwritten.invoke::<Table, _>(&stack, ()).unwrap())
    });
    filtered.bench_function("table.insert loop", |b| {
        b.iter(|| filtered_insert.invoke::<Table, _>(&stack, ()).unwrap())
    });
    filtered.finish();

    let mut count = c.benchmark_group("comprehension_count_fused");
    count.throughput(Throughput::Elements(ITEMS));
    count.bench_function("l3i fused length", |b| {
        b.iter(|| count_comprehension.invoke::<f64, _>(&stack, ()).unwrap())
    });
    count.bench_function("handwritten fused count", |b| {
        b.iter(|| count_handwritten.invoke::<f64, _>(&stack, ()).unwrap())
    });
    count.finish();
}

criterion_group!(benches, eager_list_comprehensions);
criterion_main!(benches);
