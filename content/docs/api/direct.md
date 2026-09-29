+++
title = "Direct access and the VM"
description = "Atoms, direct dispatch plans and registries, DirectAccess and direct fields; hand-written native entry points; the vector buffer writer; collector controls, dumps, the buffer cage and embedder GC; coroutines; the debug API and runtime hooks."
weight = 240

[extra]
kind = "api"
+++

Modules `l3i::direct` (with `plan`, `registry`, `field`), `l3i::native`, `l3i::vector_writer`,
`l3i::memory`, `l3i::thread` and `l3i::debug`. [Direct access](@/docs/direct-access.md) and
[The rest of the VM](@/docs/vm.md) are the guides.

## Direct access

Module `l3i::direct`. Luau can call a native callback straight from `GETTABLEKS`, `SETTABLEKS`
and `NAMECALL` for a tagged userdata when the key is an interned string with an atom, skipping
the metatable walk and the Lua call frame. Three pieces make that work: an `AtomCatalogue`
installed as `lua_Callbacks.useratom`; a `DirectPlan` or `Registry` resolving `(tag, kind,
atom)` to a slot with Luau's per-instruction 16-bit cache validated before use; and per-tag
direct callbacks, run inside a C frame whose function slot is the stored metamethod. The binder
installs wrapper metamethods that keep the original as upvalue 1, so both the direct path and the
ordinary metamethod path run the same typed handler and can fall back to the original.

{{ api_signature(value="type Atom = i16") }}

{{ api_signature(value="const UNKNOWN_ATOM: Atom = -1") }}

A Luau string atom: a small stable id assigned when the string is interned; `-1` for strings
outside the catalogue.

{{ api_signature(value="enum AccessKind { Index = 0, NewIndex = 1, Namecall = 2 }") }}

{{ api_signature(value="const ACCESS_KIND_COUNT: usize = 3") }}

Which metamethod a direct callback stands in for. `repr(u8)`, `Clone`, `Copy`, `Debug`, `Eq`.

### AtomCatalogue

{{ api_signature(value="struct AtomCatalogue") }}

Interned member names eligible for direct dispatch, owned by one VM. Names and atoms are host
data; the binder requires them to be unique and non-negative. Two runtimes in one process may
carry different catalogues. `Clone`, `Debug`.

| Method | Meaning |
|---|---|
| `fn try_new<N: Into<Cow<'static, str>>>(entries: impl IntoIterator<Item = (N, Atom)>) -> Result<AtomCatalogue>` | Validates: non-empty names, unique names, unique non-negative atoms |
| `fn from_static(entries: &'static [(&'static str, Atom)]) -> Result<AtomCatalogue>` | `try_new` over a static table, such as a `Registry`'s `catalogue_entries()` |
| `fn entries(&self) -> impl Iterator<Item = (&str, Atom)>` | The pairs in the order given |
| `fn len(&self) -> usize`, `fn is_empty(&self) -> bool` | |
| `fn atom_of(&self, name: &str) -> Option<Atom>`, `fn atom_of_bytes(&self, name: &[u8]) -> Option<Atom>` | The atom for a name |
| `fn name_of(&self, atom: Atom) -> Option<&str>` | The spelling of an atom |

{{ api_signature(value="fn install_atom_callback(runtime: &Runtime, catalogue: AtomCatalogue) -> Result<()>") }}

Installs `catalogue` as this VM's `useratom` callback and verifies every spelling resolves.
OpenMW installs the callback before `luaL_openlibs`: call this on a runtime built with
`standard_libraries(false)`, then open them, or pass the catalogue to
`RuntimeBuilder::atom_catalogue`. A VM takes exactly one catalogue; a second, or a foreign
`useratom`, is a logic error.

{{ api_signature(value="fn atom_of_view(view: ValueView<'_>) -> Option<Atom>") }}

The atom of the string at `view`, resolving it now if Luau has not yet.

On `Runtime`: `fn atom_catalogue(&self) -> Option<Rc<AtomCatalogue>>` and `fn atom_of(&self,
name: &str) -> Option<Atom>`.

### DirectAccess

{{ api_signature(value="enum Dispatch { Handled, Fallback }") }}

Outcome of a direct `__index` or `__newindex` handler: the handler produced the result (exactly
one pushed value for index, none for newindex), or the original metamethod answers. `Clone`,
`Copy`, `Debug`, `Eq`.

