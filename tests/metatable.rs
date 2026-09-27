//! Ported from `testluauregistration.cpp` (builder conflicts, native methods, dispatch,
//! script-visible identity) and `testluaubinding.cpp` (metamethod debug names).

use std::cell::Cell;
use std::ffi::c_int;

use dream_binder::bind::ArgView;
use dream_binder::ffi;
use dream_binder::stack::Scope;
use dream_binder::userdata::{Userdata, tagged};
use dream_binder::{Error, Runtime};

struct Bar {
    value: Cell<i32>,
}

unsafe impl Userdata for Bar {
    const NAME: &'static str = "dreamweave.tests.Bar";
    const TAG: Option<u8> = Some(20);
}

struct Baz;

unsafe impl Userdata for Baz {
    const NAME: &'static str = "dreamweave.tests.Baz";
    const TAG: Option<u8> = Some(21);
}

unsafe extern "C-unwind" fn good_method_body(state: *mut ffi::lua_State) -> c_int {
    unsafe {
        dream_binder::native::enter(state, |stack| {
            let discriminator = stack.at(ffi::lua_upvalueindex(1)).read::<i32>()?;
            let argument = stack.at(2).read::<i32>()?;
            stack.push(&(argument * discriminator))?;
            Ok(1)
        })
    }
}

unsafe extern "C-unwind" fn arg_error_body(state: *mut ffi::lua_State) -> c_int {
    unsafe {
        dream_binder::native::enter(state, |stack| {
            Err(dream_binder::diagnostics::type_error(stack.at(1), "probe"))
        })
    }
}

fn set_global_bar(runtime: &Runtime, name: &str, value: i32) {
    let stack = runtime.stack();
    let frame = stack.frame();
    tagged::push(&frame, Bar { value: Cell::new(value) }).unwrap();
    frame.set_global(name).unwrap();
}

#[test]
fn properties_methods_and_setters_dispatch_through_generated_metamethods() {
    let runtime = Runtime::new().unwrap();
    tagged::register::<Bar>(&runtime, |ty| {
        ty.method("double", |bar: &Bar| bar.value.get() * 2)?;
        ty.property_rw("value", |bar: &Bar| bar.value.get(), |bar: &Bar, value: i32| bar.value.set(value))?;
        ty.property("readonly", |bar: &Bar| bar.value.get() + 100)?;
        ty.method("add", |bar: &Bar, other: &Bar| bar.value.get() + other.value.get())
    })
    .unwrap();
    set_global_bar(&runtime, "bar", 13);
    set_global_bar(&runtime, "other", 4);
    runtime
        .exec(
            "assert(bar.value == 13) assert(bar:double() == 26) assert(bar.readonly == 113) \
             bar.value = 20 assert(bar.value == 20 and bar:double() == 40) \
             assert(bar:add(other) == 24) assert(bar.double(bar) == 40) assert(bar.nothing == nil)",
        )
        .unwrap();
    let error = runtime.exec("bar.readonly = 1").unwrap_err().to_string();
    assert!(error.ends_with("dreamweave.tests.Bar field 'readonly' is read-only"), "{error}");
    let error = runtime.exec("bar[1] = 1").unwrap_err().to_string();
    assert!(error.ends_with("dreamweave.tests.Bar: cannot assign to number key"), "{error}");
    let error = runtime.exec("bar:missing()").unwrap_err().to_string();
    assert!(error.ends_with("attempt to call method 'missing'"), "{error}");
    let error = runtime.exec("bar:add(5)").unwrap_err().to_string();
    assert!(error.contains("dreamweave.tests.Bar.add: bad argument #1 (expected dreamweave.tests.Bar)"), "{error}");
    let error = runtime.exec("bar.double(7)").unwrap_err().to_string();
    assert!(error.contains("invalid argument #1 to 'dreamweave.tests.Bar.double' (dreamweave.tests.Bar expected, got number)"), "{error}");
    let error = runtime.exec("bar.value = 'x'").unwrap_err().to_string();
    assert!(error.contains("dreamweave.tests.Bar.set.value: bad argument #1 (expected number)"), "{error}");
}

