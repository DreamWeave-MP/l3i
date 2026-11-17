//! What a packed argument costs a bound call on the carrier this build uses, against a number and
//! a vector argument, two ways:
//!
//! - `record_the_bound_call_cost_per_argument_kind` prints wall time per call. wasm32 under Node
//!   has no CPU counters, so the wasm32 job runs it and its log is the record #32 asks for
//!   ("boxed wasm recorded only"). It asserts nothing about speed.
//! - `budget_scenario` runs one scenario a given number of times for the budgets job, which counts
//!   its instructions under Cachegrind at two call counts, here and on the Luau 0.746 integer
//!   carrier, and holds every binder row to #32's 2 percent. Cachegrind counts in software, so
//!   the gate runs on hosted runners without perf counters, on x86-64 and AArch64.
//!
//! The file compiles unchanged on the last commit with the integer carrier, which the budgets job
//! builds it into for the paired baseline.

use l3i::Runtime;
use l3i::convert::{Bits64, Vector3};
use l3i::packed::Packed;
use l3i::raster::Color;

const CALLS: u32 = 200_000;
const ROUNDS: usize = 5;

/// The scenarios: a name the budgets job uses, a label, and the loop body.
const SCENARIOS: [(&str, &str, &str); 6] = [
    ("loop", "the loop alone", "s = i"),
    ("number", "bound (f64) -> f64", "s = take_number(i)"),
    ("vector", "bound (Vector3) -> f64", "s = take_vector(v)"),
    ("color", "bound (Packed<Color>) -> f64", "s = take_color(c)"),
    ("bits", "bound (Bits64) -> f64", "s = take_bits(b)"),
    ("make_color", "binder returns a packed Color", "c = make_color()"),
];

fn runtime() -> Runtime {
    let runtime = Runtime::new().unwrap();
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
/// the budgets job to count. Everything but the loop is the same at every call count, so the
/// difference between two counts is the loop's own instructions.
#[test]
#[ignore = "an instruction budget scenario: the budgets job runs it under Cachegrind"]
fn budget_scenario() {
    let wanted = std::env::var("L3I_BUDGET_SCENARIO").expect("L3I_BUDGET_SCENARIO names a scenario");
    let calls: u32 = std::env::var("L3I_BUDGET_CALLS").expect("L3I_BUDGET_CALLS").parse().expect("a call count");
    let (_, _, body) = SCENARIOS.iter().find(|(name, _, _)| *name == wanted).expect("a known scenario");
    let runtime = runtime();
    let f = looped(&runtime, body, calls);
    f.invoke::<f64, _>(&runtime.stack(), ()).unwrap();
}
