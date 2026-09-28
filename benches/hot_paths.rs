//! The hot paths PLAN.md §6 commits to, measured (see BENCHMARKS.md for the numbers).
//!
//! Luau→Rust paths run a Lua loop of `CALLS` iterations per sample so the per-call cost is the
//! reported time divided by `CALLS` (Criterion's throughput shows it as elements/second).

use std::cell::Cell;
use std::ffi::c_int;
use std::time::Duration;

use criterion::{BatchSize, Criterion, Throughput, criterion_group, criterion_main};
use l3i::bind::Call;
use l3i::convert::Vector3;
use l3i::direct::field::{DirectField, FieldValue};
use l3i::direct::{self, AccessKind, Atom, AtomCatalogue, DirectAccess, DirectMetamethods, Dispatch};
use l3i::ffi;
use l3i::stack::Scope;
use l3i::userdata::{Userdata, receiver, tagged, untagged};
use l3i::value::{Function, Table, Value};
use l3i::{Result, Runtime};

const CALLS: u64 = 1000;
const GET_ATOM: Atom = 1024;
const VALUE_ATOM: Atom = 1025;
/// Index and namecall only: the benchmark type has no setters, so there is no `__newindex` to wrap.
const DIRECT: DirectMetamethods = DirectMetamethods { index: true, newindex: false, namecall: true };

struct Tagged {
    value: Cell<f64>,
}

unsafe impl Userdata for Tagged {
    const NAME: &'static str = "dreamweave.bench.Tagged";
}

impl DirectAccess for Tagged {
    fn direct_index(call: &Call<'_>, data: &Tagged, atom: Atom, _: &mut u16) -> Result<Dispatch> {
        if atom == VALUE_ATOM {
            call.push(&data.value.get())?;
            return Ok(Dispatch::Handled);
        }
        Ok(Dispatch::Fallback)
    }

    fn direct_namecall(call: &Call<'_>, data: &Tagged, atom: Atom, _: &mut u16) -> Result<Option<c_int>> {
        if atom == GET_ATOM {
            call.push(&data.value.get())?;
            return Ok(Some(1));
        }
        Ok(None)
    }
}

struct TaggedX;

impl DirectField<Tagged> for TaggedX {
    fn get(value: &Tagged) -> FieldValue {
        FieldValue::Number(value.value.get())
    }
}

struct Plain {
    value: Cell<f64>,
}

unsafe impl Userdata for Plain {
    const NAME: &'static str = "dreamweave.bench.Plain";
}

struct Untagged {
    value: Cell<f64>,
}

unsafe impl Userdata for Untagged {
    const NAME: &'static str = "dreamweave.bench.Untagged";
}

struct Sequence;

unsafe impl Userdata for Sequence {
    const NAME: &'static str = "dreamweave.bench.Sequence";
}

unsafe extern "C-unwind" fn raw_add(state: *mut ffi::lua_State) -> c_int {
    unsafe {
        ffi::lua_pushnumber(state, ffi::lua_tonumber(state, 1) + ffi::lua_tonumber(state, 2));
        1
    }
}