#[test]
fn builder_conflict_rules() {
    let runtime = Runtime::new().unwrap();
    // Method first, then explicit __index conflicts.
    let error = tagged::register::<Bar>(&runtime, |ty| {
        ty.method("len", |_: &Bar| 0i32)?;
        ty.raw_metamethod("__index", good_method_body)
    })
    .unwrap_err();
    assert_eq!(error, Error::logic("Metatable already has an explicit __index"));

    // Explicit __index first, then a method conflicts.
    let error = tagged::register::<Bar>(&runtime, |ty| {
        ty.raw_metamethod("__index", good_method_body)?;
        ty.method("len", |_: &Bar| 0i32)
    })
    .unwrap_err();
    assert_eq!(error, Error::logic("Explicit __index conflicts with setMethod"));

    // Native methods after an explicit __index.
    let error = tagged::register::<Bar>(&runtime, |ty| {
        ty.raw_metamethod("__index", good_method_body)?;
        ty.begin_native_methods()
    })
    .unwrap_err();
    assert_eq!(error, Error::logic("Native methods cannot be added after an explicit __index"));

    // Double begin.
    let error = tagged::register::<Bar>(&runtime, |ty| {
        ty.begin_native_methods()?;
        ty.begin_native_methods()
    })
    .unwrap_err();
    assert_eq!(error, Error::logic("Native method registration already started"));

    // Setter then explicit __newindex.
    let error = tagged::register::<Bar>(&runtime, |ty| {
        ty.property_rw("posX", |_: &Bar| 0f32, |_: &Bar, _: f32| ())?;
        ty.raw_metamethod("__newindex", good_method_body)
    })
    .unwrap_err();
    assert_eq!(error, Error::logic("Explicit __newindex conflicts with registered properties"));

    // Generated __namecall cannot be replaced.
    let error = tagged::register::<Bar>(&runtime, |ty| {
        ty.method("getValue", |bar: &Bar| bar.value.get())?;
        ty.property("value", |bar: &Bar| bar.value.get())?;
        ty.raw_metamethod("__namecall", good_method_body)
    })
    .unwrap_err();
    assert_eq!(error, Error::logic("Metatable already has an explicit or generated __namecall"));

    // Duplicate member names, and a setter that takes no value.
    let error = tagged::register::<Bar>(&runtime, |ty| {
        ty.method("value", |bar: &Bar| bar.value.get())?;
        ty.property("value", |bar: &Bar| bar.value.get())
    })
    .unwrap_err();
    assert_eq!(error, Error::logic("dreamweave.tests.Bar.value already registered"));
    let error = tagged::register::<Bar>(&runtime, |ty| ty.property_rw("value", |bar: &Bar| bar.value.get(), |_: &Bar| ())).unwrap_err();
    assert_eq!(error, Error::logic("A property setter must accept one Lua value argument"));

    // A member whose receiver is another type.
    let error = tagged::register::<Bar>(&runtime, |ty| ty.method("wrong", |_: &Baz| 0i32)).unwrap_err();
    assert!(error.to_string().contains("receiverTypeName mismatch for dreamweave.tests.Baz"), "{error}");
    let error = tagged::register::<Bar>(&runtime, |ty| ty.method("wrong", || 0i32)).unwrap_err();
    assert_eq!(error, Error::logic("Member bindings need a receiver as their first parameter"));

    // Every failure rolled back: the type can still be registered cleanly.
    tagged::register::<Bar>(&runtime, |ty| ty.method("ok", |bar: &Bar| bar.value.get())).unwrap();
    assert!(tagged::is_registered::<Bar>(&runtime.stack()));
}

