//! Ported from `testluaudirectaccess.cpp` and `testshareduserdata.cpp`: the direct callbacks
//! shadow the metamethods for atom-keyed access, fall back to the originals otherwise, and
//! Luau's per-instruction cache is validated before it is trusted. Direct fields bypass both.

use std::cell::Cell;
use std::ffi::c_int;

use l3i::bind::Call;
use l3i::convert::Vector3;
use l3i::direct::field::{DirectField, FieldValue};
use l3i::direct::registry::{Descriptor, Registry, UNKNOWN_SLOT};
use l3i::direct::{self, AccessKind, Atom, AtomCatalogue, DirectAccess, DirectMetamethods, Dispatch};
use l3i::stack::Scope;
use l3i::userdata::{Userdata, tagged};
use l3i::{Error, Result, Runtime};

const FOO_TAG: u8 = 50;
const BAR_TAG: u8 = 51;

// A stand-in application catalogue: "get"/"set"/"name" as atoms, one "name" Index slot on two
// tags so the cache can be poisoned across tags.
const ATOMS: [(&str, Atom); 3] = [("get", 1024), ("set", 1025), ("name", 1026)];
fn catalogue() -> AtomCatalogue {
    AtomCatalogue::from_static(&ATOMS).unwrap()
}
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
    Runtime::builder().atom_catalogue(catalogue()).build().unwrap()
}

