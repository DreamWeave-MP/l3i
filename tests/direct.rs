//! Ported from `testluaudirectaccess.cpp` and `testshareduserdata.cpp`: the direct callbacks
//! shadow the metamethods for atom-keyed access, fall back to the originals otherwise, and
//! Luau's per-instruction cache is validated before it is trusted. Direct fields bypass both.

use std::cell::Cell;
use std::ffi::c_int;

use dream_binder::bind::Call;
use dream_binder::convert::Vector3;
use dream_binder::direct::field::{DirectField, FieldValue};
use dream_binder::direct::registry::{Descriptor, Registry, UNKNOWN_SLOT};
use dream_binder::direct::{self, AccessKind, Atom, AtomCatalogue, DirectAccess, DirectMetamethods, Dispatch};
use dream_binder::stack::Scope;
use dream_binder::userdata::{Userdata, tagged};
use dream_binder::{Error, Result, Runtime};

const FOO_TAG: u8 = 50;
const BAR_TAG: u8 = 51;

// A stand-in application catalogue: "get"/"set"/"name" as atoms, one "name" Index slot on two
// tags so the cache can be poisoned across tags.
const ATOMS: [(&str, Atom); 3] = [("get", 1024), ("set", 1025), ("name", 1026)];
const CATALOGUE: AtomCatalogue = AtomCatalogue::validated(&ATOMS);
const REGISTRY: Registry<3, 4> = Registry::new(
    ATOMS,
    [
        Descriptor { slot: 1024, tag: FOO_TAG, atom: 1024, kind: AccessKind::Index },
        Descriptor { slot: 1025, tag: FOO_TAG, atom: 1025, kind: AccessKind::NewIndex },
        Descriptor { slot: 1026, tag: FOO_TAG, atom: 1026, kind: AccessKind::Index },
        Descriptor { slot: 1027, tag: BAR_TAG, atom: 1026, kind: AccessKind::Index },
    ],
);

thread_local! {
    static INDEX_CALLS: Cell<u32> = const { Cell::new(0) };
    static NEWINDEX_CALLS: Cell<u32> = const { Cell::new(0) };
    static NAMECALL_CALLS: Cell<u32> = const { Cell::new(0) };
    static CACHE_HITS: Cell<u32> = const { Cell::new(0) };
    static CACHE_MISSES: Cell<u32> = const { Cell::new(0) };
}

fn bump(counter: &'static std::thread::LocalKey<Cell<u32>>) {
    counter.with(|c| c.set(c.get() + 1));
}

fn count(counter: &'static std::thread::LocalKey<Cell<u32>>) -> u32 {
    counter.with(Cell::get)
}

struct Foo {
    value: Cell<f64>,
}

unsafe impl Userdata for Foo {
    const NAME: &'static str = "dreamweave.tests.Foo";
    const TAG: Option<u8> = Some(FOO_TAG);
}

impl DirectAccess for Foo {
    fn direct_index(call: &Call<'_>, data: &Foo, atom: Atom, slot: &mut u16) -> Result<Dispatch> {
        bump(&INDEX_CALLS);
        let tag = i32::from(FOO_TAG);
        if REGISTRY.cached_slot_matches(*slot, tag, atom, AccessKind::Index) {
            bump(&CACHE_HITS);
        } else {
            bump(&CACHE_MISSES);
        }
        match REGISTRY.resolve_cached_slot(slot, tag, atom, AccessKind::Index) {
            1024 => {
                assert_eq!(call.argument_count(), 2);
                call.push(&"direct-field")?;
                Ok(Dispatch::Handled)
            }
            1026 => {
                call.push(&data.value.get())?;
                Ok(Dispatch::Handled)
            }
            _ => Ok(Dispatch::Fallback),
        }
    }

    fn direct_newindex(call: &Call<'_>, data: &Foo, atom: Atom, slot: &mut u16) -> Result<Dispatch> {
        bump(&NEWINDEX_CALLS);
        match REGISTRY.resolve_cached_slot(slot, i32::from(FOO_TAG), atom, AccessKind::NewIndex) {
            1025 => {
                assert_eq!(call.argument_count(), 3);
                data.value.set(call.arg(3).read::<f64>()?);
                Ok(Dispatch::Handled)
            }
            _ => Ok(Dispatch::Fallback),
        }
    }

