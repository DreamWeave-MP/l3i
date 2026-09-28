//! Instruction and cycle counts per bound call, from the CPU's own counters
//! (`perf_event_open` on this process, user space only, no `perf` binary needed).
//!
//! Wall time hides in noise past a few nanoseconds; retired instructions do not. Each scenario
//! runs a Luau loop of `CALLS` iterations, the loop itself is measured with an empty body and
//! subtracted, and the minimum over `ROUNDS` repeats is reported per call. Run with
//! `cargo bench --bench instructions`; Linux only, and it needs
//! `/proc/sys/kernel/perf_event_paranoid` at 2 or lower.

#![allow(clippy::cast_precision_loss, clippy::cast_possible_truncation, clippy::missing_panics_doc)]

use std::cell::Cell;
use std::ffi::{c_int, c_long, c_ulong, c_void};

use l3i::Runtime;
use l3i::bind::Call;
use l3i::stack::Scope;
use l3i::convert::{Integer, Vector3};
use l3i::direct::field::{DirectField, FieldValue};
use l3i::direct::{self, Atom, DirectAccess, DirectMetamethods};
use l3i::extension::{Extension, ExtensionDescriptor, RuntimePlan, RuntimePolicy, TagPolicy};
use l3i::ffi;
use l3i::packed::Packed;
use l3i::raster::{ClipRect, Color};
use l3i::userdata::{Owned, Userdata};
use l3i::value::Function;
use l3i::Result;

const CALLS: u64 = 100_000;
const ROUNDS: usize = 7;

// ---- perf_event_open, by hand -----------------------------------------------------------------

#[repr(C)]
struct PerfEventAttr {
    kind: u32,
    size: u32,
    config: u64,
    sample_period: u64,
    sample_type: u64,
    read_format: u64,
    flags: u64,
    wakeup_events: u32,
    bp_type: u32,
    bp_addr: u64,
    bp_len: u64,
    branch_sample_type: u64,
    sample_regs_user: u64,
    sample_stack_user: u32,
    clockid: i32,
    sample_regs_intr: u64,
    aux_watermark: u32,
    sample_max_stack: u16,
    reserved_2: u16,
    aux_sample_size: u32,
    reserved_3: u32,
    sig_data: u64,
    config3: u64,
}

unsafe extern "C" {
    fn syscall(number: c_long, ...) -> c_long;
    fn ioctl(fd: c_int, request: c_ulong, ...) -> c_int;
    fn read(fd: c_int, buffer: *mut c_void, count: usize) -> isize;
    fn close(fd: c_int) -> c_int;
}

const SYS_PERF_EVENT_OPEN: c_long = 298;
const PERF_TYPE_HARDWARE: u32 = 0;
const PERF_COUNT_HW_CPU_CYCLES: u64 = 0;
const PERF_COUNT_HW_INSTRUCTIONS: u64 = 1;
const PERF_EVENT_IOC_ENABLE: c_ulong = 0x2400;
const PERF_EVENT_IOC_DISABLE: c_ulong = 0x2401;
const PERF_EVENT_IOC_RESET: c_ulong = 0x2403;
/// `disabled | exclude_kernel | exclude_hv`.
const FLAGS: u64 = 1 | (1 << 5) | (1 << 6);

struct Counter(c_int);

impl Counter {
    fn open(config: u64) -> Option<Counter> {
        let mut attr = PerfEventAttr {
            kind: PERF_TYPE_HARDWARE,
            size: std::mem::size_of::<PerfEventAttr>() as u32,
            config,
            sample_period: 0,
            sample_type: 0,
            read_format: 0,
            flags: FLAGS,
            wakeup_events: 0,
            bp_type: 0,
            bp_addr: 0,
            bp_len: 0,
            branch_sample_type: 0,
            sample_regs_user: 0,
            sample_stack_user: 0,
            clockid: 0,
            sample_regs_intr: 0,
            aux_watermark: 0,
            sample_max_stack: 0,
            reserved_2: 0,
            aux_sample_size: 0,
            reserved_3: 0,
            sig_data: 0,
            config3: 0,
        };
        // SAFETY: a well-formed attribute block for this process and any CPU.
        let fd = unsafe { syscall(SYS_PERF_EVENT_OPEN, &raw mut attr, 0 as c_int, -1 as c_int, -1 as c_int, 0 as c_ulong) };
        (fd >= 0).then_some(Counter(fd as c_int))
    }

    fn measure(&self, body: &mut dyn FnMut()) -> u64 {
        // SAFETY: valid descriptor from `open`; the read buffer is eight bytes.
        unsafe {
            ioctl(self.0, PERF_EVENT_IOC_RESET, 0);
            ioctl(self.0, PERF_EVENT_IOC_ENABLE, 0);
            body();
            ioctl(self.0, PERF_EVENT_IOC_DISABLE, 0);
            let mut value: u64 = 0;
            let got = read(self.0, (&raw mut value).cast(), 8);
            assert_eq!(got, 8, "perf counter read");
            value
        }
    }
}

