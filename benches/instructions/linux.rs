//! Instruction, cycle, cache-miss, TLB-miss, and branch-miss counts per bound call, from the CPU's own counters
//! (`perf_event_open` on this process, user space only, no `perf` binary needed).
//!
//! Wall time hides in noise past a few nanoseconds; retired instructions do not. Each scenario
//! runs a Luau loop of `CALLS` iterations, the loop itself is measured with an empty body and
//! subtracted, and the minimum over `ROUNDS` repeats is reported per call. Run with
//! `cargo bench --bench instructions`; Linux only, and it needs
//! `/proc/sys/kernel/perf_event_paranoid` at 2 or lower.

#![allow(clippy::cast_precision_loss, clippy::cast_possible_truncation, clippy::missing_panics_doc)]

use std::cell::Cell;
use std::ffi::c_int;

use l3i::Result;
use l3i::Runtime;
use l3i::bind::Call;
use l3i::convert::{Integer, Vector3};
use l3i::direct::field::{DirectField, FieldValue};
use l3i::direct::{self, Atom, DirectAccess, DirectMetamethods};
use l3i::extension::{Extension, ExtensionDescriptor, RuntimePlan, RuntimePolicy, TagPolicy};
use l3i::ffi;
use l3i::packed::Packed;
use l3i::raster::{ClipRect, Color};
use l3i::stack::Scope;
use l3i::userdata::{Owned, Userdata};
use l3i::value::Function;

use crate::counter::{
    CACHE_READ_MISS, Counter, PERF_COUNT_HW_BRANCH_MISSES, PERF_COUNT_HW_CACHE_DTLB, PERF_COUNT_HW_CACHE_ITLB,
    PERF_COUNT_HW_CACHE_L1D, PERF_COUNT_HW_CACHE_L1I, PERF_COUNT_HW_CACHE_LL, PERF_COUNT_HW_CPU_CYCLES,
    PERF_COUNT_HW_INSTRUCTIONS, PERF_TYPE_HARDWARE, PERF_TYPE_HW_CACHE,
};

const CALLS: u64 = 100_000;
const ROUNDS: usize = 7;

// ---- scenarios ----------------------------------------------------------------------------------

struct Planned {
    value: Cell<f64>,
}

unsafe impl Userdata for Planned {
    const NAME: &'static str = "dream.bench.Planned";
}

struct PlannedValue;

impl DirectField<Planned> for PlannedValue {
    fn get(planned: &Planned) -> FieldValue {
        FieldValue::Number(planned.value.get())
    }
}

struct Nums(Vec<i64>);

impl l3i::sequence::SequenceSource for Nums {
    const NAME: &'static str = "dream.bench.Nums";
    type Item = Integer;
    fn len(&self) -> usize {
        self.0.len()
    }
    fn get(&self, index: usize) -> Option<Self::Item> {
        self.0.get(index).map(|n| Integer(*n))
    }
}

struct PlannedExtension;

impl Extension for PlannedExtension {
    fn id(&self) -> &'static str {
        "dream.bench"
    }

    fn describe(&self, d: &mut ExtensionDescriptor) -> Result<()> {
        let mut planned = d.userdata::<Planned>("dream.bench.Planned");
        planned.tag(TagPolicy::Required);
        planned.method("get", |p: &Planned| p.value.get()).untyped();
        planned.method("addTwo", |p: &Planned, a: f64, b: f64| p.value.get() + a + b).untyped();
        planned.getter("value", |p: &Planned| p.value.get()).untyped();
        planned.field::<PlannedValue>("field").untyped();
        d.sequence::<Nums>("dream.bench.Nums").tag(TagPolicy::Preferred).item_type("integer");
        d.module("@dream/bench")
            .function("new", |v: f64| Owned(Planned { value: Cell::new(v) }))
            .untyped()
            .function("nums", |call: &Call, n: f64| {
                l3i::sequence::Sequence::push(call, Nums((0..n as i64).collect())).map(l3i::value::Value::store)?
            })
            .untyped()
            .untyped()
            .function("zero", || 7.0f64)
            .untyped()
            .function("two", |a: f64, b: f64| a + b)
            .untyped()
            .function("vec", |v: Vector3| f64::from(v.x))
            .untyped()
            .function("packed", |c: Packed<Color>| f64::from(c.0.r))
            .untyped()
            .function("integer", |i: Integer| i.0 as f64)
            .untyped()
            .function("four", |a: Vector3, b: Vector3, c: Packed<Color>, d: Packed<ClipRect>| {
                f64::from(a.x + b.y) + f64::from(c.0.r) + f64::from(d.0.max_x)
            })
            .untyped();
        Ok(())
    }
}