    fn direct_namecall(call: &Call<'_>, _: &Foo, atom: Atom, _: &mut u16) -> Result<Option<c_int>> {
        bump(&NAMECALL_CALLS);
        if atom == 1024 {
            assert_eq!(call.argument_count(), 3, "receiver plus two arguments");
            call.push(&"direct-call")?;
            return Ok(Some(1));
        }
        Ok(None)
    }
}

struct Bar;

unsafe impl Userdata for Bar {
    const NAME: &'static str = "dreamweave.tests.Bar";
    const TAG: Option<u8> = Some(BAR_TAG);
}

impl DirectAccess for Bar {
    fn direct_index(call: &Call<'_>, _: &Bar, atom: Atom, slot: &mut u16) -> Result<Dispatch> {
        let tag = i32::from(BAR_TAG);
        if REGISTRY.cached_slot_matches(*slot, tag, atom, AccessKind::Index) {
            bump(&CACHE_HITS);
        } else {
            bump(&CACHE_MISSES);
        }
        match REGISTRY.resolve_cached_slot(slot, tag, atom, AccessKind::Index) {
            1027 => {
                call.push(&"bar-direct")?;
                Ok(Dispatch::Handled)
            }
            _ => Ok(Dispatch::Fallback),
        }
    }
}

struct FooValue;

impl DirectField<Foo> for FooValue {
    fn get(value: &Foo) -> FieldValue {
        FieldValue::Number(value.value.get())
    }
}

struct FooOrigin;

impl DirectField<Foo> for FooOrigin {
    fn get(_: &Foo) -> FieldValue {
        FieldValue::Vector(Vector3::new(1.0, 2.0, 3.0))
    }
}

fn runtime_with_atoms() -> Runtime {
    let runtime = Runtime::builder().standard_libraries(false).build().unwrap();
    direct::install_atom_callback(&runtime, &CATALOGUE).unwrap();
    runtime.open_standard_libraries();
    runtime
}

/// Registers Foo with Lua closure metamethods (so fallbacks are observable), wraps them, and
/// registers the direct callbacks.
fn install_foo(runtime: &Runtime) {
    let index = runtime.load_function("return function(v, k) return 'closure-' .. tostring(k) end").unwrap();
    let namecall = runtime.load_function("return function(self) return 'closure-call' end").unwrap();
    let newindex = runtime.load_function("return function(v, k, val) error('closure-newindex', 0) end").unwrap();
    tagged::register::<Foo>(runtime, |ty| {
        ty.metamethod_value("__index", index.value())?;
        ty.metamethod_value("__namecall", namecall.value())?;
        ty.metamethod_value("__newindex", newindex.value())?;
        ty.direct_dispatch::<Foo>(DirectMetamethods::ALL)
    })
    .unwrap();
    direct::register::<Foo>(runtime, DirectMetamethods::ALL).unwrap();
    let stack = runtime.stack();
    let frame = stack.frame();
    tagged::push(&frame, Foo { value: Cell::new(7.0) }).unwrap();
    frame.set_global("ud").unwrap();
}

#[test]
fn atom_catalogue_installs_once_and_resolves_every_spelling() {
    let runtime = runtime_with_atoms();
    assert_eq!(CATALOGUE.atom_of("get"), Some(1024));
    assert_eq!(CATALOGUE.name_of(1026), Some("name"));
    assert_eq!(direct::installed_catalogue().map(|c| c.entries().len()), Some(3));
    let stack = runtime.stack();
    let frame = stack.frame();
    assert_eq!(direct::atom_of_view(frame.push_string("set")), Some(1025));
    assert_eq!(direct::atom_of_view(frame.push_string("nothing")), None);
    // A second runtime shares the process-wide catalogue.
    let other = runtime_with_atoms();
    let stack = other.stack();
    let frame = stack.frame();
    assert_eq!(direct::atom_of_view(frame.push_string("name")), Some(1026));
    static OTHER: AtomCatalogue = AtomCatalogue::validated(&[("zzz", 9000)]);
    assert!(direct::install_atom_callback(&other, &OTHER).is_err());
    assert!(AtomCatalogue::new(&[("a", 1), ("a", 2)]).is_err());
    assert!(AtomCatalogue::new(&[("a", 1), ("b", 1)]).is_err());
    assert!(AtomCatalogue::new(&[("a", -1)]).is_err());
}

