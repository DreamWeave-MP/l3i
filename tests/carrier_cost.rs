//! What a packed argument costs a bound call on the carrier this build uses, against a number and
//! a vector argument, two ways:
//!
//! - `record_the_bound_call_cost_per_argument_kind` prints wall time per call. wasm32 under Node
//!   has no CPU counters, so the wasm32 job runs it and its log is the record #32 asks for
//!   ("boxed wasm recorded only"). It asserts nothing about speed.
//! - `budget_scenario` and `native_budget_scenario` run one scenario a given number of times, for
//!   Cachegrind to count at two call counts, here and on the Luau 0.746 baselines.
//!   `CARRIER_BUDGETS.md` has the commands and the measurements.
//!
//! The file compiles unchanged on the Luau 0.746 integer and light carrier commits, so the same
//! scenarios run on all three builds.

use l3i::Runtime;
use l3i::convert::{Bits64, Vector3};
use l3i::packed::Packed;
use l3i::raster::Color;

const CALLS: u32 = 200_000;
const ROUNDS: usize = 5;

/// The scenarios: a name for `L3I_BUDGET_SCENARIO`, a label, and the loop body.
const SCENARIOS: [(&str, &str, &str); 7] = [
    ("loop", "the loop alone", "s = i"),
    // Luau's own call, no binder: what a pin moves in the call path, apart from l3i.
    ("raw", "raw lua_CFunction (f64) -> f64", "s = raw_number(i)"),
    ("number", "bound (f64) -> f64", "s = take_number(i)"),
    ("vector", "bound (Vector3) -> f64", "s = take_vector(v)"),
    ("color", "bound (Packed<Color>) -> f64", "s = take_color(c)"),
    ("bits", "bound (Bits64) -> f64", "s = take_bits(b)"),
    ("make_color", "binder returns a packed Color", "c = make_color()"),
];

unsafe extern "C-unwind" fn raw_number(state: *mut l3i::ffi::lua_State) -> std::ffi::c_int {
    // SAFETY: a plain C function Luau calls with one number.
    unsafe {
        l3i::ffi::lua_pushnumber(state, l3i::ffi::lua_tonumber(state, 1));
        1
    }
}

fn runtime() -> Runtime {
    let runtime = Runtime::new().unwrap();
    runtime
        .stack()
        .with_frame(|frame| {
            // SAFETY: a plain C function with a static debug name.
            unsafe { frame.push_c_function(raw_number, c"dreamweave.cost.raw".as_ptr()).unwrap() };
            frame.set_global("raw_number")
        })
        .unwrap();
    let global = |name: &str, f: l3i::value::Function| runtime.set_global(name, &f).unwrap();
    global("take_number", runtime.bind_function("dreamweave.cost.number", |x: f64| x).unwrap());
    global("take_vector", runtime.bind_function("dreamweave.cost.vector", |v: Vector3| f64::from(v.x)).unwrap());
    global("take_color", runtime.bind_function("dreamweave.cost.color", |c: Packed<Color>| f64::from(c.0.r)).unwrap());
    global(
        "take_bits",
        runtime.bind_function("dreamweave.cost.bits", |b: Bits64| f64::from((b.0 & 0xFFFF) as u16)).unwrap(),
    );
    // Made by the binder, so each value rides in whatever carrier this build uses.
    global(
        "make_color",
        runtime.bind_function("dreamweave.cost.make_color", || Packed(Color { r: 7, g: 8, b: 9, a: 10 })).unwrap(),
    );
    global("make_bits", runtime.bind_function("dreamweave.cost.make_bits", || Bits64(0x1234_5678_9ABC_DEF0)).unwrap());
    runtime.exec("c = make_color() b = make_bits() v = vector.create(1, 2, 3)").unwrap();
    runtime
}

fn looped(runtime: &Runtime, body: &str, calls: u32) -> l3i::value::Function {
    runtime
        .load_function(&format!("return function() local s = 0 for i = 1, {calls} do {body} end return s end"))
        .unwrap()
}

#[test]
#[ignore = "a timing record: run with --ignored --nocapture"]
fn record_the_bound_call_cost_per_argument_kind() {
    let runtime = runtime();
    let time = |body: &str| -> f64 {
        let f = looped(&runtime, body, CALLS);
        let stack = runtime.stack();
        (0..ROUNDS)
            .map(|_| {
                let start = std::time::Instant::now();
                f.invoke::<f64, _>(&stack, ()).unwrap();
                start.elapsed().as_secs_f64() * 1e9 / f64::from(CALLS)
            })
            .fold(f64::INFINITY, f64::min)
    };
    let loop_only = time("s = i");
    println!("carrier: {}, arch: {}", l3i::packed::carrier::CARRIER, std::env::consts::ARCH);
    for (_, label, body) in &SCENARIOS[1..] {
        println!("{label:32} {:8.1} ns/call (loop subtracted, min of {ROUNDS} x {CALLS})", time(body) - loop_only);
    }
}

