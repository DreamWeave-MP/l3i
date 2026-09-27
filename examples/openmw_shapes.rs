//! The OpenMW parity spike (PLAN.md Phase 9): every binding shape the engine's Lua layer uses,
//! on toy types, with no engine code. Run with `cargo run --example openmw_shapes`.
//!
//! Shapes covered: a tagged `Vec3` with methods, getters and setters, a direct primitive field,
//! native `vector` ingress, an untagged `Inventory` type owning Rust data, an engine-owned
//! `World` observed through a `StableRef`, a bound function capturing Rust context, an owned
//! `Function` pin used as a callback from Rust, an array iterator, a module registered through
//! `LuauModule`, a sandboxed script instance, and a bound function crossing a coroutine
//! boundary (called from inside `coroutine.wrap`, raising through it).

use std::cell::{Cell, RefCell};
use std::ptr::NonNull;
use std::rc::Rc;

use l3i::bind::Call;
use l3i::convert::Vector3;
use l3i::direct::field::{DirectField, FieldValue};
use l3i::module::{LuauModule, ModuleBuilder};
use l3i::runtime::{CallContext, MemoryCategory};
use l3i::sandbox::{InstanceSpec, SandboxOptions};
use l3i::stack::Scope;
use l3i::userdata::{Borrowed, Owned, StableRef, Userdata, check_receiver};
use l3i::value::{Function, Value};
use l3i::{Result, Runtime};

// --- A tagged hot type: inline payload, one tag, direct field for `x` -------------------------

#[derive(Debug)]
struct Vec3 {
    x: Cell<f32>,
    y: Cell<f32>,
    z: Cell<f32>,
}

impl Vec3 {
    fn new(x: f32, y: f32, z: f32) -> Vec3 {
        Vec3 { x: Cell::new(x), y: Cell::new(y), z: Cell::new(z) }
    }
    fn length(&self) -> f32 {
        (self.x.get().powi(2) + self.y.get().powi(2) + self.z.get().powi(2)).sqrt()
    }
}

unsafe impl Userdata for Vec3 {
    const NAME: &'static str = "shapes.Vec3";
}

struct Vec3X;
impl DirectField<Vec3> for Vec3X {
    fn get(value: &Vec3) -> FieldValue {
        FieldValue::Number(f64::from(value.x.get()))
    }
}

// --- An untagged long-tail type owning Rust data --------------------------------------------

struct Inventory {
    items: RefCell<Vec<String>>,
}

unsafe impl Userdata for Inventory {
    const NAME: &'static str = "shapes.Inventory";
}

// --- An engine-owned object Lua only observes -------------------------------------------------

struct World {
    name: String,
    ticks: Cell<u64>,
}

unsafe impl Userdata for World {
    const NAME: &'static str = "shapes.World";
}

// --- The module ------------------------------------------------------------------------------

struct Shapes;

impl LuauModule for Shapes {
    const NAME: &'static str = "dreamweave.shapes";

    fn register(runtime: &Runtime, module: &mut ModuleBuilder<'_>) -> Result<()> {
        module.userdata::<Vec3>(Some(10), |ty| {
            ty.property_rw("y", |v: &Vec3| v.y.get(), |v: &Vec3, y: f32| v.y.set(y))?;
            ty.property("z", |v: &Vec3| v.z.get())?;
            ty.method("length", Vec3::length)?;
            ty.method("scaled", |v: &Vec3, factor: f32| {
                Owned(Vec3::new(v.x.get() * factor, v.y.get() * factor, v.z.get() * factor))
            })?;
            // Native `vector` ingress: Luau's own vector type converts straight into Vector3.
            ty.method("add", |v: &Vec3, other: Vector3| {
                Vector3::new(v.x.get() + other.x, v.y.get() + other.y, v.z.get() + other.z)
            })?;
            ty.metamethod("__tostring", |v: &Vec3| format!("Vec3({}, {}, {})", v.x.get(), v.y.get(), v.z.get()))
        })?;
        module.function("vec3", |x: f32, y: f32, z: f32| Owned(Vec3::new(x, y, z)))?;

        module.userdata::<Inventory>(None, |ty| {
            ty.method("add", |inv: &Inventory, item: &str| {
                inv.items.borrow_mut().push(item.to_owned());
                inv.items.borrow().len()
            })?;
            ty.property("count", |inv: &Inventory| inv.items.borrow().len())?;
            ty.array_iterator(|inv: &Inventory, index: f64| -> Option<(f64, String)> {
                let items = inv.items.borrow();
                let next = index as usize;
                items.get(next).map(|item| (index + 1.0, item.clone()))
            })
        })?;
        module.function("inventory", |_: &Call| Owned(Inventory { items: RefCell::new(Vec::new()) }))?;

        module.userdata::<World>(None, |ty| {
            ty.property("name", |w: &World| w.name.clone())?;
            ty.method("tick", |w: &World| {
                w.ticks.set(w.ticks.get() + 1);
                w.ticks.get()
            })
        })?;

        // Captured Rust context: a counter shared with the host.
        let calls = Rc::new(Cell::new(0u32));
        let seen = calls.clone();
        module.function("touch", move || {
            seen.set(seen.get() + 1);
            seen.get()
        })?;
        module.set("VERSION", &1i32)?;
        let _ = runtime;
        Ok(())
    }
}

