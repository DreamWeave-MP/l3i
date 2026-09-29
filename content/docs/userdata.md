+++
title = "Userdata"
description = "The Userdata trait, tagged against untagged registration and what each costs, every member the metatable builder takes, why receivers are shared references, and the storage wrappers for owned and engine-owned objects."
weight = 30

[extra]
kind = "guide"
+++

## The trait

A Rust type is exposed to Luau by implementing `userdata::Userdata`, which gives it a stable
identity and a script name and nothing else:

```rust
use l3i::userdata::Userdata;

struct Inventory {
    items: std::cell::RefCell<Vec<String>>,
}

unsafe impl Userdata for Inventory {
    const NAME: &'static str = "dreamweave.Inventory";
}
```

`NAME` is the script-visible `__type` (`typeof(x)`) and the root of the type's debug names:
`<NAME>.<member>` for methods, `<NAME>.get.<member>` and `<NAME>.set.<member>` for properties.
It must sit under one of the runtime's debug-name roots and be unique within a VM; registration
verifies both. The impl is `unsafe` because the type promises that `Drop` never calls into the
Lua API, never panics, and does not depend on the VM being consistent: Luau runs it while
sweeping.

How the type is exposed is decided per runtime, at registration. The same Rust type may be tag
8 in one runtime, 17 in another, and untagged in a third.

## Tagged or untagged

| | Tagged | Untagged |
|---|---|---|
| Register | `tagged::register::<T>(&runtime, tag, configure)` | `untagged::register::<T>(&runtime, configure)` |
| Identity | One Luau runtime tag, recorded `tag -> T` in the runtime's tag plan | Exact metatable identity, cached per VM by `TypeId` |
| Check | One tag read and one `TypeId` compare | One metatable pointer compare against the cached identity |
| Payload | `T` inline in the userdata | `Storage<T>`: owned, or a `StableRef<T>` borrow |
| Destructor | One per tag | One per instance |
| Tags used | One of `1..TAG_LIMIT` | None |
| For | Hot types, direct access, native lowering | The long tail |

Tags are scarce: `TAG_LIMIT` is 254, `build.rs` compiles Luau with `LUA_UTAG_LIMIT=254`, and
tag 0 is Luau's untagged default, which is never registered. Direct access and the `jit`
feature's lowering hooks need a tag; a type that scripts call a few times per frame does not.

Tagged registration is idempotent for the same type under the same tag. Another type on the
tag, the same type on another tag, or a name that already names a registry metatable is a logic
error, and a failure inside `configure` unpublishes the half-built metatable. Untagged
registration is transactional in the same way. Instances are allocated with
`lua_newuserdatataggedwithmetatable` and initialised at once, so Luau never owns a destructor
over uninitialised Rust memory.

```rust
use std::cell::Cell;

use l3i::userdata::{Userdata, tagged, untagged};
use l3i::{Result, Runtime};

struct Vec3 {
    x: Cell<f32>,
}

unsafe impl Userdata for Vec3 {
    const NAME: &'static str = "dreamweave.Vec3";
}

struct Inventory {
    items: std::cell::RefCell<Vec<String>>,
}

unsafe impl Userdata for Inventory {
    const NAME: &'static str = "dreamweave.Inventory";
}

fn register(runtime: &Runtime) -> Result<()> {
    tagged::register::<Vec3>(runtime, 10, |ty| {
        ty.property_rw("x", |v: &Vec3| v.x.get(), |v: &Vec3, x: f32| v.x.set(x))
    })?;
    untagged::register::<Inventory>(runtime, |ty| {
        ty.method("add", |inv: &Inventory, item: &str| {
            inv.items.borrow_mut().push(item.to_owned());
            inv.items.borrow().len()
        })?;
        ty.property("count", |inv: &Inventory| inv.items.borrow().len())
    })
}
```

A module registers the same way through `ModuleBuilder::userdata::<T>(Some(tag), configure)`
or `userdata::<T>(None, configure)`; see [Modules and sandboxes](@/docs/modules-and-sandboxes.md).

## Creating and reading instances

| Call | Effect |
|---|---|
| `tagged::push(scope, value)`, `untagged::push(scope, value)` | A new Luau-owned instance on the scope; a logic error for an unregistered type |
| `untagged::push_borrowed(scope, stable_ref)` | A userdata that borrows an engine-owned object |
| `userdata::push_owned(scope, value)` | Whichever path this VM registered `T` on |
| `tagged::test::<T>(view)`, `untagged::test::<T>(view)` | The payload if the view is a `T`, else `None` |
| `untagged::test_owned::<T>(view)` | The payload only when Luau owns it |
| `tagged::check::<T>(view)`, `untagged::check::<T>(view)` | The payload, or the Luau-style type error naming `T::NAME` |
| `userdata::receiver::<T>(view)`, `check_receiver::<T>(view)` | Either path: one tag compare, then one metatable compare |
| `tagged::tag_of::<T>(scope)`, `tagged::is_registered::<T>(scope)`, `untagged::is_registered::<T>(frame)` | What this VM assigned |

From a bound function, `Owned(value)` as the return type moves the value into a new instance of
whichever path the VM registered, and `Borrowed(stable_ref)` returns a borrow. Pushing an
`Owned<T>` by reference (a module value, a table field) clones the payload, so that path needs
`T: Clone`.

## The metatable builder

`configure` receives a `userdata::metatable::MetatableBuilder` over a metatable that is
protected by default (`__metatable = false`, so `getmetatable(x)` is `false` and `setmetatable`
fails), has its `__type` set from `NAME`, and is frozen by the registration that owns it.

