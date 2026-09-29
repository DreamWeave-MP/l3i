+++
title = "Direct access and atoms"
description = "Atom catalogues, the runtime-resolved dispatch plan against the compile-time registry, validating Luau's per-instruction cache in constant time, direct handlers with their metamethod fallback, direct fields, and the vector buffer writer."
weight = 60

[extra]
kind = "guide"
+++

Luau can call a native callback straight from the `GETTABLEKS`, `SETTABLEKS` and `NAMECALL`
instructions for a tagged userdata when the key is an interned string with an *atom*, skipping
the metatable walk and the Lua call frame. Three pieces make that work in l3i:

- an `AtomCatalogue` installed as `lua_Callbacks.useratom`, giving selected strings stable
  small integer ids;
- a table resolving `(tag, access kind, atom)` to a slot id, either built at run time
  (`direct::plan::DirectPlan`) or at compile time (`direct::registry::Registry`), with Luau's
  per-instruction 16-bit cache validated before use;
- per-tag direct callbacks registered with `lua_registeruserdatadirectaccess`, which Luau runs
  inside a C frame whose function slot is the stored metamethod. The binder installs wrapper
  metamethods that keep the original as upvalue 1, so the direct path and the ordinary
  metamethod path run the same typed handler and can fall back to the original.

Direct fields are separate: a per-field getter that writes straight into the destination
register with no frame at all.

## Atoms

A `direct::Atom` is an `i16`; `UNKNOWN_ATOM` (-1) is what Luau reports for strings outside the
catalogue. Names and atoms are host data: the binder only requires them to be unique and
non-negative. Each runtime carries its own `AtomCatalogue`, and two runtimes in one process may
carry different ones, because `useratom` receives the `lua_State` and the binder resolves
through that VM's shared block.

```rust
use l3i::direct::AtomCatalogue;
use l3i::Runtime;

fn main() -> l3i::Result<()> {
    let catalogue = AtomCatalogue::try_new([("value", 1), ("name", 2), ("length", 3)])?;
    let runtime = Runtime::builder().atom_catalogue(catalogue).build()?;
    assert_eq!(runtime.atom_of("name"), Some(2));
    assert_eq!(runtime.atom_of("nothing"), None);
    Ok(())
}
```

`AtomCatalogue::try_new` validates the entries (non-empty and unique names, unique non-negative
atoms); `from_static` builds one from a static table such as a registry's
`catalogue_entries()`. `atom_of`, `atom_of_bytes`, `name_of`, `entries` and `len` read it. The
catalogue goes in through `RuntimeBuilder::atom_catalogue`, or through
`direct::install_atom_callback(&runtime, catalogue)` on a runtime built with
`standard_libraries(false)` before the libraries are opened, which is OpenMW's order. A VM takes
exactly one catalogue; a second, or one over a foreign `useratom`, is a logic error.
`Runtime::atom_catalogue()` and `atom_of(name)` read the installed one, and
`direct::atom_of_view(view)` the atom of a string on the stack, resolving it now if Luau has
not yet.

## Plans and registries

Luau's per-instruction cache is a `u16` shared between every userdata type and every access
kind, starting at `UNKNOWN_SLOT` (0). A cached slot is trusted only after a check that it names
exactly this `(tag, atom, kind)`, and that check is one load and one compare.

`direct::plan::DirectPlan` is the normal path: a dense `(tag, kind, atom) -> slot` table built
per runtime from the tags and atoms that VM actually assigned. A type's tag comes from the
runtime's tag plan, a member's atom from the VM's catalogue, and the slot ids are the host's
protocol identifiers. Handlers written against a plan keep working when the same type is tag 8
in one VM and tag 17 in another, and the cache-hit path compares the cached descriptor's
`TypeId`, so it never needs a tag lookup.

```rust
use l3i::direct::plan::DirectPlanBuilder;
use l3i::direct::AccessKind;
use l3i::Runtime;

const VALUE_GET: u16 = 1;
const VALUE_SET: u16 = 2;

fn publish(runtime: &Runtime) -> l3i::Result<()> {
    // `Planned` is registered tagged in this runtime and "value" is in its catalogue.
    DirectPlanBuilder::new(runtime)
        .slot::<Planned>(AccessKind::Index, "value", VALUE_GET)?
        .slot::<Planned>(AccessKind::NewIndex, "value", VALUE_SET)?
        .finish()?;
    Ok(())
}
```