fn main() {
    let runtime = Runtime::builder().debug_roots(&["shapes", "dreamweave"]).profiler(true).build().unwrap();
    let shapes = runtime.register_module::<Shapes>().unwrap();
    l3i::direct::field::register::<Vec3, Vec3X>(&runtime, "x").unwrap();
    runtime.set_global("shapes", &shapes).unwrap();

    // An engine object outliving every script observation of it.
    let mut world = World { name: "Vvardenfell".to_owned(), ticks: Cell::new(0) };
    let world_ref = unsafe { StableRef::new(NonNull::from(&mut world)) };
    let world_userdata = runtime
        .stack()
        .with_frame(|frame| {
            frame.push(&Borrowed(world_ref))?;
            Value::store(frame.top_value())
        })
        .unwrap();
    runtime.set_global("world", &world_userdata).unwrap();

    // Rust calls a Lua callback it pinned.
    let callback: Function = runtime.load_function("return function(v) return v:length() * 2 end").unwrap();

    runtime
        .exec(
            r#"
            local v = shapes.vec3(3, 4, 0)
            assert(v.x == 3 and v.y == 4 and v.z == 0)          -- x is a direct field, y/z generated getters
            v.y = 0                                              -- generated setter
            assert(v:length() == 3)
            assert(tostring(v) == 'Vec3(3, 0, 0)')
            local s = v:scaled(2)                                -- Rust returns a new tagged userdata
            assert(typeof(s) == 'shapes.Vec3' and s.x == 6)
            local sum = v:add(vector.create(1, 2, 3))            -- native vector in, native vector out
            assert(sum.x == 4 and sum.y == 2 and sum.z == 3)
            local ok, err = pcall(function() return v.z == nil or v.nope end)
            assert(ok)
            ok, err = pcall(function() v.z = 5 end)              -- read-only property
            assert(not ok and err:find('z'), err)

            local inv = shapes.inventory()
            assert(inv:add('sword') == 1 and inv:add('shield') == 2 and inv.count == 2)
            local names = {}
            for i, item in inv do names[#names + 1] = i .. '=' .. item end
            assert(table.concat(names, ',') == '1=sword,2=shield')

            assert(world.name == 'Vvardenfell' and world:tick() == 1 and world:tick() == 2)
            assert(shapes.touch() == 1 and shapes.touch() == 2)
            assert(shapes.VERSION == 1)

            -- A bound function called across a coroutine boundary, including a raise through it.
            local co = coroutine.wrap(function(a)
                local first = shapes.vec3(a, 0, 0):length()
                local b = coroutine.yield(first)
                return shapes.vec3(b, 0, 0):scaled(2).x
            end)
            assert(co(5) == 5 and co(7) == 14)
            local failing = coroutine.wrap(function() return shapes.vec3('x', 0, 0) end)
            ok, err = pcall(failing)
            assert(not ok and err:find('bad argument #1'), err)
            "#,
        )
        .unwrap();
    assert_eq!(world.ticks.get(), 2, "the engine object saw both ticks");

    let doubled = runtime
        .stack()
        .with_frame(|frame| {
            l3i::userdata::tagged::push(frame, Vec3::new(0.0, 3.0, 4.0))?;
            let view = frame.top_value();
            let _ = check_receiver::<Vec3>(view)?;
            callback.invoke::<f32, _>(frame, (view,))
        })
        .unwrap();
    assert_eq!(doubled, 10.0);

    // The same module inside a sandboxed instance: frozen library tables, private globals.
    let sandbox = runtime.sandbox(|line| println!("[script] {line}"), SandboxOptions::default()).unwrap();
    let loader = runtime.load_function("return function(name) error('module ' .. name .. ' not found') end").unwrap();
    let instance = sandbox
        .new_instance(&runtime, &InstanceSpec { name: "spike", packages: &[], hidden_data: None, loader: &loader })
        .unwrap();
    let template = sandbox
        .load_template(
            &runtime,
            "spike.lua",
            "local shapes = require('shapes')\nprint('length', shapes.vec3(1, 2, 2):length())\nassert(pcall(function() math.pi = 3 end) == false)\nreturn shapes.touch()",
        )
        .unwrap();
    let results =
        sandbox.run(&runtime, &template, &instance, CallContext { id: 1, category: MemoryCategory(3) }).unwrap();
    let touches = results[0].with_value(&runtime.stack(), |_, view| view.read::<i32>()).unwrap();
    assert_eq!(touches, 3);
    let stats = runtime.call_stats();
    assert_eq!(stats.timed_calls, 1);
    println!(
        "script time {:.3} ms, category 3 holds {} bytes",
        stats.total_script_ms,
        runtime.total_bytes_in(MemoryCategory(3))
    );
    println!("all shapes verified");
}