{{ api_signature(value="trait DirectAccess: Userdata") }}

Direct member access for a tagged userdata type. Handlers run inside Luau's direct-access C
frame: for index the stack is `[ud, key]`, for newindex `[ud, key, value]`, for namecall `[ud,
args...]` with `lua_namecallatom` valid. `slot` is Luau's per-instruction 16-bit cache, shared
between every userdata type and starting at 0; validate it with a plan or registry before
trusting it. Errors are raised into Luau; panics abort. Every method defaults to the fallback.

| Method | Meaning |
|---|---|
| `fn direct_index(call: &Call<'_>, data: &Self, atom: Atom, slot: &mut u16) -> Result<Dispatch>` | |
| `fn direct_newindex(call: &Call<'_>, data: &Self, atom: Atom, slot: &mut u16) -> Result<Dispatch>` | |
| `fn direct_namecall(call: &Call<'_>, data: &Self, atom: Atom, slot: &mut u16) -> Result<Option<c_int>>` | `Some(result_count)` when handled, `None` to fall back |

{{ api_signature(value="struct DirectMetamethods { pub index: bool, pub newindex: bool, pub namecall: bool }") }}

Which of the three metamethods a type dispatches directly, with the constants `ALL`, `INDEX` and
`NAMECALL`. `Clone`, `Copy`, `Debug`, `Default`, `Eq`.

{{ api_signature(value="fn direct_dispatch<T: DirectAccess>(&mut self, which: DirectMetamethods) -> Result<()>") }}

On `MetatableBuilder`: installs the direct-dispatch wrappers over the metamethods already
present, keeping each original as the wrapper's upvalue 1. Call after every method, property and
metamethod registration for `T`, and register the VM callbacks with `register` afterwards. A
missing original metamethod, or a metatable whose `__type` is not `T::NAME`, is a logic error.

{{ api_signature(value="fn register<T: DirectAccess>(runtime: &Runtime, which: DirectMetamethods) -> Result<()>") }}

Registers `T`'s direct callbacks with the VM (`lua_registeruserdatadirectaccess`) for the
metamethods in `which`. `T` must be tagged in this runtime with a read-only metatable whose
corresponding metamethods are the wrappers `direct_dispatch` installed; anything else is a logic
error, because a fallback would otherwise re-enter the callback.

The callbacks themselves are public for hosts that register by hand: `index_callback::<T>`,
`newindex_callback::<T>` (`lua_UserdataDirectAccess`), `namecall_callback::<T>`
(`lua_UserdataDirectNamecall`), and the metamethod wrappers `index_wrapper::<T>`,
`newindex_wrapper::<T>`, `namecall_wrapper::<T>`. All are `unsafe extern "C-unwind"` and must
only be reached through Luau for userdata of `T`'s tag.

### DirectPlan

Module `l3i::direct::plan`: the normal path. A plan is the `(tag, kind, atom) -> slot` table
built at run time from what this VM actually assigned, so handlers keep working when the same
type is tag 8 in one VM and tag 17 in another. The cache-hit path compares the cached entry's
`TypeId`, so it never needs a tag lookup.

{{ api_signature(value="struct PlanEntry { pub slot: u16, pub tag: RuntimeTag, pub atom: Atom, pub kind: AccessKind, pub type_id: TypeId, pub type_name: &'static str, pub member: String }") }}

One resolved dispatch entry. `Clone`, `Debug`, `Eq`.

| Constant | Value | Meaning |
|---|---|---|
| `MAX_SLOT: u16` | 4096 | The highest slot id a plan accepts |
| `MAX_ATOM_SPAN: usize` | 4096 | The widest atom range: the table is dense over the tags it uses times the atom span |

{{ api_signature(value="struct DirectPlan") }}

The dense table. `Debug`.

