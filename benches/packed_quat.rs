//! Packed quaternions (`l3i::quat`): an integer64 rotation against an f32 quaternion userdata,
//! both through the binder, the raw encode/decode cost, and (jit) the natively lowered receiver.

#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::semicolon_if_nothing_returned,
    clippy::missing_panics_doc
)]

#[path = "../tests/support/quat_baseline.rs"]
mod support;

use std::time::Duration;

use std::hint::black_box;

use criterion::{Criterion, Throughput, criterion_group, criterion_main};
use l3i::Runtime;
use l3i::extension::{RuntimePlan, RuntimePolicy};
use l3i::quat::{PackedRotation, QuatExtension};
use support::{QuatBaseline, random_quat};

const CALLS: u64 = 1000;

fn policy() -> RuntimePolicy {
    RuntimePolicy::new().compat_global("@dream/quat", "quat").compat_global("@dream/quatud", "quatud")
}

fn raw(c: &mut Criterion) {
    let mut rng = 42u64;
    let quats: Vec<_> = (0..1024).map(|_| random_quat(&mut rng)).collect();
    let packed: Vec<_> = quats.iter().map(|q| PackedRotation::encode(*q)).collect();
    let mut group = c.benchmark_group("packed_quat_raw");
    group.throughput(Throughput::Elements(1024));
    group.bench_function("encode smallest-three", |b| {
        b.iter(|| {
            for q in &quats {
                black_box(PackedRotation::encode(*q));
            }
        })
    });
    group.bench_function("decode smallest-three", |b| {
        b.iter(|| {
            for p in &packed {
                black_box(p.decode());
            }
        })
    });
    group.bench_function("f64 mul (reference)", |b| {
        b.iter(|| {
            for pair in quats.windows(2) {
                black_box(pair[0] * pair[1]);
            }
        })
    });
    group.bench_function("decode, mul, encode", |b| {
        b.iter(|| {
            for pair in packed.windows(2) {
                black_box(PackedRotation::encode(pair[0].decode() * pair[1].decode()));
            }
        })
    });
    group.finish();
}

fn through_luau(c: &mut Criterion) {
    let plan =
        RuntimePlan::builder().policy(policy()).extension(QuatExtension).extension(QuatBaseline).finalize().unwrap();
    let runtime = Runtime::from_plan(&plan).unwrap();
    runtime
        .exec(
            "a = quat.axisAngle(vector.create(0, 0, 1), 0.3) b = quat.axisAngle(vector.create(1, 0, 0), 0.7) \
             ua = quatud.axisAngle(vector.create(0, 0, 1), 0.3) ub = quatud.axisAngle(vector.create(1, 0, 0), 0.7) \
             v = vector.create(1, 2, 3)",
        )
        .unwrap();
    let cases = [
        ("packed mul", "s = quat.mul(a, b)"),
        ("userdata mul (allocates)", "s = ua:mul(ub)"),
        ("packed slerp", "s = quat.slerp(a, b, 0.5)"),
        ("userdata slerp (allocates)", "s = ua:slerp(ub, 0.5)"),
        ("packed rotate vector", "s = quat.rotate(a, v)"),
        ("userdata rotate vector", "s = ua:rotate(v)"),
    ];
    let mut group = c.benchmark_group("packed_quat_luau");
    group.throughput(Throughput::Elements(CALLS));
    for (name, body) in cases {
        let function = runtime
            .load_function(&format!(
                "return function() local a, b, ua, ub, v, quat = a, b, ua, ub, v, quat local s for i = 1, {CALLS} do {body} end return 0 end"
            ))
            .unwrap();
        let stack = runtime.stack();
        group.bench_function(name, |b| b.iter(|| function.invoke::<f64, _>(&stack, ()).unwrap()));
    }
    group.finish();
}

fn configure() -> Criterion {
    Criterion::default().warm_up_time(Duration::from_secs(1)).measurement_time(Duration::from_secs(3))
}

