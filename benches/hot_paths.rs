//! The hot paths PLAN.md §6 commits to, measured (see BENCHMARKS.md for the numbers).
//!
//! Luau→Rust paths run a Lua loop of `CALLS` iterations per sample so the per-call cost is the
//! reported time divided by `CALLS` (Criterion's throughput shows it as elements/second).

use std::cell::Cell;
use std::ffi::c_int;
use std::time::Duration;

use criterion::{BatchSize, Criterion, Throughput, criterion_group, criterion_main};
use dream_binder::bind::Call;
use dream_binder::convert::Vector3;
use dream_binder::direct::field::{DirectField, FieldValue};
use dream_binder::direct::{self, Atom, AtomCatalogue, DirectAccess, DirectMetamethods, Dispatch};
use dream_binder::ffi;
use dream_binder::stack::Scope;
use dream_binder::userdata::{Userdata, receiver, tagged, untagged};
use dream_binder::value::{Function, Table, Value};
use dream_binder::{Result, Runtime};

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

criterion_group! { name = benches; config = configure(); targets = rust_to_luau, luau_to_rust, methods_and_properties, host_side }
criterion_main!(benches);
