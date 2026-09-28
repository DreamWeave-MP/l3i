//! Color arithmetic through the module functions (interpreter) and the `raster.math()`
//! receiver lowered to native code.

#![allow(clippy::semicolon_if_nothing_returned, clippy::missing_panics_doc)]

use std::time::Duration;

use criterion::{Criterion, Throughput, criterion_group, criterion_main};
use l3i::Runtime;
use l3i::extension::{RuntimePlan, RuntimePolicy};
use l3i::raster::RasterExtension;

const CALLS: u64 = 1000;

fn module_functions(c: &mut Criterion) {
    let plan = RuntimePlan::builder()
        .policy(RuntimePolicy::new().compat_global("@dream/raster", "raster"))
        .extension(RasterExtension)
        .finalize()
        .unwrap();
    let runtime = Runtime::from_plan(&plan).unwrap();
    let cases = [
        ("rgba8", "c = raster.rgba8(i % 256, 40, 40, 255)"),
        ("lerp", "c = raster.lerp(c, w, 0.25)"),
        ("mul", "c = raster.mul(c, w)"),
        ("premultiply", "c = raster.premultiply(c)"),
    ];
    let mut group = c.benchmark_group("raster_module");
    group.throughput(Throughput::Elements(CALLS));
    for (name, body) in cases {
        let function = runtime
            .load_function(&format!(
                "return function() local raster = raster local c, w = raster.rgba8(10, 20, 30, 200), raster.rgba8(200, 100, 50, 128) \
                 for i = 1, {CALLS} do {body} end return 0 end"
            ))
            .unwrap();
        let stack = runtime.stack();
        group.bench_function(name, |b| b.iter(|| function.invoke::<f64, _>(&stack, ()).unwrap()));
    }
    group.finish();
}

#[cfg(feature = "jit")]
fn lowered(c: &mut Criterion) {
    use l3i::extension::NativeCodePolicy;
    use l3i::native_code::NativeCodeMode;
    use l3i::runtime::{CallContext, MemoryCategory};
    use l3i::sandbox::{InstanceSpec, SandboxOptions};
    use l3i::value::Function;

    let policy = RuntimePolicy::new()
        .compat_global("@dream/raster", "raster")
        .native_code(NativeCodePolicy { mode: NativeCodeMode::Eager, ..NativeCodePolicy::default() });
    let plan = RuntimePlan::builder().policy(policy).extension(RasterExtension).finalize().unwrap();
    let runtime = Runtime::from_plan(&plan).unwrap();
    if !runtime.native_code().is_some_and(l3i::native_code::NativeCodeGen::is_available) {
        return;
    }
    let sandbox = runtime
        .sandbox(|_| {}, SandboxOptions { compile_options: runtime.compile_options(), ..SandboxOptions::default() })
        .unwrap();
    let loader = runtime.load_function("return function(name) error('module ' .. name .. ' not found') end").unwrap();
    let instance = sandbox
        .new_instance(&runtime, &InstanceSpec { name: "c", packages: &[], hidden_data: None, loader: &loader })
        .unwrap();
    let cases = [
        ("rgba8 (native lowered)", "c = M:rgba8(i % 256, 40, 40, 255)"),
        ("lerp (native lowered)", "c = M:lerp(c, w, 0.25)"),
        ("mul (native lowered)", "c = M:mul(c, w)"),
        ("premultiply (native lowered)", "c = M:premultiply(c)"),
        ("rgba8 (native, module)", "c = raster.rgba8(i % 256, 40, 40, 255)"),
        ("lerp (native, module)", "c = raster.lerp(c, w, 0.25)"),
    ];
    let mut group = c.benchmark_group("raster_lowered");
    group.throughput(Throughput::Elements(CALLS));
    for (name, body) in cases {
        let template = sandbox
            .load_template(
                &runtime,
                "bench.lua",
                &format!(
                    "--!native\nlocal M: dream_raster_Math = raster.math()\n\
                     local w = raster.rgba8(200, 100, 50, 128)\n\
                     return function() local c = raster.rgba8(10, 20, 30, 200) for i = 1, {CALLS} do {body} end return 0 end"
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

fn configure() -> Criterion {
    Criterion::default().warm_up_time(Duration::from_secs(1)).measurement_time(Duration::from_secs(3))
}

criterion_group! { name = benches; config = configure(); targets = module_functions, lowered }
criterion_main!(benches);
