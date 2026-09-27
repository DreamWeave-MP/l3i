//! The component registration contract and read-only packages.

use std::cell::Cell;

use l3i::bind::Call;
use l3i::module::{LuauModule, ModuleBuilder};
use l3i::readonly;
use l3i::userdata::{Userdata, tagged};
use l3i::value::{Table, Value};
use l3i::{Error, Result, Runtime};

struct Asset {
    name: String,
    loads: Cell<u32>,
}

unsafe impl Userdata for Asset {
    const NAME: &'static str = "dreamweave.assets.Asset";
}

struct AssetsModule;

impl LuauModule for AssetsModule {
    const NAME: &'static str = "dreamweave.assets";

    fn register(_runtime: &Runtime, module: &mut ModuleBuilder<'_>) -> Result<()> {
        module.userdata::<Asset>(Some(30), |ty| {
            ty.property("name", |asset: &Asset| asset.name.clone())?;
            ty.method("load", |asset: &Asset| {
                asset.loads.set(asset.loads.get() + 1);
                asset.loads.get()
            })
        })?;
        module.function("open", |call: &Call, name: &str| -> Result<l3i::bind::StackResults> {
            tagged::push(call, Asset { name: name.to_owned(), loads: Cell::new(0) })?;
            Ok(l3i::bind::StackResults)
        })?;
        module.set("VERSION", &3i32)?;
        module.metamethod("__tostring", |_: Table| "dreamweave.assets package")?;
        Ok(())
    }
}

struct SecondModule;

impl LuauModule for SecondModule {
    const NAME: &'static str = "dreamweave.second";

    fn register(_runtime: &Runtime, module: &mut ModuleBuilder<'_>) -> Result<()> {
        module.function("twice", |x: i32| x * 2)?;
        Ok(())
    }
}

#[test]
fn a_host_registers_independent_modules_into_one_vm() {
    let runtime = Runtime::new().unwrap();
    let assets = runtime.register_module::<AssetsModule>().unwrap();
    let second = runtime.register_module::<SecondModule>().unwrap();
    runtime.set_global("assets", &assets).unwrap();
    runtime.set_global("second", &second).unwrap();
    runtime
        .exec(
            "local a = assets.open('tree.nif') assert(a.name == 'tree.nif') assert(a:load() == 1 and a:load() == 2) \
             assert(assets.VERSION == 3) assert(tostring(assets) == 'dreamweave.assets package') \
             assert(second.twice(21) == 42)",
        )
        .unwrap();
    let error = runtime.exec("assets.open(5)").unwrap_err().to_string();
    assert!(error.contains("dreamweave.assets.open: bad argument #1 (expected string)"), "{error}");
    let error = runtime.exec("assets.extra = 1").unwrap_err().to_string();
    assert!(error.contains("attempt to modify a readonly table"), "{error}");
    let error = runtime.exec("second.twice = nil").unwrap_err().to_string();
    assert!(error.contains("attempt to modify a readonly table"), "{error}");
}

#[test]
fn module_paths_must_live_under_the_debug_roots() {
    let runtime = Runtime::new().unwrap();
    assert!(runtime.module("openmw.assets").is_err());
    assert!(runtime.module("dreamweave").is_err());
    assert!(runtime.module("dreamweave.ok").is_ok());
    let openmw = Runtime::builder().debug_roots(&["openmw", "string", "vector"]).build().unwrap();
    assert!(openmw.module("openmw.assets").is_ok());
    assert!(openmw.module("dreamweave.assets").is_err());
}

#[test]
fn read_only_views_iterate_and_measure_the_backing_table_without_exposing_it() {
    let runtime = Runtime::new().unwrap();
    runtime.exec("backing = {a = 1, b = 2, 10, 20, 30}").unwrap();
    let backing = Table::from_value(runtime.global("backing").unwrap()).unwrap();
    let view = readonly::make_read_only_view(&runtime, &backing).unwrap();
    runtime.set_global("view", &view).unwrap();
    runtime
        .exec(
            // Stock Luau's pairs/ipairs ignore __pairs/__ipairs; the generic for honours __iter,
            // and OpenMW's sandbox prelude (a later phase) wraps pairs/ipairs to use them.
            "assert(view.a == 1 and view[2] == 20 and #view == 3) \
             local m = 0 for k, v in view do m += 1 end assert(m == 5, m) \
             local mt = getmetatable(backing) assert(mt == nil) \
             local n = 0 for i = 1, #view do n += view[i] end assert(n == 60, n) \
             assert(getmetatable(view) == false) assert(view.missing == nil)",
        )
        .unwrap();
    let error = runtime.exec("view.a = 5").unwrap_err().to_string();
    assert!(error.contains("attempt to modify a readonly table"), "{error}");

    // Writes to the backing table are visible; host-side field mutation goes through the view.
    runtime.exec("backing.c = 3").unwrap();
    runtime.exec("assert(view.c == 3)").unwrap();
    readonly::set_read_only_field(&runtime, &view, "d", &4i32).unwrap();
    runtime.exec("assert(view.d == 4 and backing.d == 4)").unwrap();

    // A view of a view is the same view; a strict view over it raises on missing keys.
    let again = readonly::make_read_only_view(&runtime, &view).unwrap();
    assert_eq!(again.value(), view.value());
    let strict = readonly::make_strict_read_only_view(&runtime, &view).unwrap();
    runtime.set_global("strict", &strict).unwrap();
    runtime.exec("assert(strict.a == 1)").unwrap();
    let error = runtime.exec("return strict.nothing").unwrap_err().to_string();
    assert!(error.contains("Key not found: nothing"), "{error}");
}

#[test]
fn strict_freezing_and_frozen_packages() {
    let runtime = Runtime::new().unwrap();
    runtime.exec("cfg = {speed = 1}").unwrap();
    let cfg = Table::from_value(runtime.global("cfg").unwrap()).unwrap();
    readonly::make_strict_read_only(&runtime, &cfg).unwrap();
    runtime.exec("assert(cfg.speed == 1)").unwrap();
    let error = runtime.exec("return cfg.typo").unwrap_err().to_string();
    assert!(error.contains("Key not found: typo"), "{error}");
    assert_eq!(
        readonly::make_strict_read_only(&runtime, &cfg).unwrap_err(),
        Error::logic("Strict read-only table is already frozen")
    );

    let package = Table::new(&runtime.stack(), 0, 1).unwrap();
    package.set(&runtime.stack(), "x", &1i32).unwrap();
    let to_string = runtime.load_function("return function() return 'pkg' end").unwrap();
    readonly::make_frozen_package(&runtime, &package, &to_string).unwrap();
    runtime.set_global("pkg", &package).unwrap();
    runtime.exec("assert(tostring(pkg) == 'pkg' and pkg.x == 1 and getmetatable(pkg) == false)").unwrap();
    assert!(readonly::make_frozen_package(&runtime, &package, &to_string).is_err());

    let other = Runtime::new().unwrap();
    let foreign = Table::new(&other.stack(), 0, 0).unwrap();
    assert!(readonly::make_read_only(&runtime, &foreign).is_err());
    let _ = Value::invalid();
}
