//! Packed quaternion experiment: an integer64 rotation against an f32 quaternion userdata,
//! both through the binder, plus the raw encode/decode cost.

#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::semicolon_if_nothing_returned,
    clippy::missing_panics_doc
)]

#[path = "../tests/support/packed_quat.rs"]
mod support;

use std::time::Duration;

use std::hint::black_box;

use criterion::{Criterion, Throughput, criterion_group, criterion_main};
use l3i::Runtime;
use l3i::extension::{RuntimePlan, RuntimePolicy};
use support::{PackedRotation, QuatExtension, random_quat};

const CALLS: u64 = 1000;

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
                black_box(pair[0].mul(pair[1]));
            }
        })
    });
    group.bench_function("decode, mul, encode", |b| {
        b.iter(|| {
            for pair in packed.windows(2) {
                black_box(PackedRotation::encode(pair[0].decode().mul(pair[1].decode())));
            }
        })
    });
    group.finish();
}

fn through_luau(c: &mut Criterion) {
    let plan = RuntimePlan::builder()
        .policy(RuntimePolicy::new().compat_global("@dream/quat", "quat"))
        .extension(QuatExtension)
        .finalize()
        .unwrap();
    let runtime = Runtime::from_plan(&plan).unwrap();
    runtime
        .exec(
            "a = quat.axisAngle(vector.create(0, 0, 1), 0.3) b = quat.axisAngle(vector.create(1, 0, 0), 0.7) \
             ua = quat.udAxisAngle(vector.create(0, 0, 1), 0.3) ub = quat.udAxisAngle(vector.create(1, 0, 0), 0.7) \
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

criterion_group! { name = benches; config = configure(); targets = raw, through_luau }
criterion_main!(benches);