| Method | Meaning |
|---|---|
| `fn entries(&self) -> &[PlanEntry]` | |
| `fn resolve_slot(&self, tag: i32, atom: Atom, kind: AccessKind) -> u16` | The slot, or `UNKNOWN_SLOT` |
| `fn cached_slot_matches<T: Userdata>(&self, cached: u16, atom: Atom, kind: AccessKind) -> bool` | True when `cached` names exactly `(T, atom, kind)`: the cache-hit test with no tag lookup |
| `fn cached_slot_matches_tag(&self, cached: u16, tag: i32, atom: Atom, kind: AccessKind) -> bool` | The same for callbacks that know the receiver's tag but not its Rust type |
| `fn resolve_cached_slot<T: Userdata>(&self, scope: &impl Scope, cached: &mut u16, atom: Atom, kind: AccessKind) -> u16` | A validated hit is returned at once; a miss resolves `T`'s tag in `scope`'s VM and writes the slot back |

{{ api_signature(value="struct DirectPlanBuilder<'r>") }}

| Method | Meaning |
|---|---|
| `fn new(runtime: &'r Runtime) -> Self` | |
| `fn slot<T: Userdata>(self, kind: AccessKind, member: &str, slot: u16) -> Result<Self>` | Maps `T.member` accessed as `kind` to `slot`, a host protocol id above 0 and unique in the plan. `T` must be tagged here and `member` catalogued |
| `fn finish(self) -> Result<Rc<DirectPlan>>` | Builds the dense table and installs the plan as the runtime's. A runtime takes exactly one plan: a second `finish` is refused, because the slot ids are the host's dispatch protocol and Luau's inline caches hold them |

{{ api_signature(value="fn plan(scope: &impl Scope) -> Option<Rc<DirectPlan>>") }}

The plan installed on `scope`'s VM, if any: one shared-block read and an `Rc` clone.

```rust
use l3i::direct::plan::DirectPlanBuilder;
use l3i::direct::{self, AccessKind, DirectMetamethods};

// After `tagged::register::<Planned>` installed `direct_dispatch` wrappers on the metatable:
direct::register::<Planned>(&runtime, DirectMetamethods { index: true, newindex: true, namecall: false })?;
DirectPlanBuilder::new(&runtime)
    .slot::<Planned>(AccessKind::Index, "value", PLANNED_VALUE_GET)?
    .slot::<Planned>(AccessKind::NewIndex, "value", PLANNED_VALUE_SET)?
    .finish()?;
```

### Registry

Module `l3i::direct::registry`: the compile-time alternative for hosts whose tags and atoms are
constants. An atom catalogue plus slot descriptors, folded into a dense table in a `const fn`.
Slot values are contiguous and fixed by catalogue order, never by registration order, so they
can be treated as protocol identifiers.

{{ api_signature(value="const UNKNOWN_SLOT: u16 = 0") }}

Slot 0 is `Unknown`, the value Luau starts every instruction cache with.

{{ api_signature(value="struct Descriptor { pub slot: u16, pub tag: RuntimeTag, pub atom: Atom, pub kind: AccessKind }") }}

One dispatch entry. `Clone`, `Copy`, `Debug`, `Eq`.

{{ api_signature(value="struct Registry<const ATOMS: usize, const SLOTS: usize>") }}

`ATOMS` catalogued names, `SLOTS` descriptors, and the dense lookup table. `Debug`.

| Method | Meaning |
|---|---|
| `const fn new(atoms: [(&'static str, Atom); ATOMS], descriptors: [Descriptor; SLOTS]) -> Registry<ATOMS, SLOTS>` | Builds and validates at compile time: atoms contiguous from the first in catalogue order with unique non-empty names; slots contiguous from the first (above 0) in descriptor order, each naming a catalogued atom and a usable tag; no `(tag, atom, kind)` repeated |
| `const fn atoms(&self) -> &[(&'static str, Atom); ATOMS]` | |
| `const fn descriptors(&self) -> &[Descriptor; SLOTS]` | |
| `const fn atom_of(&self, name: &str) -> Option<Atom>` | |
| `const fn name_of(&self, atom: Atom) -> &'static str` | `unknown` outside the catalogue |
| `const fn resolve_slot(&self, tag: i32, atom: Atom, kind: AccessKind) -> u16` | Bounds checked; `UNKNOWN_SLOT` on a miss |
| `const fn cached_slot_matches(&self, cached: u16, tag: i32, atom: Atom, kind: AccessKind) -> bool` | True when `cached` is one of this registry's slots and names exactly `(tag, atom, kind)` |
| `fn resolve_cached_slot(&self, cached: &mut u16, tag: i32, atom: Atom, kind: AccessKind) -> u16` | A validated hit is returned, otherwise the slot is resolved and written back |
| `const fn atom_range_matches_slots(&self, tag: RuntimeTag, kind: AccessKind, first: Atom, last: Atom) -> bool` | True when exactly the atoms in `first..=last` have a slot of `kind` on `tag`; for code that dispatches on an atom range and must fail to compile when a row moves |
| `const fn catalogue_entries(&self) -> &[(&'static str, Atom); ATOMS]` | The catalogue as `AtomCatalogue` entries |