/// One scenario (`L3I_BUDGET_SCENARIO`, a name from `SCENARIOS`) run `L3I_BUDGET_CALLS` times, for
/// Cachegrind to count (`CARRIER_BUDGETS.md`). Everything but the loop is the same at every call count, so the
/// difference between two counts is the loop's own instructions.
#[test]
#[ignore = "an instruction budget scenario, counted under Cachegrind: see CARRIER_BUDGETS.md"]
fn budget_scenario() {
    let wanted = std::env::var("L3I_BUDGET_SCENARIO").expect("L3I_BUDGET_SCENARIO names a scenario");
    let calls: u32 = std::env::var("L3I_BUDGET_CALLS").expect("L3I_BUDGET_CALLS").parse().expect("a call count");
    let (_, _, body) = SCENARIOS.iter().find(|(name, _, _)| *name == wanted).expect("a known scenario");
    let runtime = runtime();
    let f = looped(&runtime, body, calls);
    f.invoke::<f64, _>(&runtime.stack(), ()).unwrap();
}

/// The lowered calls #32 budgets at 3 percent, in natively compiled code, for Cachegrind: a
/// name, then the loop body. `x` varies per iteration so nothing is hoisted, as in
/// `benches/instructions`; `loop` is the bare native loop.
#[cfg(feature = "jit")]
const NATIVE_SCENARIOS: [(&str, &str); 5] = [
    ("native_loop", "s = x"),
    ("native_fromxyzw", "s = Q:fromXYZW(x, 0.2, 0.3, 0.9)"),
    ("native_rotate", "s = Q:rotate(r, p)"),
    ("native_mul_local", "s = Q:mul(r, r)"),
    ("native_rgba8", "s = C:rgba8(i % 256, 40, 40, 255)"),
];

/// One native scenario (`L3I_BUDGET_SCENARIO`) run `L3I_BUDGET_CALLS` times inside `--!native`
/// code with the lowered receivers, for Cachegrind to count.
#[cfg(feature = "jit")]
#[test]
#[ignore = "an instruction budget scenario, counted under Cachegrind: see CARRIER_BUDGETS.md"]
fn native_budget_scenario() {
    use l3i::extension::{NativeCodePolicy, RuntimePlan, RuntimePolicy};
    use l3i::native_code::NativeCodeMode;
    use l3i::quat::QuatExtension;
    use l3i::raster::RasterExtension;
    use l3i::runtime::{CallContext, MemoryCategory};
    use l3i::sandbox::{InstanceSpec, SandboxOptions};

    let wanted = std::env::var("L3I_BUDGET_SCENARIO").expect("L3I_BUDGET_SCENARIO names a scenario");
    let calls: u32 = std::env::var("L3I_BUDGET_CALLS").expect("L3I_BUDGET_CALLS").parse().expect("a call count");
    let (_, body) = NATIVE_SCENARIOS.iter().find(|(name, _)| *name == wanted).expect("a known native scenario");
    let policy = RuntimePolicy::new()
        .compat_global("@dream/quat", "quat")
        .compat_global("@dream/raster", "raster")
        .native_code(NativeCodePolicy { mode: NativeCodeMode::Eager, ..NativeCodePolicy::default() });
    let plan =
        RuntimePlan::builder().policy(policy).extension(QuatExtension).extension(RasterExtension).finalize().unwrap();
    let runtime = Runtime::from_plan(&plan).unwrap();
    assert!(
        runtime.native_code().is_some_and(l3i::native_code::NativeCodeGen::is_available),
        "native scenarios run where Luau generates native code"
    );
    let sandbox = runtime
        .sandbox(|_| {}, SandboxOptions { compile_options: runtime.compile_options(), ..SandboxOptions::default() })
        .unwrap();
    let loader = runtime.load_function("return function(name) error('module ' .. name .. ' not found') end").unwrap();
    let instance = sandbox
        .new_instance(&runtime, &InstanceSpec { name: "n", packages: &[], hidden_data: None, loader: &loader })
        .unwrap();
    let template = sandbox
        .load_template(
            &runtime,
            "native.lua",
            &format!(
                "--!native\nlocal Q: dream_quat_Math = quat.math()\nlocal C: dream_raster_Math = raster.math()\n\
                 local r = quat.axisAngle(vector.create(0, 0, 1), 0.3)\nlocal p = vector.create(1, 2, 3)\nlocal sink\n\
                 return function() local s = 0 for i = 1, {calls} do local x = 0.1 + (i % 64) * 0.01 {body} end sink = s return 0 end"
            ),
        )
        .unwrap();
    let results =
        sandbox.run(&runtime, &template, &instance, CallContext { id: 1, category: MemoryCategory(0) }).unwrap();
    let f = l3i::value::Function::from_value(results.into_iter().next().unwrap()).unwrap();
    f.invoke::<f64, _>(&runtime.stack(), ()).unwrap();
}