| Member | Registers |
|---|---|
| `method(name, callable)` | `x:name(..)` and `x.name(x, ..)`; the callable's first parameter is the `&T` receiver, left out of argument numbering |
| `property(name, getter)` | `x.name`; the getter takes the receiver and returns the value |
| `property_rw(name, getter, setter)` | `x.name` and `x.name = v`; the setter takes the receiver and exactly one Lua value |
| `unbound_method(name, &function)` | An already-bound function as a callable member with no native receiver |
| `metamethod(name, callable)` | A bound closure as `__tostring`, `__len`, `__call`, `__eq` and the rest, named `<NAME>.<metamethod>`; these bind in function mode, so the receiver is argument #1 |
| `metamethod_value(name, &value)`, `raw_metamethod(name, lua_CFunction)` | An existing function or a raw C function as a metamethod |
| `set_field(name, &value)` | A raw field on the metatable, `__metatable` included |
| `array_iterator(next)` | `__iter` returning `(next, object, 0)`: `next` receives the object and the numeric control and returns `Option<(control, value)>` |
| `keyed_iterator(next)` | `__iter` returning `(next, object, nil)`: `next` receives the object and the previous key |
| `cursor_iterator(make_cursor, next)` | `__iter` with a private cursor per loop: `make_cursor` receives the call, `next` a `Cursor<C>`, so nested loops over one object never share state |
| `begin_native_methods()`, `add_native_method(name, body, discriminator)`, `install_native_method_index()` | A table of hand-written `lua_CFunction`s, each with an integer discriminator as upvalue 1 (`MetatableBuilder::native_discriminator(call)` reads it), frozen and installed as `__index` |
| `direct_dispatch::<T>(which)` | The direct-access wrappers; see [Direct access](@/docs/direct-access.md) |

Member registration is a phase machine with OpenMW's conflict rules. Methods alone make a plain
methods table the `__index` (no dispatcher). The first property getter upgrades it to a
generated `__index` (methods, then getters) plus a generated `__namecall` keyed by Luau atoms;
the first setter installs a generated `__newindex`, whose misses are read-only errors
(`dreamweave.Vec3 field 'z' is read-only`, `dreamweave.Vec3: cannot assign to number key`). An
explicit `__index`, `__newindex`, `__namecall` or `__len`, a native method table, a duplicate
member name, a setter that does not take one value, a member whose receiver is another type,
and a second `__iter` all fail at registration with a logic error, and every failure rolls the
registration back.

The generated dispatchers run bound members directly on their own stack instead of `lua_call`ing
the member closure, so a generated method call or property read costs one Luau call frame, not
two. A getter sees exactly one argument, the receiver. Untagged receiver checks compare against
the per-VM cached metatable identity rather than reading the registry each time.

## Receivers are `&T`

A member's receiver is always `&T`, never `&mut T`. The same userdata can appear in several
argument slots of one call (`bar:add(bar)`), so a `&mut` receiver could alias a `&T` argument.
Mutation goes through interior mutability in the payload, `Cell` and `RefCell` in the examples
above. `tagged::test_mut` exists as an `unsafe` escape hatch whose caller proves no other
reference to the payload is live.

A method called without its receiver reads the receiver first and reports `missing argument #1
to 'dreamweave.Vec3.length' (dreamweave.Vec3 expected)`; the C++ binder reported an argument
count of -1 there. Both are listed in [Safety model](@/docs/safety.md#deliberate-divergences).

## Storage

| Type | Meaning |
|---|---|
| `Owned<T>(T)` | A return or a pushable value: moves (or clones, when pushed by reference) `T` into a new instance |
| `Borrowed<T>(StableRef<T>)` | A return or a pushable value: an untagged instance that observes an engine-owned object |
| `Storage<T>` | The untagged payload as stored: `get()` for either case, `owned()` for the payload only when Luau owns it |
| `StableRef<T>` | A `NonNull<T>` to an engine-owned object; `StableRef::new` is `unsafe` |

`StableRef` carries the host's guarantee that the pointee and its address stay valid whenever
Lua can reach the userdata, and that the pointee is destroyed only after Lua execution has
stopped. Dropping the userdata never dereferences the pointer. Tagged payloads are always owned;
a borrow is an untagged instance, and only the read path (`test`, `check`, the `&T` receiver)
can see it. Where an engine object's lifetime is not already that strong, an `Arc<T>` payload
or a handle is the better payload.

```rust
use std::ptr::NonNull;

use l3i::stack::Scope;
use l3i::userdata::{Borrowed, StableRef, Userdata, untagged};
use l3i::value::Value;
use l3i::{Result, Runtime};

struct World {
    name: String,
}

unsafe impl Userdata for World {
    const NAME: &'static str = "dreamweave.World";
}

fn expose(runtime: &Runtime, world: &mut World) -> Result<Value> {
    untagged::register::<World>(runtime, |ty| ty.property("name", |w: &World| w.name.clone()))?;
    // SAFETY: the caller keeps `world` alive and in place until the runtime is dropped.
    let world_ref = unsafe { StableRef::new(NonNull::from(world)) };
    runtime.stack().with_frame(|frame| {
        frame.push(&Borrowed(world_ref))?;
        Value::store(frame.top_value())
    })
}
```

## Collection

Luau runs a tagged type's destructor once per instance when it frees the userdata, and an
untagged instance's `Storage<T>` drop, which drops an owned payload and only the pointer for a
borrow. `Runtime::collect_garbage()` runs a full cycle; two full cycles free everything that
was unreachable at the first. The dispatch tables root every member closure, so a metatable
survives a full collection before its first use.