/// The VM floor for a userdata method call: a typed direct handler that pushes a number and
/// nothing else, no binder involved.
struct Direct {
    value: Cell<f64>,
}

unsafe impl Userdata for Direct {
    const NAME: &'static str = "dream.bench.Direct";
}

impl DirectAccess for Direct {
    fn direct_index(call: &Call<'_>, data: &Direct, _: Atom, _: &mut u16) -> Result<direct::Dispatch> {
        call.push(&data.value.get())?;
        Ok(direct::Dispatch::Handled)
    }

    fn direct_namecall(call: &Call<'_>, data: &Direct, _: Atom, _: &mut u16) -> Result<Option<c_int>> {
        call.push(&data.value.get())?;
        Ok(Some(1))
    }
}

const DIRECT: DirectMetamethods = DirectMetamethods { index: true, newindex: false, namecall: true };

unsafe extern "C-unwind" fn raw_add(state: *mut ffi::lua_State) -> c_int {
    // SAFETY: a plain C function called by Luau with two numbers.
    unsafe {
        ffi::lua_pushnumber(state, ffi::lua_tonumber(state, 1) + ffi::lua_tonumber(state, 2));
        1
    }
}

fn runtime() -> Runtime {
    let plan = RuntimePlan::builder()
        .policy(RuntimePolicy::new().compat_global("@dream/bench", "bench"))
        .extension(PlannedExtension)
        .finalize()
        .unwrap();
    let runtime = Runtime::from_plan(&plan).unwrap();
    l3i::userdata::tagged::register::<Direct>(&runtime, 200, |ty| {
        ty.property("value", |d: &Direct| d.value.get())?;
        ty.method("get", |d: &Direct| d.value.get())?;
        ty.direct_dispatch::<Direct>(DIRECT)
    })
    .unwrap();
    direct::register::<Direct>(&runtime, DIRECT).unwrap();
    runtime
        .stack()
        .with_frame(|frame| {
            l3i::userdata::tagged::push(frame, Direct { value: Cell::new(7.0) })?;
            frame.set_global("direct")
        })
        .unwrap();
    runtime
        .stack()
        .with_frame(|frame| {
            // SAFETY: a plain C function with a static debug name.
            unsafe { frame.push_c_function(raw_add, c"dream.bench.rawAdd".as_ptr()) };
            frame.set_global("raw_add")
        })
        .unwrap();
    runtime.set_global("color", &Integer(Color::WHITE.pack().bits().unwrap())).unwrap();
    runtime.set_global("clip", &Integer(ClipRect::ALL.pack().bits().unwrap())).unwrap();
    runtime.exec("planned = bench.new(7) hoisted = vector.create(1, 2, 3) seq = bench.nums(16)").unwrap();
    runtime
}

fn looped(runtime: &Runtime, body: &str) -> Function {
    runtime
        .load_function(&format!(
            "return function() local bench, planned, hoisted, color, clip, raw_add, direct, seq = bench, planned, hoisted, color, clip, raw_add, direct, seq \
             local s = 0 for i = 1, {CALLS} do {body} end return s end"
        ))
        .unwrap()
}