### Direct fields

Module `l3i::direct::field`: `lua_registeruserdatadirectfieldget`. A registered getter runs from
`GETTABLEKS` with no Lua frame, receiving only the userdata payload and a result slot. It must
not touch the Lua API and must not fail. Only the value kinds Luau provides setters for can be
produced; there is no string setter, so a text field is a getter.

{{ api_signature(value="enum FieldValue { Nil, Boolean(bool), Number(f64), Integer(i64), Vector(Vector3) }") }}

A value a direct field getter can produce. Also `Push` and `Return`, so the same getter serves as
the canonical property. `Clone`, `Copy`, `Debug`, `PartialEq`.

{{ api_signature(value="trait DirectField<T: Userdata>: 'static { fn get(value: &T) -> FieldValue; }") }}

A direct field getter for `T`: a unit type, so the getter is a plain function pointer with no
context.

{{ api_signature(value="fn register<T: Userdata, H: DirectField<T>>(runtime: &Runtime, field: &str) -> Result<()>") }}

Registers `H` as the direct getter for `field` on `T`, which must be a registered tagged type
with a read-only metatable. Luau offers no query, replacement or removal for direct fields, so
registering a field twice is a logic error here.

```rust
use l3i::direct::field::{DirectField, FieldValue};

struct ValueField;

impl DirectField<Counter> for ValueField {
    fn get(counter: &Counter) -> FieldValue {
        FieldValue::Integer(counter.value.get())
    }
}
```

## native

Module `l3i::native`: the entry point for hand-written `lua_CFunction`s that use the binder's
stack layer.

{{ api_signature(value="unsafe fn enter(state: *mut lua_State, body: impl FnOnce(&Stack<'_>) -> Result<c_int>) -> c_int") }}

Runs `body` as the entire implementation of a `lua_CFunction`, with a `Stack` over the calling
thread. Errors become Lua errors after every Rust value has dropped; panics abort. `state` must
be the `lua_State*` Luau passed to the enclosing C function, and the caller that function's
frame. [The raw C API](@/docs/api/ffi.md) has the rules for such functions.

```rust
use std::ffi::c_int;
use l3i::ffi;
use l3i::stack::Scope;

unsafe extern "C-unwind" fn good_method_body(state: *mut ffi::lua_State) -> c_int {
    unsafe {
        l3i::native::enter(state, |stack| {
            let discriminator = stack.at(ffi::lua_upvalueindex(1)).read::<i32>()?;
            let argument = stack.at(2).read::<i32>()?;
            stack.push(&(argument * discriminator))?;
            Ok(1)
        })
    }
}
```

## The vector buffer writer

Module `l3i::vector_writer`: the interpreter half of OpenMW's native vector buffer writer. Luau's
`vector` metatable ships an `__index` but no `__namecall`; installing one lets a script write a
vector into a buffer in a single call with exactly the semantics of three `buffer.writef32`
calls. Any other method name resolves through the original `__index`, kept as upvalue 1. The code
generation half, `native_code::vector_buffer::VectorBufferWriter`, lowers the call to three
native f32 stores.

{{ api_signature(value="fn install_vector_buffer_writer(&self) -> Result<()>") }}

On `Runtime`: installs `vector:writef32x3(buffer, offset)` on the vector metatable. Fails when
the metatable already has a `__namecall`. Uses this VM's atom for `writef32x3` when its
catalogue has one, else compares the method name.

## Memory

Module `l3i::memory`: collector controls beyond the watchdog, allocation rate, dumps, the buffer
cage, embedder GC integration, light userdata, raw tag operations, coroutine finalizers and
fast-flag introspection.

{{ api_signature(value="enum GcControl { Stop, Restart, Collect, Count, CountRemainder, IsRunning, Step(c_int), SetGoal(c_int), SetStepMultiplier(c_int), SetStepSize(c_int), IsPaused }") }}

