//! Ported from `testluauregistration.cpp` (untagged registration, storage, receivers).

use std::cell::Cell;
use std::ptr::NonNull;

use dream_binder::stack::Scope;
use dream_binder::userdata::{StableRef, Userdata, untagged};
use dream_binder::{Error, Runtime};

thread_local!(static DROPPED: Cell<usize> = const { Cell::new(0) });

struct Untagged {
    value: i32,
}

impl Drop for Untagged {
    fn drop(&mut self) {
        DROPPED.with(|d| d.set(d.get() + 1));
    }
}

unsafe impl Userdata for Untagged {
    const NAME: &'static str = "dreamweave.tests.Untagged";
    const TAG: Option<u8> = None;
}

struct Other {
    value: i32,
}

unsafe impl Userdata for Other {
    const NAME: &'static str = "dreamweave.tests.Other";
    const TAG: Option<u8> = None;
}

struct SameName;

unsafe impl Userdata for SameName {
    const NAME: &'static str = "dreamweave.tests.Untagged";
    const TAG: Option<u8> = None;
}

fn register_untagged(runtime: &Runtime) {
    untagged::register::<Untagged>(runtime, |ty| {
        ty.property("value", |u: &Untagged| u.value)?;
        ty.method("getValue", |u: &Untagged| u.value)?;
        ty.property("constValue", |u: &Untagged| u.value)
    })
    .unwrap();
}

#[test]
fn registration_is_transactional_and_strict() {
    let runtime = Runtime::new().unwrap();
    let failed =
        untagged::register::<Untagged>(&runtime, |_| Err(Error::logic("untagged configure failed"))).unwrap_err();
    assert_eq!(failed, Error::logic("untagged configure failed"));
    assert_eq!(runtime.stack().top(), 0);
    {
        let stack = runtime.stack();
        let frame = stack.frame();
        assert!(!untagged::is_registered::<Untagged>(&frame));
    }
    runtime.exec("assert(getmetatable == nil or true)").unwrap();

    register_untagged(&runtime);
    let duplicate = untagged::register::<Untagged>(&runtime, |_| Ok(())).unwrap_err();
    assert_eq!(duplicate, Error::logic("Duplicate untagged userdata metatable: dreamweave.tests.Untagged"));
    let same_name = untagged::register::<SameName>(&runtime, |_| Ok(())).unwrap_err();
    assert_eq!(same_name, Error::logic("Duplicate untagged userdata metatable: dreamweave.tests.Untagged"));
    assert_eq!(runtime.stack().top(), 0);
}

#[test]
fn typed_builder_infers_receiver_and_debug_names() {
    let runtime = Runtime::new().unwrap();
    untagged::register::<Untagged>(&runtime, |ty| {
        ty.property_rw("value", |u: &Untagged| u.value, |_: &Untagged, _: i32| ())?;
        ty.method("getValue", |u: &Untagged| u.value)
    })
    .unwrap();
    {
        let stack = runtime.stack();
        let frame = stack.frame();
        untagged::push(&frame, Untagged { value: 7 }).unwrap();
        frame.set_global("untagged").unwrap();
    }
    runtime.exec("assert(untagged.value == 7 and untagged:getValue() == 7)").unwrap();
    runtime.exec("assert(typeof(untagged) == 'dreamweave.tests.Untagged')").unwrap();

    let error = runtime.exec("untagged.value = 'wrong'").unwrap_err().to_string();
    assert!(error.contains("dreamweave.tests.Untagged.set.value"), "{error}");
    let error = runtime.exec("return untagged:getValue(1)").unwrap_err().to_string();
    assert!(error.contains("dreamweave.tests.Untagged.getValue"), "{error}");
    assert!(error.contains("expected at most 0, got 1"), "{error}");
    let error = runtime.exec("untagged.missing = 1").unwrap_err().to_string();
    assert!(error.contains("dreamweave.tests.Untagged field 'missing' is read-only"), "{error}");
}