fn runtime() -> Runtime {
    let catalogue = AtomCatalogue::try_new([("get", GET_ATOM), ("value", VALUE_ATOM)]).unwrap();
    let runtime = Runtime::builder().atom_catalogue(catalogue).build().unwrap();
    // Tagged with generated dispatch wrapped by direct callbacks, plus a direct field.
    tagged::register::<Tagged>(&runtime, 40, |ty| {
        ty.property("value", |t: &Tagged| t.value.get())?;
        ty.method("get", |t: &Tagged| t.value.get())?;
        ty.direct_dispatch::<Tagged>(DIRECT)
    })
    .unwrap();
    direct::register::<Tagged>(&runtime, DIRECT).unwrap();
    direct::field::register::<Tagged, TaggedX>(&runtime, "x").unwrap();
    // Tagged with only the generated metamethods.
    tagged::register::<Plain>(&runtime, 41, |ty| {
        ty.property("value", |p: &Plain| p.value.get())?;
        ty.method("get", |p: &Plain| p.value.get())
    })
    .unwrap();
    untagged::register::<Untagged>(&runtime, |ty| {
        ty.property("value", |u: &Untagged| u.value.get())?;
        ty.method("get", |u: &Untagged| u.value.get())
    })
    .unwrap();
    tagged::register::<Sequence>(&runtime, 42, |ty| {
        ty.array_iterator(|_: &Sequence, index: f64| -> Option<(f64, f64)> {
            if index >= 100.0 { None } else { Some((index + 1.0, index)) }
        })
    })
    .unwrap();
    {
        let stack = runtime.stack();
        let frame = stack.frame();
        tagged::push(&frame, Tagged { value: Cell::new(7.0) }).unwrap();
        frame.set_global("direct").unwrap();
        tagged::push(&frame, Plain { value: Cell::new(7.0) }).unwrap();
        frame.set_global("plain").unwrap();
        untagged::push(&frame, Untagged { value: Cell::new(7.0) }).unwrap();
        frame.set_global("untagged").unwrap();
        tagged::push(&frame, Sequence).unwrap();
        frame.set_global("sequence").unwrap();
        unsafe { frame.push_c_function(raw_add, c"dreamweave.bench.rawAdd".as_ptr()) };
        frame.set_global("raw_add").unwrap();
    }
    let typed = runtime.bind_function("dreamweave.bench.typedAdd", |a: f64, b: f64| a + b).unwrap();
    runtime.set_global("typed_add", &typed).unwrap();
    let counter = std::rc::Rc::new(Cell::new(0u64));
    let captured = runtime
        .bind_function("dreamweave.bench.captured", move || {
            counter.set(counter.get() + 1);
            counter.get()
        })
        .unwrap();
    runtime.set_global("captured", &captured).unwrap();
    let vector_x = runtime.bind_function("dreamweave.bench.vectorX", |v: Vector3| v.x).unwrap();
    runtime.set_global("vector_x", &vector_x).unwrap();
    runtime.exec("plain_table = { value = 7 }").unwrap();
    runtime
}

/// A Lua function running `body` `CALLS` times.
fn looped(runtime: &Runtime, body: &str) -> Function {
    // Globals are copied into locals first: `global.field` would otherwise compile to a folded
    // GETIMPORT, which resolves through the ordinary metamethod path rather than the direct
    // access and direct field fast paths (as in OpenMW).
    runtime
        .load_function(&format!(
            "return function() local direct, plain, untagged, sequence, plain_table = direct, plain, untagged, sequence, plain_table \
             local s = 0 for i = 1, {CALLS} do {body} end return s end"
        ))
        .unwrap()
}

fn bench_loop(c: &mut Criterion, group: &str, cases: &[(&str, &str)]) {
    let runtime = runtime();
    let mut group = c.benchmark_group(group);
    group.throughput(Throughput::Elements(CALLS));
    for (name, body) in cases {
        let function = looped(&runtime, body);
        let stack = runtime.stack();
        group.bench_function(*name, |b| b.iter(|| function.invoke::<f64, _>(&stack, ()).unwrap()));
    }
    group.finish();
}

fn rust_to_luau(c: &mut Criterion) {
    let runtime = runtime();
    let add = runtime.load_function("return function(a, b) return a + b end").unwrap();
    let identity = runtime.load_function("return function(t) return t end").unwrap();
    let table = Table::new(&runtime.stack(), 0, 0).unwrap();
    let stack = runtime.stack();
    let mut group = c.benchmark_group("rust_to_luau_call");
    group.bench_function("scalar (f64, f64) -> f64", |b| b.iter(|| add.invoke::<f64, _>(&stack, (1.0, 2.0)).unwrap()));
    group.bench_function("table argument, view result", |b| {
        b.iter(|| identity.invoke_with(&stack, (&table,), |_, view| Ok(view.is_table())).unwrap())
    });
    group.bench_function("table argument, pinned result", |b| {
        b.iter(|| identity.invoke::<Table, _>(&stack, (&table,)).unwrap())
    });
    group.finish();
}

fn luau_to_rust(c: &mut Criterion) {
    bench_loop(
        c,
        "luau_to_rust_call",
        &[
            ("typed binder (f64, f64) -> f64", "s = typed_add(i, 1)"),
            ("hand-written lua_CFunction", "s = raw_add(i, 1)"),
            ("captured Rust context", "s = captured()"),
            ("Vector3 ingress", "s = vector_x(vector.create(i, 2, 3))"),
        ],
    );
}