`lua_gc` commands. Sizes are in kilobytes where Luau's are: `Count` is the heap size in KB,
`CountRemainder` the remainder in bytes, `Step(n)` one incremental step of `n` KB (0 for a basic
step) returning 1 when a cycle ended, `SetGoal` the target heap growth percentage before a new
cycle. The defaults, a 200 percent goal, multiplier 200 and 1 KB steps, are Luau's. `Clone`,
`Copy`, `Debug`, `Eq`.

On `Runtime`:

| Method | Meaning |
|---|---|
| `fn gc(&self, control: GcControl) -> c_int` | Drives the collector and returns its answer |
| `fn allocation_rate(&self) -> i64` | Bytes per second as Luau estimates it; `-1` until the collector has enough history |
| `fn clock() -> f64` | Luau's high-resolution clock in seconds (`lua_clock`) |
| `fn encode_pointer(&self, pointer: usize) -> usize` | Encodes with this VM's pointer-encoding key, as `tostring` does for addresses |
| `fn memory_dump(&self, path: &Path) -> Result<()>` | Luau's memory dump (object counts and sizes per category) |
| `fn gc_dump(&self, path: &Path, category_names: Option<&'static [&'static CStr]>) -> Result<()>` | Luau's full heap graph dump, with categories labelled by index where given |
| `fn set_light_userdata_name(&self, tag: c_int, name: &str) -> Result<()>` | Names light userdata `tag` as `typeof` reports it; tags run `0..LUA_LUTAG_LIMIT` |
| `fn light_userdata_name(&self, tag: c_int) -> Option<String>` | |
| `fn weak_ref(&self, view: ValueView<'_>) -> Result<WeakRef>` | A weak reference that does not keep the value alive |
| `fn set_embedder_gc(&self, gc: impl EmbedderGc)`, `fn clear_embedder_gc(&self)` | The embedder half of cross-heap GC |
| `fn enable_coroutine_finalizers(&self) -> Result<()>` | Luau's experimental `DebugLuauCoroutineFinally` flag |
| `fn finalizer_function(&self) -> Result<Function>` | The `finalize(coroutine)` function that runs a finished coroutine's finalizers, pinned |

```rust
use l3i::Runtime;
use l3i::memory::GcControl;

fn main() -> l3i::Result<()> {
    let runtime = Runtime::new()?;
    runtime.exec("keep = {} for i = 1, 1000 do keep[i] = { i } end")?;
    assert_eq!(runtime.gc(GcControl::IsRunning), 1);
    let previous_goal = runtime.gc(GcControl::SetGoal(150));
    assert_eq!(runtime.gc(GcControl::SetGoal(previous_goal)), 150);
    while runtime.gc(GcControl::Step(1)) != 1 {}
    assert!(runtime.allocation_rate() >= -1);
    Ok(())
}
```

{{ api_signature(value="trait BufferCage: 'static { fn allocate(&self, ptr: *mut c_void, old_size: usize, new_size: usize, kind: c_int) -> *mut c_void; }") }}

A caged allocator for Luau buffers (`lua_setbuffercage`): every `buffer` allocation goes through
it. The `lua_Alloc` contract: `new_size == 0` frees `ptr` and returns null; `ptr` null with
`new_size > 0` allocates; otherwise reallocates. Installed through
`RuntimeBuilder::buffer_cage` before any buffer exists.

{{ api_signature(value="trait EmbedderGc: 'static { fn reset(&self); fn mark_reachable(&self, mark: &mut dyn FnMut(&WeakRef)); }") }}

The embedder side of cross-heap marking (`lua_setembeddergc`): each cycle Luau first asks the
embedder to reset its bookkeeping, then, once it has marked userdata through `UserdataMark`
callbacks, asks it to mark every weak reference reachable from marked native objects. No Lua API
may be used from these methods.

{{ api_signature(value="trait UserdataMark<T: Userdata>: 'static { fn mark(value: &T); }") }}

{{ api_signature(value="fn set_userdata_mark<T: Userdata, M: UserdataMark<T>>(runtime: &Runtime) -> Result<()>") }}

A mark callback for `T` (`lua_setuserdatamark`), called when the collector marks a `T`
reachable; `T` must be tagged in this runtime. No Lua API may be used in `mark`.

{{ api_signature(value="struct WeakRef") }}

