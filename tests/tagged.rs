//! The handoff's first vertical slice: register one tagged type with a method, create
//! instances from Lua, drop them, collect, and observe exactly one Rust drop per instance.

use std::cell::Cell;
use std::ffi::c_int;

use dream_binder::ffi;
use dream_binder::userdata::{Userdata, tagged};
use dream_binder::{Error, Runtime, TAG_LIMIT};

thread_local! {
    static CONSTRUCTED: Cell<usize> = const { Cell::new(0) };
    static DROPPED: Cell<usize> = const { Cell::new(0) };
}

struct Probe {
    value: f64,
}

impl Probe {
    fn new(value: f64) -> Probe {
        CONSTRUCTED.with(|c| c.set(c.get() + 1));
        Probe { value }
    }
}

impl Drop for Probe {
    fn drop(&mut self) {
        DROPPED.with(|d| d.set(d.get() + 1));
    }
}

unsafe impl Userdata for Probe {
    const TAG: Option<u8> = Some(7);
    const NAME: &'static str = "dreamweave.test.Probe";
}

struct Impostor;

unsafe impl Userdata for Impostor {
    const TAG: Option<u8> = Some(7);
    const NAME: &'static str = "dreamweave.test.Impostor";
}

struct SameNameOtherTag;

unsafe impl Userdata for SameNameOtherTag {
    const TAG: Option<u8> = Some(8);
    const NAME: &'static str = "dreamweave.test.Probe";
}

struct ZeroTag;

unsafe impl Userdata for ZeroTag {
    const TAG: Option<u8> = Some(0);
    const NAME: &'static str = "dreamweave.test.ZeroTag";
}

struct LimitTag;

unsafe impl Userdata for LimitTag {
    const TAG: Option<u8> = Some(TAG_LIMIT);
    const NAME: &'static str = "dreamweave.test.LimitTag";
}

unsafe extern "C-unwind" fn make_probe(state: *mut ffi::lua_State) -> c_int {
    unsafe {
        dream_binder::native::enter(state, |stack| {
            let value = ffi::lua_tonumber(state, 1);
            tagged::push(stack, Probe::new(value))?;
            Ok(1)
        })
    }
}

fn runtime_with_probe() -> Runtime {
    let runtime = Runtime::new().unwrap();
    tagged::register::<Probe>(&runtime, |ty| ty.method("value", |probe: &Probe| probe.value)).unwrap();
    {
        let stack = runtime.stack();
        let frame = stack.frame();
        unsafe { frame.push_c_function(make_probe, std::ptr::null()) };
        frame.set_global("make_probe").unwrap();
    }
    runtime
}

#[test]
fn instances_are_created_used_and_dropped_exactly_once() {
    let runtime = runtime_with_probe();
    runtime
        .exec(
            "local p = make_probe(7) \
             assert(p:value() == 7, 'method') \
             assert(p.value(p) == 7, 'plain index') \
             assert(typeof(p) == 'dreamweave.test.Probe', typeof(p)) \
             assert(getmetatable(p) == false, 'protected metatable') \
             for i = 1, 9 do make_probe(i) end",
        )
        .unwrap();
    assert_eq!(CONSTRUCTED.with(Cell::get), 10);
    runtime.collect_garbage();
    runtime.collect_garbage();
    assert_eq!(DROPPED.with(Cell::get), 10, "every unreachable instance dropped exactly once");
    assert_eq!(runtime.stack().top(), 0);
}

#[test]
fn wrong_receiver_is_a_luau_type_error() {
    let runtime = runtime_with_probe();
    let error = runtime.exec("local p = make_probe(1) p.value(42)").unwrap_err();
    assert!(
        error.to_string().ends_with(
            "invalid argument #1 to 'dreamweave.test.Probe.value' (dreamweave.test.Probe expected, got number)"
        ),
        "{error}"
    );
    let error = runtime.exec("local p = make_probe(1) p.value()").unwrap_err();
    assert!(
        error
            .to_string()
            .ends_with("missing argument #1 to 'dreamweave.test.Probe.value' (dreamweave.test.Probe expected)"),
        "{error}"
    );
}

#[test]
fn registration_is_idempotent_for_the_same_type_and_rejects_conflicts() {
    let runtime = runtime_with_probe();
    tagged::register::<Probe>(&runtime, |_| Ok(())).unwrap();

    let conflict = tagged::register::<Impostor>(&runtime, |_| Ok(())).unwrap_err();
    assert_eq!(
        conflict,
        Error::logic("Conflicting Luau userdata tag registration: tag 7 is already 'dreamweave.test.Probe'")
    );
    let conflict = tagged::register::<SameNameOtherTag>(&runtime, |_| Ok(())).unwrap_err();
    assert_eq!(conflict, Error::logic("Conflicting Luau userdata name registration: 'dreamweave.test.Probe'"));
    assert!(tagged::register::<ZeroTag>(&runtime, |_| Ok(())).is_err());
    assert!(tagged::register::<LimitTag>(&runtime, |_| Ok(())).is_err());
    assert_eq!(runtime.stack().top(), 0);
}

#[test]
fn a_failing_configure_unpublishes_the_metatable() {
    let runtime = Runtime::new().unwrap();
    let error = tagged::register::<Probe>(&runtime, |_| Err(Error::logic("nope"))).unwrap_err();
    assert_eq!(error, Error::logic("nope"));
    assert!(!tagged::is_registered::<Probe>(&runtime.stack()));
    // The name is free again, so a second attempt succeeds.
    tagged::register::<Probe>(&runtime, |_| Ok(())).unwrap();
    assert!(tagged::is_registered::<Probe>(&runtime.stack()));
}

#[test]
fn pushing_an_unregistered_type_is_a_logic_error() {
    let runtime = Runtime::new().unwrap();
    let error = tagged::push(&runtime.stack(), Probe::new(1.0)).unwrap_err();
    assert_eq!(error, Error::logic("Luau tagged userdata type 'dreamweave.test.Probe' is not registered"));
}