`slot` requires the type registered tagged in this runtime, the member catalogued as an atom, a
slot id above 0 and unique in the plan, and slots and atoms allocated densely: the table covers
the tags in use times the atom span, so `MAX_SLOT` and `MAX_ATOM_SPAN` are 4096 and a two-member
plan with atoms 1 and 30000 is refused rather than costing megabytes. `finish` installs the plan
as the runtime's, and a runtime takes exactly one: the slot numbers are the host's dispatch
protocol and Luau's inline caches hold them, so a second `finish` fails with `This runtime
already has a direct plan; plans are published once per VM`. `direct::plan::plan(scope)` reads
the installed plan from any scope of the VM, one shared-block read and an `Rc` clone.

| `DirectPlan` | Answers |
|---|---|
| `resolve_slot(tag, atom, kind)` | The slot, or `UNKNOWN_SLOT` |
| `cached_slot_matches::<T>(cached, atom, kind)` | Whether `cached` names exactly `(T, atom, kind)` here, with no tag lookup |
| `cached_slot_matches_tag(cached, tag, atom, kind)` | The same for callbacks that know the tag but not the Rust type |
| `resolve_cached_slot::<T>(scope, &mut cached, atom, kind)` | A validated hit at once; a miss resolves `T`'s tag in the scope's VM and writes the slot back |
| `entries()` | Every `PlanEntry`: slot, tag, atom, kind, type and member name |

`direct::registry::Registry<ATOMS, SLOTS>` is the static alternative for hosts whose identities
really are compile-time constants: `Registry::new(atoms, descriptors)` is a `const fn` that
folds a catalogue and its `Descriptor`s into the dense table at compile time and rejects a
non-contiguous catalogue, a repeated `(tag, atom, kind)`, or a slot out of order. Slot values are
contiguous and fixed by catalogue order, never registration order, so they can be treated as
protocol identifiers. `resolve_slot`, `cached_slot_matches` and `resolve_cached_slot(&mut
cached, tag, atom, kind)` mirror the plan, and `atom_range_matches_slots(tag, kind, first, last)`
lets code that dispatches on an atom range fail to compile when a row moves.

## Direct handlers

A tagged type implements `direct::DirectAccess`. Handlers run inside Luau's direct-access C
frame: for index the stack is `[ud, key]`, for newindex `[ud, key, value]`, for namecall
`[ud, args...]` with `lua_namecallatom` valid. `slot` is Luau's cache word for the instruction.
Errors raise into Luau; panics abort.

```rust
use std::cell::Cell;

use l3i::bind::Call;
use l3i::direct::{self, AccessKind, Atom, DirectAccess, Dispatch};
use l3i::stack::Scope;
use l3i::userdata::Userdata;
use l3i::Result;

struct Planned {
    value: Cell<f64>,
}

unsafe impl Userdata for Planned {
    const NAME: &'static str = "dreamweave.Planned";
}

const VALUE_GET: u16 = 1;
const VALUE_SET: u16 = 2;

impl DirectAccess for Planned {
    fn direct_index(call: &Call<'_>, data: &Planned, atom: Atom, slot: &mut u16) -> Result<Dispatch> {
        let Some(plan) = direct::plan::plan(call) else { return Ok(Dispatch::Fallback) };
        match plan.resolve_cached_slot::<Planned>(call, slot, atom, AccessKind::Index) {
            VALUE_GET => {
                call.push(&data.value.get())?;
                Ok(Dispatch::Handled)
            }
            _ => Ok(Dispatch::Fallback),
        }
    }