An embedder-managed weak reference (`lua_weakref`). Released explicitly; a leaked one costs a
registry slot until the VM closes. `Debug`.

| Method | Meaning |
|---|---|
| `fn get<'s>(&self, scope: &'s impl Scope) -> Option<ValueView<'s>>` | Pushes the value if still alive; `None` (nothing pushed) once collected |
| `fn release(self, scope: &impl Scope)` | Releases the slot |
| `fn id(&self) -> c_int` | |

{{ api_signature(value="struct LightUserdata { pub pointer: *mut c_void, pub tag: c_int }") }}

A light userdata: a raw pointer with a tag (`lua_pushlightuserdatatagged`); tag 0 is the plain
light userdata Lua knows. `Push` (a tag outside `0..LUA_LUTAG_LIMIT` is a logic error) and
`FromView`. `Clone`, `Copy`, `Debug`, `Eq`.

Module `l3i::memory::raw`, for hosts that manage a tag's meaning themselves:

| Function | Meaning |
|---|---|
| `unsafe fn set_userdata_tag(view: ValueView<'_>, tag: RuntimeTag) -> Result<()>` | `lua_setuserdatatag`. Every check in the crate trusts a tag to identify its registered payload type; the caller guarantees the payload fits the new tag's type and its destructor |
| `unsafe fn new_userdata_tagged<'s>(scope: &'s impl Scope, size: usize, tag: RuntimeTag) -> Result<(*mut c_void, ValueView<'s>)>` | `lua_newuserdatatagged` with no metatable; the payload is uninitialised and must be written before Luau can observe it |

Fast flags (process-wide, frozen by the crate's policy once a runtime exists; for Luau's
`Debug*` flags only):

| Function | Meaning |
|---|---|
| `fn fast_flags() -> Vec<(String, bool)>` | Every boolean fast flag in this build with its value |
| `fn fast_flag(name: &str) -> Option<bool>` | One boolean flag; `None` when the build has no such flag |
| `fn set_fast_flag(name: &str, value: bool) -> Result<()>` | |
| `fn fast_int(name: &str) -> Option<c_int>`, `fn set_fast_int(name: &str, value: c_int) -> Result<()>` | Integer flags (`FInt`) |

On `Thread`: `fn add_finalizer(&self, scope: &impl Scope, callback: ValueView<'_>) -> Result<()>`
registers a function to run when the coroutine is finalized (enable finalizers first; fails for
the main thread, a fresh thread or a dead coroutine), and `fn has_finalizers(&self) -> bool`.

## Threads

Module `l3i::thread`: Lua threads driven from the host. The host starts a `Thread` with a
function and arguments, then resumes it with the values the coroutine's `yield` receives. Bound
functions yield by returning `bind::Yield` and request a debugger break by returning
`bind::Break`.

{{ api_signature(value="enum Resume { Yielded(Vec<Value>), Finished(Vec<Value>), Break }") }}

What a resume step did: the coroutine yielded these values and can be resumed; its function
returned these values and the thread is finished; or it hit a `lua_break`. `Debug`.

{{ api_signature(value="enum ThreadStatus { Ok, Yielded, Break, Error(c_int) }") }}

`lua_status`: not started or finished normally; yielded; broken; or died with a `LUA_ERR*`
code. `Clone`, `Copy`, `Debug`, `Eq`.

{{ api_signature(value="enum CoroutineStatus { Running, Suspended, Normal, Finished, FinishedWithError }") }}

`lua_costatus` as seen from another thread; `Normal` is active but not running (it resumed
another coroutine). `Clone`, `Copy`, `Debug`, `Eq`.

{{ api_signature(value="struct Thread") }}

A pinned Lua thread, from `Runtime::new_thread(&self) -> Result<Thread>`, sharing the VM's
globals and registry.