/// The lowered receiver against the binder paths, all inside natively compiled code.
#[cfg(feature = "jit")]
fn lowered(c: &mut Criterion) {
    use l3i::extension::NativeCodePolicy;
    use l3i::native_code::NativeCodeMode;
    use l3i::runtime::{CallContext, MemoryCategory};
    use l3i::sandbox::{InstanceSpec, SandboxOptions};
    use l3i::value::Function;

    let policy = policy().native_code(NativeCodePolicy { mode: NativeCodeMode::Eager, ..NativeCodePolicy::default() });
    let plan =
        RuntimePlan::builder().policy(policy).extension(QuatExtension).extension(QuatBaseline).finalize().unwrap();
    let runtime = Runtime::from_plan(&plan).unwrap();
    if !runtime.native_code().is_some_and(l3i::native_code::NativeCodeGen::is_available) {
        return;
    }
    let sandbox = runtime
        .sandbox(|_| {}, SandboxOptions { compile_options: runtime.compile_options(), ..SandboxOptions::default() })
        .unwrap();
    let loader = runtime.load_function("return function(name) error('module ' .. name .. ' not found') end").unwrap();
    let instance = sandbox
        .new_instance(&runtime, &InstanceSpec { name: "q", packages: &[], hidden_data: None, loader: &loader })
        .unwrap();
    // Every case feeds its result into the next call so no store is dead: Luau's IR optimiser
    // removes stores to registers nothing reads and would otherwise delete a lowered call.
    let cases = [
        ("lowered Q:mul (native)", "s = Q:mul(s, b)"),
        ("binder quat.mul (native)", "s = quat.mul(s, b)"),
        ("userdata mul (native, allocates)", "us = us:mul(ub)"),
        ("lowered Q:rotate (native)", "w = Q:rotate(a, w)"),
        ("binder quat.rotate (native)", "w = quat.rotate(a, w)"),
        ("userdata rotate (native)", "w = ua:rotate(w)"),
        // Two statements: a call nested as the last argument is a multiple-results call, and
        // the outer call then has a dynamic argument count, so neither site would be lowered.
        ("lowered Q:key + Q:keyRotation (native)", "local k = Q:key(s, 3) s = Q:keyRotation(k)"),
        ("binder quat.key + quat.keyRotation (native)", "local k = quat.key(s, 3) s = quat.keyRotation(k)"),
    ];
    let mut group = c.benchmark_group("packed_quat_lowered");
    group.throughput(Throughput::Elements(CALLS));
    for (name, body) in cases {
        let template = sandbox
            .load_template(
                &runtime,
                "bench.lua",
                &format!(
                    "--!native\nlocal Q: dream_quat_Math = quat.math()\n\
                     local a = quat.axisAngle(vector.create(0, 0, 1), 0.3) local b = quat.axisAngle(vector.create(1, 0, 0), 0.7)\n\
                     local ua = quatud.axisAngle(vector.create(0, 0, 1), 0.3) local ub = quatud.axisAngle(vector.create(1, 0, 0), 0.7)\n\
                     local v = vector.create(1, 2, 3)\n\
                     return function() local s, us, w = a, ua, v for i = 1, {CALLS} do {body} end return 0 end"
                ),
            )
            .unwrap();
        let results = sandbox.run(&runtime, &template, &instance, CallContext { id: 1, category: MemoryCategory(0) }).unwrap();
        let function = Function::from_value(results.into_iter().next().unwrap()).unwrap();
        let stack = runtime.stack();
        group.bench_function(name, |b| b.iter(|| function.invoke::<f64, _>(&stack, ()).unwrap()));
    }
    group.finish();
}

#[cfg(not(feature = "jit"))]
fn lowered(_: &mut Criterion) {}

criterion_group! { name = benches; config = configure(); targets = raw, through_luau, lowered }
criterion_main!(benches);