/// Registers Foo with Lua closure metamethods (so fallbacks are observable), wraps them, and
/// registers the direct callbacks.
fn install_foo(runtime: &Runtime) {
    let index = runtime.load_function("return function(v, k) return 'closure-' .. tostring(k) end").unwrap();
    let namecall = runtime.load_function("return function(self) return 'closure-call' end").unwrap();
    let newindex = runtime.load_function("return function(v, k, val) error('closure-newindex', 0) end").unwrap();
    tagged::register::<Foo>(runtime, FOO_TAG, |ty| {
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
fn atom_catalogues_are_per_vm_and_resolve_every_spelling() {
    let runtime = runtime_with_atoms();
    let installed = runtime.atom_catalogue().expect("installed at build");
    assert_eq!(installed.atom_of("get"), Some(1024));
    assert_eq!(installed.name_of(1026), Some("name"));
    assert_eq!(installed.len(), 3);
    assert_eq!(runtime.atom_of("set"), Some(1025));
    {
        let stack = runtime.stack();
        let frame = stack.frame();
        assert_eq!(direct::atom_of_view(frame.push_string("set")), Some(1025));
        assert_eq!(direct::atom_of_view(frame.push_string("nothing")), None);
    }
    // A second runtime carries its own catalogue; the two never see each other's atoms.
    let other = Runtime::builder().standard_libraries(false).build().unwrap();
    direct::install_atom_callback(&other, AtomCatalogue::try_new([("zzz", 9000), ("name", 5)]).unwrap()).unwrap();
    other.open_standard_libraries();
    {
        let stack = other.stack();
        let frame = stack.frame();
        assert_eq!(direct::atom_of_view(frame.push_string("zzz")), Some(9000));
        assert_eq!(direct::atom_of_view(frame.push_string("name")), Some(5));
        assert_eq!(direct::atom_of_view(frame.push_string("get")), None);
    }
    {
        let stack = runtime.stack();
        let frame = stack.frame();
        assert_eq!(direct::atom_of_view(frame.push_string("name")), Some(1026));
        assert_eq!(direct::atom_of_view(frame.push_string("zzz")), None);
    }
    // One catalogue per VM.
    assert!(direct::install_atom_callback(&other, catalogue()).is_err());
    // A runtime without a catalogue resolves nothing and offers no atoms.
    let bare = Runtime::new().unwrap();
    assert!(bare.atom_catalogue().is_none());
    assert_eq!(bare.atom_of("get"), None);
    assert!(AtomCatalogue::try_new([("a", 1), ("a", 2)]).is_err());
    assert!(AtomCatalogue::try_new([("a", 1), ("b", 1)]).is_err());
    assert!(AtomCatalogue::try_new([("a", -1)]).is_err());
    assert!(AtomCatalogue::try_new([("", 1)]).is_err());
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
    tagged::register::<Bar>(&runtime, BAR_TAG, |ty| {
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
    assert_eq!(error, Error::logic("'dreamweave.tests.Foo' is not tagged in this runtime; direct access needs a tag"));
    tagged::register::<Foo>(&runtime, FOO_TAG, |ty| ty.method("plain", |f: &Foo| f.value.get())).unwrap();
    let error = direct::register::<Foo>(&runtime, DirectMetamethods::INDEX).unwrap_err();
    assert!(error.to_string().contains("no corresponding wrapper metamethod"), "{error}");
    assert!(direct::register::<Foo>(&runtime, DirectMetamethods::default()).is_err());
    // Wrapping requires an original metamethod to exist.
    let error = tagged::register::<Bar>(&runtime, BAR_TAG, |ty| ty.direct_dispatch::<Bar>(DirectMetamethods::NAMECALL))
        .unwrap_err();
    assert!(error.to_string().contains("requires an original __namecall metamethod"), "{error}");
}

// ---- Runtime-resolved plans: the same handler code, different tags per VM ------------------

struct Planned {
    value: Cell<f64>,
}

unsafe impl Userdata for Planned {
    const NAME: &'static str = "dreamweave.tests.Planned";
}

const PLANNED_VALUE_GET: u16 = 1;
const PLANNED_VALUE_SET: u16 = 2;

impl DirectAccess for Planned {
    fn direct_index(call: &Call<'_>, data: &Planned, atom: Atom, slot: &mut u16) -> Result<Dispatch> {
        let Some(plan) = direct::plan::plan(call) else { return Ok(Dispatch::Fallback) };
        match plan.resolve_cached_slot::<Planned>(call, slot, atom, AccessKind::Index) {
            PLANNED_VALUE_GET => {
                call.push(&data.value.get())?;
                Ok(Dispatch::Handled)
            }
            _ => Ok(Dispatch::Fallback),
        }
    }

    fn direct_newindex(call: &Call<'_>, data: &Planned, atom: Atom, slot: &mut u16) -> Result<Dispatch> {
        let Some(plan) = direct::plan::plan(call) else { return Ok(Dispatch::Fallback) };
        match plan.resolve_cached_slot::<Planned>(call, slot, atom, AccessKind::NewIndex) {
            PLANNED_VALUE_SET => {
                data.value.set(call.arg(3).read::<f64>()?);
                Ok(Dispatch::Handled)
            }
            _ => Ok(Dispatch::Fallback),
        }
    }
}

fn planned_runtime(tag: u8, atom_base: Atom) -> Runtime {
    let catalogue = AtomCatalogue::try_new([("value", atom_base), ("name", atom_base + 1)]).unwrap();
    let runtime = Runtime::builder().atom_catalogue(catalogue).build().unwrap();
    let index = runtime.load_function("return function(v, k) return 'fallback-' .. tostring(k) end").unwrap();
    let newindex =
        runtime.load_function("return function(v, k, val) error('read-only ' .. tostring(k), 0) end").unwrap();
    tagged::register::<Planned>(&runtime, tag, |ty| {
        ty.metamethod_value("__index", index.value())?;
        ty.metamethod_value("__newindex", newindex.value())?;
        ty.direct_dispatch::<Planned>(DirectMetamethods { index: true, newindex: true, namecall: false })
    })
    .unwrap();
    direct::register::<Planned>(&runtime, DirectMetamethods { index: true, newindex: true, namecall: false }).unwrap();
    direct::plan::DirectPlanBuilder::new(&runtime)
        .slot::<Planned>(AccessKind::Index, "value", PLANNED_VALUE_GET)
        .unwrap()
        .slot::<Planned>(AccessKind::NewIndex, "value", PLANNED_VALUE_SET)
        .unwrap()
        .finish()
        .unwrap();
    {
        let stack = runtime.stack();
        let frame = stack.frame();
        tagged::push(&frame, Planned { value: Cell::new(1.5) }).unwrap();
        frame.set_global("p").unwrap();
    }
    runtime
}

#[test]
fn runtime_plans_resolve_slots_from_the_vms_own_tags_and_atoms() {
    // Two runtimes, two tags, two atom numberings, one handler.
    let a = planned_runtime(60, 2000);
    let b = planned_runtime(61, 3000);
    for runtime in [&a, &b] {
        runtime
            .exec("local u = p assert(u.value == 1.5) u.value = 4 assert(u.value == 4) assert(u.other == 'fallback-other') \
                   local ok, err = pcall(function() u.other = 1 end) assert(not ok and err == 'read-only other')")
            .unwrap();
    }
    let plan = direct::plan::plan(&a.stack()).unwrap();
    assert_eq!(plan.entries().len(), 2);
    assert_eq!(plan.resolve_slot(60, 2000, AccessKind::Index), PLANNED_VALUE_GET);
    assert_eq!(plan.resolve_slot(61, 2000, AccessKind::Index), UNKNOWN_SLOT, "tag 61 is another VM's");
    let plan_b = direct::plan::plan(&b.stack()).unwrap();
    assert_eq!(plan_b.resolve_slot(61, 3000, AccessKind::Index), PLANNED_VALUE_GET);
    // Builder validation.
    let error = direct::plan::DirectPlanBuilder::new(&a).slot::<Planned>(AccessKind::Index, "missing", 5).unwrap_err();
    assert!(error.to_string().contains("not in this runtime's atom catalogue"), "{error}");
    let error = direct::plan::DirectPlanBuilder::new(&a).slot::<Bar>(AccessKind::Index, "value", 5).unwrap_err();
    assert!(error.to_string().contains("not tagged in this runtime"), "{error}");
    let error = direct::plan::DirectPlanBuilder::new(&a)
        .slot::<Planned>(AccessKind::Index, "value", 7)
        .unwrap()
        .slot::<Planned>(AccessKind::Index, "name", 7)
        .unwrap_err();
    assert!(error.to_string().contains("used twice"), "{error}");
    assert!(direct::plan::DirectPlanBuilder::new(&a).slot::<Planned>(AccessKind::Index, "value", 0).is_err());
    // Slots and atom spans must stay dense: the table is dense.
    let error = direct::plan::DirectPlanBuilder::new(&a).slot::<Planned>(AccessKind::Index, "value", 9000).unwrap_err();
    assert!(error.to_string().contains("allocate slots densely"), "{error}");
    let sparse = Runtime::builder()
        .atom_catalogue(AtomCatalogue::try_new([("value", 1), ("far", 30000)]).unwrap())
        .build()
        .unwrap();
    tagged::register::<Planned>(&sparse, 60, |ty| ty.property("value", |p: &Planned| p.value.get())).unwrap();
    let error = direct::plan::DirectPlanBuilder::new(&sparse)
        .slot::<Planned>(AccessKind::Index, "value", 1)
        .unwrap()
        .slot::<Planned>(AccessKind::Index, "far", 2)
        .unwrap()
        .finish()
        .unwrap_err();
    assert!(error.to_string().contains("catalogue them densely"), "{error}");

    // A published plan is immutable: the runtime refuses a second one.
    let sealed = Runtime::builder().atom_catalogue(AtomCatalogue::try_new([("value", 1)]).unwrap()).build().unwrap();
    tagged::register::<Planned>(&sealed, 60, |ty| ty.property("value", |p: &Planned| p.value.get())).unwrap();
    direct::plan::DirectPlanBuilder::new(&sealed).slot::<Planned>(AccessKind::Index, "value", 1).unwrap().finish().unwrap();
    let error =
        direct::plan::DirectPlanBuilder::new(&sealed).slot::<Planned>(AccessKind::Index, "value", 2).unwrap().finish().unwrap_err();
    assert_eq!(error, Error::logic("This runtime already has a direct plan; plans are published once per VM"));
    assert_eq!(direct::plan::plan(&sealed.stack()).unwrap().resolve_slot(60, 1, AccessKind::Index), 1);
}