| Method | Meaning |
|---|---|
| `fn value(&self) -> &Value` | The thread as a pinned `thread` value |
| `fn status(&self) -> ThreadStatus` | |
| `fn coroutine_status(&self, scope: &impl Scope) -> Result<CoroutineStatus>` | Relative to `scope`'s thread |
| `fn start<A: PushArgs>(&self, scope: &impl Scope, function: &Function, args: A) -> Result<Resume>` | Runs `function` on this thread with `args` until it yields, returns, breaks or fails. The thread must be idle: fresh, finished normally, or reset |
| `fn resume<A: PushArgs>(&self, scope: &impl Scope, args: A) -> Result<Resume>` | Resumes a yielded or broken coroutine with `args` as the results of its `yield` |
| `fn resume_with_error(&self, scope: &impl Scope, message: &str) -> Result<Resume>` | Resumes by raising `message` inside it, as if its `yield` failed |
| `fn reset(&self) -> Result<()>` | Returns a finished or failed thread to the fresh state |
| `fn is_reset(&self) -> bool` | Fresh or reset, holding no function |
| `fn with_stack<R>(&self, scope: &impl Scope, body: impl FnOnce(&Stack<'_>) -> Result<R>) -> Result<R>` | Runs `body` with the root stack of this thread, for pushing arguments or reading values between resumes. Panics if a root stack on this thread is already alive |
| `fn sandbox(&self, scope: &impl Scope) -> Result<()>` | `luaL_sandboxthread`: a writable globals table proxying the frozen main globals |
| `fn data(&self) -> *mut c_void` | Host data attached to the thread; null when none |
| `unsafe fn set_data(&self, data: *mut c_void) -> Result<()>` | Attaches host data; the binder only stores the pointer |

An error inside the coroutine comes back as `Error::Runtime` (`Lua error: <message>`) and leaves
the thread in its error status. A function or thread from another VM, or a start on a thread
that is not idle, is `Error::Logic`.

```rust
use l3i::Runtime;
use l3i::thread::{Resume, ThreadStatus};

fn main() -> l3i::Result<()> {
    let runtime = Runtime::new()?;
    let generator = runtime.load_function(
        "return function(a) local b = coroutine.yield(a + 1) local c = coroutine.yield(b * 2) return a + b + c end",
    )?;
    let thread = runtime.new_thread()?;
    let stack = runtime.stack();
    let Resume::Yielded(values) = thread.start(&stack, &generator, (1,))? else { panic!("expected a yield") };
    assert_eq!(values.len(), 1);
    assert_eq!(thread.status(), ThreadStatus::Yielded);
    let Resume::Yielded(_) = thread.resume(&stack, (10,))? else { panic!("expected a yield") };
    let Resume::Finished(values) = thread.resume(&stack, (100,))? else { panic!("expected a return") };
    assert_eq!(values.len(), 1);
    Ok(())
}
```

## Debug

Module `l3i::debug`: activation records, locals, upvalues, arguments, tracebacks, single
stepping, breakpoints, coverage, and the VM callbacks a debugger or host needs.

{{ api_signature(value="struct DebugInfo { pub what: String, pub source: String, pub short_source: String, pub name: Option<String>, pub line_defined: i32, pub current_line: i32, pub upvalue_count: u8, pub parameter_count: u8, pub is_vararg: bool }") }}

One activation record (`lua_getinfo` with `"slnua"`): `what` is `Lua` or `C`, `source` the chunk
name without its leading `=` or `@`, `current_line` the line being executed or `-1`. `Clone`,
`Debug`, `Eq`.

{{ api_signature(value="struct CoverageEntry { pub function: Option<String>, pub line_defined: i32, pub depth: i32, pub hits: Vec<i32> }") }}

Hit counts of one function from `lua_getcoverage`; `hits` per line, `-1` for lines without
executable code. `Clone`, `Debug`, `Eq`.

{{ api_signature(value="trait DebugScope: Scope + Sized") }}

Debug queries over the call stack of a scope's thread, implemented for every `Scope`.