#[test]
fn protection_is_restored_and_metatable_frozen() {
    let runtime = Runtime::new().unwrap();
    untagged::register::<Untagged>(&runtime, |_| Ok(())).unwrap();
    {
        let stack = runtime.stack();
        let frame = stack.frame();
        untagged::push(&frame, Untagged { value: 1 }).unwrap();
        frame.set_global("protectedUntagged").unwrap();
    }
    runtime.exec("assert(getmetatable(protectedUntagged) == false)").unwrap();
    let error = runtime.exec("setmetatable(protectedUntagged, {})").unwrap_err().to_string();
    assert!(error.contains("table expected"), "{error}");
}

#[test]
fn receivers_respect_owned_and_borrowed_storage_and_exact_identity() {
    let runtime = Runtime::new().unwrap();
    register_untagged(&runtime);
    untagged::register::<Other>(&runtime, |ty| ty.property("value", |o: &Other| o.value)).unwrap();

    let mut engine_owned = Untagged { value: 11 };
    {
        let stack = runtime.stack();
        let frame = stack.frame();
        untagged::push(&frame, Untagged { value: 7 }).unwrap();
        frame.set_global("owned").unwrap();
        // SAFETY (test): `engine_owned` outlives the runtime and is destroyed after it.
        let borrowed = unsafe { StableRef::new(NonNull::from(&mut engine_owned)) };
        untagged::push_borrowed(&frame, borrowed).unwrap();
        frame.set_global("borrowed").unwrap();
        untagged::push(&frame, Other { value: 3 }).unwrap();
        frame.set_global("other").unwrap();

        let view = frame.push(&()).unwrap();
        assert!(untagged::test::<Untagged>(view).is_none());
    }
    runtime.exec("assert(owned.value == 7 and owned:getValue() == 7 and owned.constValue == 7)").unwrap();
    runtime.exec("assert(borrowed.constValue == 11 and borrowed:getValue() == 11)").unwrap();
    runtime.exec("assert(other.value == 3)").unwrap();

    // The wrong receiver type is rejected with the expected type's name.
    let error = runtime.exec("local get = getmetatable == nil return owned.getValue(other)").unwrap_err().to_string();
    assert!(error.contains("dreamweave.tests.Untagged expected, got dreamweave.tests.Other"), "{error}");

    // Borrowed storage is visible only through the read path.
    {
        let stack = runtime.stack();
        let frame = stack.frame();
        let global = dream_binder::value::Value::get_global(&frame, "borrowed").unwrap().push_to(&frame).unwrap();
        assert_eq!(untagged::test::<Untagged>(global).unwrap().value, 11);
        assert!(untagged::test_owned::<Untagged>(global).is_none());
        let owned = dream_binder::value::Value::get_global(&frame, "owned").unwrap().push_to(&frame).unwrap();
        assert_eq!(untagged::test_owned::<Untagged>(owned).unwrap().value, 7);
    }
    drop(runtime);
    assert_eq!(engine_owned.value, 11, "borrowed storage never dropped the engine object");
    assert_eq!(DROPPED.with(Cell::get), 1, "only the owned instance dropped with the VM");
}

#[test]
fn pushing_an_unregistered_type_is_a_logic_error() {
    let runtime = Runtime::new().unwrap();
    let stack = runtime.stack();
    let frame = stack.frame();
    let error = untagged::push(&frame, Other { value: 1 }).unwrap_err();
    assert_eq!(error, Error::logic("Unknown or writable untagged userdata metatable: dreamweave.tests.Other"));
    assert_eq!(frame.len(), 0);
}

#[test]
fn instances_drop_exactly_once_on_collection() {
    let runtime = Runtime::new().unwrap();
    register_untagged(&runtime);
    let before = DROPPED.with(Cell::get);
    {
        let stack = runtime.stack();
        let frame = stack.frame();
        for i in 0..5 {
            untagged::push(&frame, Untagged { value: i }).unwrap();
        }
    }
    runtime.collect_garbage();
    runtime.collect_garbage();
    assert_eq!(DROPPED.with(Cell::get), before + 5);
}