fn methods_and_properties(c: &mut Criterion) {
    bench_loop(
        c,
        "method_call",
        &[
            ("tagged generated __namecall", "s = plain:get()"),
            ("tagged direct namecall", "s = direct:get()"),
            ("untagged generated __namecall", "s = untagged:get()"),
        ],
    );
    bench_loop(
        c,
        "property_get",
        &[
            ("plain table field", "s = plain_table.value"),
            ("tagged generated __index", "s = plain.value"),
            ("tagged direct index", "s = direct.value"),
            ("tagged direct field", "s = direct.x"),
            ("untagged generated __index", "s = untagged.value"),
        ],
    );
    bench_loop(c, "iterator", &[("array __iter, 100 elements", "for _, v in sequence do s = v end")]);
}

fn host_side(c: &mut Criterion) {
    let runtime = runtime();
    runtime.exec("fields = { value = 7 }").unwrap();
    let fields = Table::from_value(runtime.global("fields").unwrap()).unwrap();
    let direct = runtime.global("direct").unwrap();
    let untagged = runtime.global("untagged").unwrap();
    let stack = runtime.stack();
    let mut group = c.benchmark_group("host_side");
    group.bench_function("borrowed table field read", |b| {
        b.iter(|| {
            stack
                .with_frame(|frame| {
                    let view = fields.push_to(frame)?;
                    view.get_as::<f64>(frame, "value")
                })
                .unwrap()
        })
    });
    group.bench_function("owned table field read", |b| b.iter(|| fields.get::<f64>(&stack, "value").unwrap()));
    group.bench_function("tagged receiver check", |b| {
        b.iter(|| {
            stack
                .with_frame(|frame| {
                    let view = direct.push_to(frame)?;
                    Ok(receiver::<Tagged>(view).is_some())
                })
                .unwrap()
        })
    });
    group.bench_function("untagged receiver check", |b| {
        b.iter(|| {
            stack
                .with_frame(|frame| {
                    let view = untagged.push_to(frame)?;
                    Ok(receiver::<Untagged>(view).is_some())
                })
                .unwrap()
        })
    });
    group.bench_function("value pin create and drop", |b| {
        b.iter_batched(|| (), |()| Value::new_table(&stack, 0, 0).unwrap(), BatchSize::SmallInput)
    });
    group.finish();
}

fn configure() -> Criterion {
    Criterion::default().measurement_time(Duration::from_secs(2)).warm_up_time(Duration::from_secs(1))
}

criterion_group! { name = benches; config = configure(); targets = rust_to_luau, luau_to_rust, methods_and_properties, host_side, plan_dispatch, typed_variants }
criterion_main!(benches);

// ---- Runtime-resolved plans: cached direct index and namecall at several plan sizes ---------

struct Wide {
    value: Cell<f64>,
}

unsafe impl Userdata for Wide {
    const NAME: &'static str = "dreamweave.bench.Wide";
}

impl DirectAccess for Wide {
    fn direct_index(call: &Call<'_>, data: &Wide, atom: Atom, slot: &mut u16) -> Result<Dispatch> {
        let Some(plan) = direct::plan::plan(call) else { return Ok(Dispatch::Fallback) };
        if plan.resolve_cached_slot::<Wide>(call, slot, atom, AccessKind::Index) == 0 {
            return Ok(Dispatch::Fallback);
        }
        call.push(&data.value.get())?;
        Ok(Dispatch::Handled)
    }

    fn direct_namecall(call: &Call<'_>, data: &Wide, atom: Atom, slot: &mut u16) -> Result<Option<c_int>> {
        let Some(plan) = direct::plan::plan(call) else { return Ok(None) };
        if plan.resolve_cached_slot::<Wide>(call, slot, atom, AccessKind::Namecall) == 0 {
            return Ok(None);
        }
        call.push(&data.value.get())?;
        Ok(Some(1))
    }
}