| Method | Meaning |
|---|---|
| `fn debug_info(&self, level: c_int) -> Option<DebugInfo>` | The record `level` frames up (0 is the running function), or `None` past the bottom |
| `fn function_info(&self, function: ValueView<'_>) -> Result<DebugInfo>` | The record of the function at a stack slot |
| `fn stack_depth(&self) -> c_int` | Lua and C frames on this thread |
| `fn debug_trace(&self) -> String` | Luau's own multi-line trace |
| `fn traceback(&self, message: Option<&str>, level: c_int) -> Result<String>` | `luaL_traceback` from `level` with an optional leading message |
| `fn local<'s>(&'s self, level: c_int, n: c_int) -> Option<(String, ValueView<'s>)>` | Pushes local `n` (1-based) of the function `level` frames up and returns its name with the view |
| `fn set_local(&self, level: c_int, n: c_int) -> Option<String>` | Pops the top value into that local; returns its name, or `None` (value still popped) when there is no such local |
| `fn argument<'s>(&'s self, level: c_int, n: c_int) -> Option<ValueView<'s>>` | Pushes argument `n`, vararg-aware |
| `fn upvalue<'s>(&'s self, function: ValueView<'_>, n: c_int) -> Option<(String, ValueView<'s>)>` | Pushes upvalue `n` of the function at `function` |
| `fn set_upvalue(&self, function: ValueView<'_>, n: c_int) -> Option<String>` | Pops the top value into that upvalue |
| `fn single_step(&self, enabled: bool)` | Each instruction then reaches `RuntimeHooks::debug_step` |
| `fn set_breakpoint(&self, function: ValueView<'_>, line: c_int, enabled: bool) -> Result<c_int>` | Sets or clears a breakpoint; returns the line it landed on (the next line with code), an error when none exists |
| `fn coverage(&self, function: ValueView<'_>) -> Result<Vec<CoverageEntry>>` | Line hit counts of the function and its nested functions; the chunk must be compiled with a coverage level |

{{ api_signature(value="enum DebugAction { Continue, Break }") }}

What a debugger hook wants the VM to do next. `Break` stops the thread with `LUA_BREAK` for the
host to resume; only possible on a thread the host drives through `Thread`, since on the main
thread Luau raises `attempt to break across metamethod/C-call boundary`. `Clone`, `Copy`,
`Debug`, `Eq`.

{{ api_signature(value="trait RuntimeHooks: 'static") }}

Host callbacks for the remaining `lua_Callbacks` slots. Every method has a no-op default; a
`HookSet` selects which slots are installed so the VM pays only for the ones in use. Hooks that
receive a `Stack` run where the Lua API may be used on that thread; the ones that receive raw
states may not touch it (Luau's documented restriction). Hooks must not panic: panics abort.

| Method | Lua API | Meaning |
|---|---|---|
| `fn panic(&self, error_code: c_int)` | no | An unprotected error was raised (only with `LUA_USE_LONGJMP` builds) |
| `fn user_thread(&self, parent: Option<*mut lua_State>, thread: *mut lua_State)` | no | A thread was created (`parent` is `Some`) or is being destroyed |
| `fn user_finalizer(&self, thread: *mut lua_State, coroutine: *mut lua_State)` | no | A finalizer is about to be attached to `coroutine` |
| `fn debug_break(&self, stack: &Stack<'_>, info: &DebugInfo) -> DebugAction` | yes | A breakpoint was reached. Resuming re-executes the instruction, so the hook is called again for the same breakpoint and must answer `Continue` to step off it |
| `fn debug_step(&self, stack: &Stack<'_>, info: &DebugInfo) -> DebugAction` | yes | One instruction in single-step mode |
| `fn debug_interrupt(&self, stack: &Stack<'_>, info: &DebugInfo, interrupted: *mut lua_State) -> DebugAction` | yes | This thread was interrupted by a break in a coroutine it resumed |
| `fn debug_protected_error(&self, stack: &Stack<'_>)` | yes | A protected call is about to unwind with an error; the error object is on top |
| `fn pre_resume(&self, thread: *mut lua_State)`, `fn post_resume(&self, thread: *mut lua_State)` | no | Around `lua_resume` |
| `fn on_free(&self, thread: *mut lua_State, block: *mut c_void)` | no | A block is being freed |

{{ api_signature(value="struct HookSet { pub panic: bool, pub user_thread: bool, pub user_finalizer: bool, pub debug_break: bool, pub debug_step: bool, pub debug_interrupt: bool, pub debug_protected_error: bool, pub pre_resume: bool, pub post_resume: bool, pub on_free: bool }") }}

Which slots to install, with the constants `NONE`, `ALL` and `DEBUGGER` (break, step,
interrupt, protected error). `user_thread` is always delivered, since the runtime owns that
callback for its per-thread records. `Clone`, `Copy`, `Debug`, `Default`, `Eq`.

On `Runtime`: `fn set_hooks(&self, hooks: impl RuntimeHooks, set: HookSet)` installs `hooks` for
the slots in `set`, replacing any previous hooks and clearing the slots outside `set`;
`fn clear_hooks(&self)` removes them.