/// Every counter the table reports, in column order. Instructions and cycles are required; a
/// cache or branch counter the CPU does not expose leaves its column blank.
const COLUMNS: &[(&str, u32, u64)] = &[
    ("instructions", PERF_TYPE_HARDWARE, PERF_COUNT_HW_INSTRUCTIONS),
    ("cycles", PERF_TYPE_HARDWARE, PERF_COUNT_HW_CPU_CYCLES),
    ("L1D miss", PERF_TYPE_HW_CACHE, PERF_COUNT_HW_CACHE_L1D | CACHE_READ_MISS),
    ("L1I miss", PERF_TYPE_HW_CACHE, PERF_COUNT_HW_CACHE_L1I | CACHE_READ_MISS),
    ("LLC miss", PERF_TYPE_HW_CACHE, PERF_COUNT_HW_CACHE_LL | CACHE_READ_MISS),
    ("dTLB miss", PERF_TYPE_HW_CACHE, PERF_COUNT_HW_CACHE_DTLB | CACHE_READ_MISS),
    ("iTLB miss", PERF_TYPE_HW_CACHE, PERF_COUNT_HW_CACHE_ITLB | CACHE_READ_MISS),
    ("branch miss", PERF_TYPE_HARDWARE, PERF_COUNT_HW_BRANCH_MISSES),
];

pub fn main() {
    let counters: Vec<Option<Counter>> =
        COLUMNS.iter().map(|(_, kind, config)| Counter::open(*kind, *config)).collect();
    if counters[0].is_none() || counters[1].is_none() {
        eprintln!("perf counters unavailable (perf_event_paranoid > 2, or not Linux); nothing measured");
        return;
    }
    let runtime = runtime();
    let scenarios: &[(&str, &str)] = &[
        ("loop only", "s = i"),
        ("raw lua_CFunction (f64, f64)", "s = raw_add(i, 1)"),
        ("bound () -> f64", "s = bench.zero()"),
        ("bound (f64, f64) -> f64", "s = bench.two(i, 1)"),
        ("bound (Vector3) -> f64", "s = bench.vec(hoisted)"),
        ("bound (Integer) -> f64", "s = bench.integer(color)"),
        ("bound (Packed<Color>) -> f64", "s = bench.packed(color)"),
        ("bound (V, V, Packed, Packed) -> f64", "s = bench.four(hoisted, hoisted, color, clip)"),
        ("typed direct namecall (VM floor)", "s = direct:get()"),
        ("planned method () -> f64", "s = planned:get()"),
        ("planned method (f64, f64) -> f64", "s = planned:addTwo(i, 1)"),
        ("planned getter", "s = planned.value"),
        ("planned direct field", "s = planned.field"),
        ("planned sequence [i]", "s = seq[7]"),
        ("planned sequence #", "s = #seq"),
    ];
    // `L3I_SCENARIO=<name>` runs one scenario for `L3I_ROUNDS` rounds (a profiling workload).
    let only = std::env::var("L3I_SCENARIO").ok();
    let rounds: usize = std::env::var("L3I_ROUNDS").ok().and_then(|r| r.parse().ok()).unwrap_or(ROUNDS);
    let mut baseline = vec![0u64; COLUMNS.len()];
    print!("{:<40}", "scenario");
    for (label, _, _) in COLUMNS {
        print!(" {label:>12}");
    }
    println!();
    for (name, body) in scenarios {
        if only.as_deref().is_some_and(|only| only != *name && *name != "loop only") {
            continue;
        }
        let function = looped(&runtime, body);
        let stack = runtime.stack();
        // The minimum over the rounds for every counter: the steady state, without the round
        // that took an interrupt or a page fault.
        let mut best = vec![u64::MAX; COLUMNS.len()];
        for _ in 0..rounds {
            let mut run = || {
                function.invoke::<f64, _>(&stack, ()).unwrap();
            };
            for (column, counter) in counters.iter().enumerate() {
                if let Some(counter) = counter {
                    best[column] = best[column].min(counter.measure(&mut run));
                }
            }
        }
        let is_baseline = *name == "loop only";
        print!("{name:<40}");
        for (column, counter) in counters.iter().enumerate() {
            if counter.is_none() {
                print!(" {:>12}", "-");
                continue;
            }
            let total = if is_baseline { best[column] } else { best[column].saturating_sub(baseline[column]) };
            let per_call = total as f64 / CALLS as f64;
            if column < 2 {
                print!(" {per_call:>12.1}");
            } else {
                print!(" {per_call:>12.3}");
            }
        }
        if is_baseline {
            baseline = best;
            println!("   (per iteration, subtracted below)");
        } else {
            println!();
        }
    }
}