    fn direct_newindex(call: &Call<'_>, data: &Planned, atom: Atom, slot: &mut u16) -> Result<Dispatch> {
        let Some(plan) = direct::plan::plan(call) else { return Ok(Dispatch::Fallback) };
        match plan.resolve_cached_slot::<Planned>(call, slot, atom, AccessKind::NewIndex) {
            VALUE_SET => {
                data.value.set(call.arg(3).read::<f64>()?);
                Ok(Dispatch::Handled)
            }
            _ => Ok(Dispatch::Fallback),
        }
    }
}
```

`direct_index` and `direct_newindex` return `Dispatch::Handled` (exactly one pushed value for
index, none for newindex) or `Dispatch::Fallback` to let the original metamethod answer;
`direct_namecall` returns `Some(result_count)` or `None`. A method whose handler is not
overridden has a default that falls back.

Installing the handlers takes two steps in a fixed order:

1. inside the type's registration, after every method, property and metamethod, call
   `ty.direct_dispatch::<T>(DirectMetamethods::ALL)` (or `INDEX`, `NAMECALL`, or a struct with
   the three booleans). It wraps the metamethods already present, keeping each original as the
   wrapper's upvalue 1, and requires an original to exist for each kind it wraps;
2. after registration, `direct::register::<T>(&runtime, which)` registers the VM callbacks for
   the same kinds. It requires the type tagged with a read-only metatable whose metamethods are
   the wrappers, because a fallback would otherwise re-enter the callback.

Keys without an atom, and non-string keys, never reach the handler: they go straight to the
original metamethod. The ordinary metamethod path (an import-folded global access, say) runs the
same handler through the wrapper, with a fresh cache slot each time since it has no instruction
cache. A shared call site that sees two tags in turn misses the cache once per switch and never
serves one type's slot to the other.

## Direct fields

`direct::field` is Luau 0.740's `lua_registeruserdatadirectfieldget`: a registered getter runs
from `GETTABLEKS` with no Lua frame, receiving only the userdata payload and a result slot. It
must not touch the Lua API and cannot fail, so it is a unit type implementing
`direct::field::DirectField<T>` with one function.

```rust
use std::cell::Cell;

use l3i::direct::field::{DirectField, FieldValue};
use l3i::userdata::Userdata;
use l3i::Runtime;

struct Vec3 {
    x: Cell<f32>,
}

unsafe impl Userdata for Vec3 {
    const NAME: &'static str = "dreamweave.Vec3";
}

struct Vec3X;

impl DirectField<Vec3> for Vec3X {
    fn get(value: &Vec3) -> FieldValue {
        FieldValue::Number(f64::from(value.x.get()))
    }
}

fn install(runtime: &Runtime) -> l3i::Result<()> {
    // `Vec3` is already registered tagged, with a frozen metatable.
    l3i::direct::field::register::<Vec3, Vec3X>(runtime, "x")
}
```

`FieldValue` carries what Luau has setters for: `Nil`, `Boolean`, `Number`, `Integer` and
`Vector`. Luau's direct-field API has no string setter, so a text field is a getter. Direct
fields never reach `__index`; `T` must be a registered tagged type with a read-only metatable,
and since Luau offers no query, replacement or removal, registering a field twice is a logic
error. A planned direct field costs 78 instructions per read against 397 for a planned getter;
[Performance](@/docs/performance.md) has the whole table.

## The vector buffer writer

Luau's `vector` metatable ships an `__index` but no `__namecall`.
`Runtime::install_vector_buffer_writer()` installs one that adds
`vector:writef32x3(buffer, offset)`: one call with exactly the semantics of three
`buffer.writef32` calls, the same offset conversion, bounds check, message (`buffer access out
of bounds`) and host byte order. Any other method name resolves through the original `__index`,
kept as upvalue 1, so its diagnostics are unchanged. The call fails when the metatable already
has a `__namecall`, and uses this VM's atom for `writef32x3` when the catalogue has one.

```luau
local v = vector.create(1.5, -2.25, 1e10)
local b = buffer.create(16)
v:writef32x3(b, 4)
assert(buffer.readf32(b, 4) == 1.5 and buffer.readf32(b, 12) == 1e10)
```

Under the `jit` feature, `native_code::vector_buffer::VectorBufferWriter` lowers the same call
to three native f32 stores; natively compiled functions reach this shim only when a guard fails.
[Native code](@/docs/native-code.md) covers the hooks.