#[test]
fn registry_resolves_and_validates_cached_slots() {
    assert_eq!(REGISTRY.resolve_slot(FOO_TAG.into(), 1024, AccessKind::Index), 1024);
    assert_eq!(REGISTRY.resolve_slot(FOO_TAG.into(), 1024, AccessKind::NewIndex), UNKNOWN_SLOT);
    assert_eq!(REGISTRY.resolve_slot(BAR_TAG.into(), 1026, AccessKind::Index), 1027);
    assert_eq!(REGISTRY.resolve_slot(300, 1026, AccessKind::Index), UNKNOWN_SLOT);
    assert_eq!(REGISTRY.resolve_slot(FOO_TAG.into(), 5, AccessKind::Index), UNKNOWN_SLOT);
    assert!(REGISTRY.cached_slot_matches(1027, BAR_TAG.into(), 1026, AccessKind::Index));
    assert!(!REGISTRY.cached_slot_matches(1027, FOO_TAG.into(), 1026, AccessKind::Index), "cross-tag");
    assert!(!REGISTRY.cached_slot_matches(1025, FOO_TAG.into(), 1025, AccessKind::Index), "wrong kind");
    assert!(!REGISTRY.cached_slot_matches(9999, FOO_TAG.into(), 1024, AccessKind::Index), "stale");
    let mut cached = 1027;
    assert_eq!(REGISTRY.resolve_cached_slot(&mut cached, FOO_TAG.into(), 1026, AccessKind::Index), 1026);
    assert_eq!(cached, 1026);
    assert_eq!(REGISTRY.name_of(1025), "set");
    assert_eq!(REGISTRY.atom_of("name"), Some(1026));
    assert!(REGISTRY.atom_range_matches_slots(FOO_TAG, AccessKind::NewIndex, 1025, 1025));
}

#[test]
fn direct_callbacks_dispatch_and_shadow_metamethods() {
    let runtime = runtime_with_atoms();
    install_foo(&runtime);
    let field = runtime.load_function("return function() local u = ud return u.get end").unwrap();
    assert_eq!(field.invoke::<String, _>(&runtime.stack(), ()).unwrap(), "direct-field");
    assert_eq!(field.invoke::<String, _>(&runtime.stack(), ()).unwrap(), "direct-field");
    let method = runtime.load_function("return function() return ud:get(7, 'argument') end").unwrap();
    assert_eq!(method.invoke::<String, _>(&runtime.stack(), ()).unwrap(), "direct-call");
    runtime.exec("ud.set = 1").unwrap();
    runtime.exec("local u = ud assert(u.name == 1)").unwrap();

    // Unknown keys and non-atom keys fall back to the original closures.
    let unknown = runtime.load_function("return function() local u = ud return u.unknown end").unwrap();
    assert_eq!(unknown.invoke::<String, _>(&runtime.stack(), ()).unwrap(), "closure-unknown");
    runtime.exec("local u = ud assert(u[37] == 'closure-37')").unwrap();
    runtime.exec("local u = ud assert(u:other() == 'closure-call')").unwrap();
    let error = runtime.exec("local u = ud u.other = 2").unwrap_err().to_string();
    assert!(error.contains("closure-newindex"), "{error}");
    let dynamic = runtime.load_function("return function(value, key) value[key] = 1 end").unwrap();
    let ud = runtime.global("ud").unwrap();
    let error = dynamic.invoke::<(), _>(&runtime.stack(), (&ud, "other")).unwrap_err().to_string();
    assert!(error.contains("closure-newindex"), "{error}");

    // The ordinary metamethod path (import-folded global access) runs the same handler.
    assert_eq!(
        runtime
            .load_function("return function() return ud['get'] end")
            .unwrap()
            .invoke::<String, _>(&runtime.stack(), ())
            .unwrap(),
        "direct-field"
    );

    assert!(count(&INDEX_CALLS) >= 3);
    // Keys without an atom never reach the handler: only the atom-keyed `ud.set = 1` counted.
    assert_eq!(count(&NEWINDEX_CALLS), 1);
    // Likewise `u:other()` has no atom and went straight to the closure.
    assert_eq!(count(&NAMECALL_CALLS), 1);
    assert!(count(&CACHE_HITS) >= 1, "the second u.get at the same site hit the cache");
}