#[test]
fn native_method_table_lifecycle_and_dispatch() {
    let runtime = Runtime::new().unwrap();
    tagged::register::<Baz>(&runtime, |ty| {
        ty.begin_native_methods()?;
        ty.add_native_method("good", good_method_body, 21)?;
        let duplicate = ty.add_native_method("good", good_method_body, 22).unwrap_err();
        assert_eq!(duplicate, Error::logic("Duplicate native method 'good'"));
        let frozen = ty.frozen_native_methods()?;
        assert!(frozen.is_table());
        let late = ty.add_native_method("late", good_method_body, 1).unwrap_err();
        assert!(late.to_string().contains("frozen"), "{late}");
        ty.install_native_method_index()
    })
    .unwrap();
    {
        let stack = runtime.stack();
        let frame = stack.frame();
        tagged::push(&frame, Baz).unwrap();
        frame.set_global("obj").unwrap();
    }
    runtime.exec("assert(obj:good(2) == 42) local m = obj.good assert(m(obj, 4) == 84)").unwrap();
    let error = runtime.exec("return obj:missing()").unwrap_err().to_string();
    assert!(error.contains("attempt to call missing method 'missing'"), "{error}");
}

#[test]
fn metamethod_debug_names_compose_from_the_type() {
    let runtime = Runtime::new().unwrap();
    tagged::register::<Baz>(&runtime, |ty| ty.raw_metamethod("__namecall", arg_error_body)).unwrap();
    {
        let stack = runtime.stack();
        let frame = stack.frame();
        tagged::push(&frame, Baz).unwrap();
        frame.set_global("composed").unwrap();
    }
    // currfuncname reports the closure's own debug name unless it is literally "__namecall".
    let error = runtime.exec("composed:anything()").unwrap_err().to_string();
    assert!(error.contains("invalid argument #1 to 'dreamweave.tests.Baz.__namecall' (probe expected"), "{error}");

    tagged::register::<Bar>(&runtime, |ty| {
        ty.metamethod("__len", |bar: &Bar, _extra: dream_binder::stack::ValueView| bar.value.get())?;
        ty.metamethod("__tostring", |bar: &Bar| format!("Bar({})", bar.value.get()))?;
        // Metamethods bind in function mode: the receiver is argument #1, so a variadic call
        // metamethod takes the rest as an ArgView.
        ty.metamethod("__call", |_: &Bar, rest: ArgView| rest.len() as i32)
    })
    .unwrap();
    set_global_bar(&runtime, "bar", 42);
    runtime.exec("assert(#bar == 42) assert(tostring(bar) == 'Bar(42)') assert(bar(1, 2, 3) == 3) assert(bar() == 0)").unwrap();
    let error = runtime.exec("return #bar + tostring(bar)").unwrap_err().to_string();
    assert!(error.contains("attempt to perform arithmetic"), "{error}");
}

#[test]
fn script_visible_identity_and_protected_metatable() {
    let runtime = Runtime::new().unwrap();
    tagged::register::<Baz>(&runtime, |ty| {
        let stack_value = dream_binder::value::Value::get_global(&runtime.stack(), "_VERSION");
        assert!(stack_value.is_ok());
        ty.set_field("__metatable", &protected_marker(&runtime))
    })
    .unwrap();
    {
        let stack = runtime.stack();
        let frame = stack.frame();
        tagged::push(&frame, Baz).unwrap();
        frame.set_global("obj").unwrap();
    }
    runtime
        .exec("assert(type(obj) == 'userdata') assert(typeof(obj) == 'dreamweave.tests.Baz') assert(getmetatable(obj) == 'Baz protected')")
        .unwrap();
    let error = runtime.exec("setmetatable(obj, {})").unwrap_err().to_string();
    assert!(error.contains("table expected"), "{error}");
}

fn protected_marker(runtime: &Runtime) -> dream_binder::value::Value {
    let stack = runtime.stack();
    let frame = stack.frame();
    dream_binder::value::Value::store(frame.push_string("Baz protected")).unwrap()
}