/// A runtime whose plan maps `members` index slots (`m0..`) and `members` namecall slots
/// (`c0..`) on `Wide`.
fn plan_runtime(members: usize) -> Runtime {
    let fields: Vec<String> = (0..members).map(|i| format!("m{i}")).collect();
    let methods: Vec<String> = (0..members).map(|i| format!("c{i}")).collect();
    let catalogue = AtomCatalogue::try_new(
        fields.iter().chain(methods.iter()).enumerate().map(|(i, n)| (n.clone(), 1000 + i as Atom)),
    )
    .unwrap();
    let runtime = Runtime::builder().atom_catalogue(catalogue).build().unwrap();
    const DIRECT: DirectMetamethods = DirectMetamethods { index: true, newindex: false, namecall: true };
    tagged::register::<Wide>(&runtime, 45, |ty| {
        ty.property("m0", |w: &Wide| w.value.get())?;
        ty.method("c0", |w: &Wide| w.value.get())?;
        ty.direct_dispatch::<Wide>(DIRECT)
    })
    .unwrap();
    direct::register::<Wide>(&runtime, DIRECT).unwrap();
    let mut builder = direct::plan::DirectPlanBuilder::new(&runtime);
    for (i, name) in fields.iter().enumerate() {
        builder = builder.slot::<Wide>(AccessKind::Index, name, (1 + i) as u16).unwrap();
    }
    for (i, name) in methods.iter().enumerate() {
        builder = builder.slot::<Wide>(AccessKind::Namecall, name, (1 + members + i) as u16).unwrap();
    }
    builder.finish().unwrap();
    {
        let stack = runtime.stack();
        let frame = stack.frame();
        tagged::push(&frame, Wide { value: Cell::new(7.0) }).unwrap();
        frame.set_global("wide").unwrap();
    }
    runtime
}

fn plan_dispatch(c: &mut Criterion) {
    let mut group = c.benchmark_group("plan_dispatch");
    group.throughput(Throughput::Elements(CALLS));
    for members in [4usize, 32, 128] {
        let runtime = plan_runtime(members);
        // The last member: a hit path that never depends on the plan's size.
        let last = members - 1;
        let index = runtime
            .load_function(&format!(
                "return function() local w = wide local s = 0 for i = 1, {CALLS} do s = w.m{last} end return s end"
            ))
            .unwrap();
        let namecall = runtime
            .load_function(&format!(
                "return function() local w = wide local s = 0 for i = 1, {CALLS} do s = w:c{last}() end return s end"
            ))
            .unwrap();
        let stack = runtime.stack();
        group.bench_function(format!("cached direct index, {members} members"), |b| {
            b.iter(|| index.invoke::<f64, _>(&stack, ()).unwrap())
        });
        group.bench_function(format!("cached direct namecall, {members} members"), |b| {
            b.iter(|| namecall.invoke::<f64, _>(&stack, ()).unwrap())
        });
    }
    group.finish();
}

// ---- Diagnostic: where the typed binder's per-argument cost lives -------------------------

fn typed_variants(c: &mut Criterion) {
    let runtime = Runtime::new().unwrap();
    let zero = runtime.bind_function("dreamweave.bench.zero", || 1.0f64).unwrap();
    let call_only = runtime.bind_function("dreamweave.bench.callOnly", |_: &Call| 1.0f64).unwrap();
    let one = runtime.bind_function("dreamweave.bench.one", |a: f64| a).unwrap();
    let view = runtime.bind_function("dreamweave.bench.view", |_: l3i::stack::ValueView| 1.0f64).unwrap();
    let two = runtime.bind_function("dreamweave.bench.two", |a: f64, b: f64| a + b).unwrap();
    let two_i32 = runtime.bind_function("dreamweave.bench.twoInt", |a: i32, b: i32| a + b).unwrap();
    let unit = runtime.bind_function("dreamweave.bench.unit", |_a: f64| ()).unwrap();
    for (name, function) in [
        ("zero", &zero),
        ("call_only", &call_only),
        ("one", &one),
        ("view", &view),
        ("two", &two),
        ("two_i32", &two_i32),
        ("unit", &unit),
    ] {
        runtime.set_global(name, function).unwrap();
    }
    let mut group = c.benchmark_group("typed_variants");
    group.throughput(Throughput::Elements(CALLS));
    for (name, body) in [
        ("() -> f64", "s = zero()"),
        ("(&Call) -> f64", "s = call_only()"),
        ("(f64) -> f64", "s = one(i)"),
        ("(ValueView) -> f64", "s = view(i)"),
        ("(f64, f64) -> f64", "s = two(i, 1)"),
        ("(i32, i32) -> i32", "s = two_i32(i, 1)"),
        ("(f64) -> ()", "unit(i)"),
    ] {
        let function = looped(&runtime, body);
        let stack = runtime.stack();
        group.bench_function(name, |b| b.iter(|| function.invoke::<f64, _>(&stack, ()).unwrap()));
    }
    group.finish();
}
