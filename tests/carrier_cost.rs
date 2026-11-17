//! A wall-clock record of what a packed argument costs a bound call on the carrier this build
//! uses, against a number and a vector argument. Instruction counts come from CPU counters
//! (`benches/instructions`), which wasm32 under Node does not have, so the wasm32 job runs this
//! with `--ignored --nocapture` and its log is the record #32 asks for ("boxed wasm recorded
//! only"). It measures and prints; it asserts nothing about speed.

use l3i::Runtime;
use l3i::convert::{Bits64, Vector3};
use l3i::packed::Packed;
use l3i::raster::Color;

const CALLS: u32 = 200_000;
const ROUNDS: usize = 5;

#[test]
#[ignore = "a timing record: run with --ignored --nocapture"]
fn record_the_bound_call_cost_per_argument_kind() {
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
    let time = |body: &str| -> f64 {
        let f: l3i::value::Function = runtime
            .load_function(&format!("return function() local s = 0 for i = 1, {CALLS} do {body} end return s end"))
            .unwrap();
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
    for (name, body) in [
        ("bound (f64) -> f64", "s = take_number(i)"),
        ("bound (Vector3) -> f64", "s = take_vector(v)"),
        ("bound (Packed<Color>) -> f64", "s = take_color(c)"),
        ("bound (Bits64) -> f64", "s = take_bits(b)"),
        ("binder returns a packed Color", "c = make_color()"),
    ] {
        println!("{name:32} {:8.1} ns/call (loop subtracted, min of {ROUNDS} x {CALLS})", time(body) - loop_only);
    }
}