#[test]
fn cache_rejects_cross_tag_poisoning_at_a_shared_site() {
    let runtime = runtime_with_atoms();
    install_foo(&runtime);
    let index = runtime.load_function("return function(v, k) return 'bar-closure' end").unwrap();
    tagged::register::<Bar>(&runtime, |ty| {
        ty.metamethod_value("__index", index.value())?;
        ty.direct_dispatch::<Bar>(DirectMetamethods::INDEX)
    })
    .unwrap();
    direct::register::<Bar>(&runtime, DirectMetamethods::INDEX).unwrap();
    {
        let stack = runtime.stack();
        let frame = stack.frame();
        tagged::push(&frame, Bar).unwrap();
        frame.set_global("bar").unwrap();
    }
    let access = runtime.load_function("return function(value) return value.name end").unwrap();
    let foo = runtime.global("ud").unwrap();
    let bar = runtime.global("bar").unwrap();
    let stack = runtime.stack();
    let before_misses = count(&CACHE_MISSES);
    assert_eq!(access.invoke::<f64, _>(&stack, (&foo,)).unwrap(), 7.0);
    assert_eq!(access.invoke::<f64, _>(&stack, (&foo,)).unwrap(), 7.0);
    assert_eq!(access.invoke::<String, _>(&stack, (&bar,)).unwrap(), "bar-direct");
    assert_eq!(access.invoke::<String, _>(&stack, (&bar,)).unwrap(), "bar-direct");
    assert_eq!(access.invoke::<f64, _>(&stack, (&foo,)).unwrap(), 7.0);
    // Foo miss, Foo hit, Bar miss (cross-tag rejection), Bar hit, Foo miss again.
    assert_eq!(count(&CACHE_MISSES) - before_misses, 3);
}

#[test]
fn direct_fields_bypass_the_metatable_entirely() {
    let runtime = runtime_with_atoms();
    install_foo(&runtime);
    direct::field::register::<Foo, FooValue>(&runtime, "value").unwrap();
    direct::field::register::<Foo, FooOrigin>(&runtime, "origin").unwrap();
    let before = count(&INDEX_CALLS);
    runtime
        .exec("local u = ud assert(u.value == 7) u.set = 9 assert(u.value == 9) local o = u.origin assert(o.x == 1 and o.y == 2 and o.z == 3)")
        .unwrap();
    assert_eq!(count(&INDEX_CALLS), before, "direct fields never reach __index");
    assert!(direct::field::register::<Foo, FooValue>(&runtime, "").is_err());
    assert!(
        direct::field::register::<Bar, FooOriginForBar>(&runtime, "x").is_err(),
        "unregistered tag has no metatable"
    );
}

struct FooOriginForBar;

impl DirectField<Bar> for FooOriginForBar {
    fn get(_: &Bar) -> FieldValue {
        FieldValue::Nil
    }
}

#[test]
fn registration_requires_wrappers_and_a_frozen_metatable() {
    let runtime = runtime_with_atoms();
    let error = direct::register::<Foo>(&runtime, DirectMetamethods::ALL).unwrap_err();
    assert_eq!(error, Error::logic("Luau userdata tag has no registered metatable"));
    tagged::register::<Foo>(&runtime, |ty| ty.method("plain", |f: &Foo| f.value.get())).unwrap();
    let error = direct::register::<Foo>(&runtime, DirectMetamethods::INDEX).unwrap_err();
    assert!(error.to_string().contains("no corresponding wrapper metamethod"), "{error}");
    assert!(direct::register::<Foo>(&runtime, DirectMetamethods::default()).is_err());
    // Wrapping requires an original metamethod to exist.
    let error =
        tagged::register::<Bar>(&runtime, |ty| ty.direct_dispatch::<Bar>(DirectMetamethods::NAMECALL)).unwrap_err();
    assert!(error.to_string().contains("requires an original __namecall metamethod"), "{error}");
}