impl Drop for Counter {
    fn drop(&mut self) {
        // SAFETY: the descriptor is ours.
        unsafe { close(self.0) };
    }
}

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

struct PlannedExtension;

impl Extension for PlannedExtension {
    fn id(&self) -> &'static str {
        "dream.bench"
    }

    fn describe(&self, d: &mut ExtensionDescriptor) -> Result<()> {
        let mut planned = d.userdata::<Planned>("dream.bench.Planned");
        planned.tag(TagPolicy::Required);
        planned.method("get", |p: &Planned| p.value.get());
        planned.method("addTwo", |p: &Planned, a: f64, b: f64| p.value.get() + a + b);
        planned.getter("value", |p: &Planned| p.value.get());
        planned.field::<PlannedValue>("field");
        d.module("@dream/bench")
            .function("new", |v: f64| Owned(Planned { value: Cell::new(v) }))
            .function("zero", || 7.0f64)
            .function("two", |a: f64, b: f64| a + b)
            .function("vec", |v: Vector3| f64::from(v.x))
            .function("packed", |c: Packed<Color>| f64::from(c.0.r))
            .function("integer", |i: Integer| i.0 as f64)
            .function("four", |a: Vector3, b: Vector3, c: Packed<Color>, d: Packed<ClipRect>| {
                f64::from(a.x + b.y) + f64::from(c.0.r) + f64::from(d.0.max_x)
            });
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
    runtime.stack().with_frame(|frame| {
        l3i::userdata::tagged::push(frame, Direct { value: Cell::new(7.0) })?;
        frame.set_global("direct")
    }).unwrap();
    runtime.stack().with_frame(|frame| {
        // SAFETY: a plain C function with a static debug name.
        unsafe { frame.push_c_function(raw_add, c"dream.bench.rawAdd".as_ptr()) };
        frame.set_global("raw_add")
    }).unwrap();
    runtime.set_global("color", &Integer(Color::WHITE.pack().bits())).unwrap();
    runtime.set_global("clip", &Integer(ClipRect::ALL.pack().bits())).unwrap();
    runtime.exec("planned = bench.new(7) hoisted = vector.create(1, 2, 3)").unwrap();
    runtime
}

fn looped(runtime: &Runtime, body: &str) -> Function {
    runtime
        .load_function(&format!(
            "return function() local bench, planned, hoisted, color, clip, raw_add, direct = bench, planned, hoisted, color, clip, raw_add, direct \
             local s = 0 for i = 1, {CALLS} do {body} end return s end"
        ))
        .unwrap()
}

fn main() {
    let (Some(instructions), Some(cycles)) =
        (Counter::open(PERF_COUNT_HW_INSTRUCTIONS), Counter::open(PERF_COUNT_HW_CPU_CYCLES))
    else {
        eprintln!("perf counters unavailable (perf_event_paranoid > 2, or not Linux); nothing measured");
        return;
    };
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
    ];
    // `L3I_SCENARIO=<name>` runs one scenario for `L3I_ROUNDS` rounds (a profiling workload).
    let only = std::env::var("L3I_SCENARIO").ok();
    let rounds: usize = std::env::var("L3I_ROUNDS").ok().and_then(|r| r.parse().ok()).unwrap_or(ROUNDS);
    let mut baseline = (0u64, 0u64);
    println!("{:<40} {:>14} {:>12}", "scenario", "instructions", "cycles");
    for (name, body) in scenarios {
        if only.as_deref().is_some_and(|only| only != *name && *name != "loop only") {
            continue;
        }
        let function = looped(&runtime, body);
        let stack = runtime.stack();
        let mut best = (u64::MAX, u64::MAX);
        for _ in 0..rounds {
            let mut run = || {
                function.invoke::<f64, _>(&stack, ()).unwrap();
            };
            let i = instructions.measure(&mut run);
            let c = cycles.measure(&mut run);
            best = (best.0.min(i), best.1.min(c));
        }
        if *name == "loop only" {
            baseline = best;
            println!("{name:<40} {:>14.1} {:>12.1}   (per iteration, subtracted below)", best.0 as f64 / CALLS as f64, best.1 as f64 / CALLS as f64);
            continue;
        }
        let per_call = |total: u64, base: u64| total.saturating_sub(base) as f64 / CALLS as f64;
        println!("{name:<40} {:>14.1} {:>12.1}", per_call(best.0, baseline.0), per_call(best.1, baseline.1));
    }
}
