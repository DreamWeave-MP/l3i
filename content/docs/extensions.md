+++
title = "Extensions and runtime plans"
description = "How a native crate declares its Luau surface once, how a plan resolves tags, atoms, slots and modules for every runtime it creates, and how the generated type definitions keep the declared API and the VM in step."
weight = 80

[extra]
kind = "guide"
+++

A native crate exposes its Luau surface as an `Extension`. The extension describes itself once,
without a VM; a `RuntimePlan` resolves everything runtime-specific and can instantiate any number
of runtimes; each runtime binds the declared callables and runs the extension's `install` for what
needs the live VM. The same Rust type may be tag 8 in one runtime, tag 17 in another, and untagged
in a third, with identical semantics: the direct path is an optimisation of the metatable path,
never a second API.

The pieces live in `l3i::extension`: the `Extension` trait, `ExtensionDescriptor`, `RuntimePlan`,
`RuntimePlanBuilder`, `RuntimePolicy`, `InstallContext`, `TagPolicy` and `CompilerTypePolicy`.

## The two phases

{{ api_signature(value="trait Extension: 'static { fn id(&self) -> &'static str; fn describe(&self, d: &mut ExtensionDescriptor) -> Result<()>; fn install(&self, cx: &mut InstallContext<'_>) -> Result<()> { Ok(()) } }") }}

| Phase | Runs | May touch a VM | Declares |
|---|---|---|---|
| `describe` | Once, at `finalize` | No | Identity, dependencies, modules and their members, userdata types and their members, services, capabilities, packed kinds, memory categories, native hooks, with every callable bound here |
| `install` | Once per runtime, in dependency order | Yes | Values that need the live VM or the resolved policy: a capability-gated module function, a userdata instance set into a module, a service read, runtime-owned state |

`id` is the stable public identity, such as `dream.archive`: dot-separated segments of letters,
digits, `_` and `-`, not starting with a digit. Other extensions name it in `requires`; profiler
and debug names derive from it (hyphens fold to underscores, so `dream.openmw-config` binds its
functions as `dream.openmw_config.<name>`). `install` has a default that does nothing; most
extensions leave it.

## What describe declares

`ExtensionDescriptor` collects everything the planner must know.

| Call | Declares |
|---|---|
| `requires(id)` | An extension that must be planned and installed before this one |
| `optional(id)` | An extension this one integrates with when present; it installs first if it is |
| `module(path)` | A native module at `path`, frozen by default; a second call with the same path returns the same declaration |
| `userdata::<T>(key)` | Ownership of the Rust type `T` under the stable string key, and its members |
| `augment_userdata::<T>(key)` | Members added to a type another extension owns, which this one must `require` |
| `sequence::<S>(key)`, `stream::<S>(key)` | A `Sequence` or `Stream` view over `S` as a userdata type, fully bound (see [Extension primitives](@/docs/primitives.md)) |
| `packed::<T>()` | A packed scalar kind this extension's members use, so every runtime registers `T` as the kind's owner |
| `service::<S>()` | A host service of Rust type `S` read at install time |
| `capability(name)` | A capability the policy must grant, or the plan does not finalize |
| `optional_capability(name)` | A capability the extension checks at install time without needing it |
| `memory_category(name)` | A symbolic memory category the planner maps to a Luau category number |
| `native_hooks(hooks)` | A `NativeCodeHooks` set for this extension's types (`jit` feature, see [Native code generation](@/docs/native-code.md)) |

### Modules

A `ModuleDecl` has three kinds of member. Each takes a `.signature(..)` or `.untyped()` and an
optional `.doc(..)`, and returns a builder that continues the chain.

| Member | Bound | The compiler |
|---|---|---|
| `function(name, callable)` | In every runtime, from the declared callable | Knows it as a function when the module is a compat global |
| `constant(name, CompileConstant)` | In every runtime, from the value | Folds it when the module is a compat global |
| `installed(name)` | By `install`, through `InstallContext::module(path)?.function(..)` or `.set(..)` | Knows a function; a `set` value is unknown |

```rust
d.module("@dream/archive")
    .doc("Archives.")
    .function("open", open).signature("(path: string) -> dream_archive_Archive")
    .constant("VERSION", CompileConstant::Number(3.0))
    .installed("client").signature("(options: { schema: dream_udp_Schema }) -> dream_udp_Client");
```

A module is read-only after install (`frozen`); `mutable()` turns that off for a stated reason.
An `installed` member the plan declares and `install` never provides fails instantiation, and
`install` cannot add a name the plan does not know, replace a declared function, or replace a
constant: the plan's definitions and compiler metadata describe the whole API before any runtime
exists.

### Userdata

`userdata::<T>(key)` returns a `UserdataBuilder<T>`. `T` implements `Userdata` (its `NAME` is the
metatable `__type`); the key is the script-facing identity, ASCII letters, digits, `.`, `_` and
`-`.

| Call | Member |
|---|---|
| `method(name, callable)` | `obj:name(...)` |
| `getter(name, callable)` | `obj.name`, read-only |
| `property(name, getter, setter)` | `obj.name` read and written; the returned declaration is the getter's, and one signature types both |
| `field::<H>(name)` | `obj.name` served by a `DirectField<T>` implementation straight into the destination register, with the same getter installed as the canonical property; makes the tag policy effectively `Required` |
| `metamethod(name, callable)` | `__tostring`, `__eq`, `__len`, ...; not a dispatch member |
| `tag(TagPolicy)` | Owner only: `Required`, `Preferred` (the default), or `Never` |
| `compiler_type(CompilerTypePolicy)` | Owner only: `Required`, `Preferred` (the default), or `Never` |
| `doc(text)` | A comment in the definitions |

Every member carries a signature or is marked `untyped()`; a plan with neither fails to finalize,
so a type is never `any` by accident. A method's signature is the Luau function type after the
name, with `self`: `(self, x: number): integer`. A getter, setter or field's signature is a type:
`number`, `string?`.

`TagPolicy::Required` means planning fails without a tag (direct fields and native lowering need
one). `CompilerTypePolicy::Required` means the type must take one of Luau's compiler userdata type
slots too, and needs a tag for it; a type whose methods lower natively declares both, and the
plan fails rather than leave that path interpreted.

### The type vocabulary

The signatures follow the runtime, where Luau's checker keeps `integer` and `number` apart:

| Value | Luau type |
|---|---|
| A packed scalar, a `Bits64`, an `Integer` result, a `CompileConstant::Integer` | `integer` |
| A count, a size, a plain Rust integer result, an `f64` | `number` |
| A userdata type with key `k` | Its class name: `k` with every non-identifier character replaced by `_`, so `dream.udp.Client` is `dream_udp_Client` |
| A module at path `p` | `Module_` plus the class name of `p`, so `@dream/udp` is `Module__dream_udp` |
| A sequence or stream element | What `item_type(..)` names, `any` until it does |

A member may name a module declared later in the plan (`() -> Module__dream_archive_ba2`): the
renderer orders module types by reference.

### Callables and state

Callables are `Clone`. One plan binds each member once in every runtime it creates, so whatever a
callable captures is shared by every one of those runtimes by construction: an `Rc<RefCell<_>>`
captured in `describe` is one cell for all of them. Closures that capture nothing, `Rc`s and
`Clone` data qualify.

Mutable per-runtime state belongs in `InstallContext::insert_state`, never in a capture. It is
one value per Rust type per extension, dropped before the VM closes, so a `Value` it holds is
still valid in its destructor. `Runtime::state_of::<S>(owner)` reads it from the host;
`Runtime::insert_state` and `Runtime::host_state` are the host's own, separate store.

## The install context

`InstallContext` is the per-runtime view one extension receives in `install`.

| Call | Gives |
|---|---|
| `runtime()`, `plan()`, `extension_id()` | The runtime being built, its plan, and the extension being installed |
| `module(path)?` | The `ModuleInstaller` for a module this extension declared, to `function(name, callable)?` or `set(name, &value)?` declared `installed` members |
| `service::<S>()?` | A service the extension declared with `service::<S>()`; reading an undeclared one is a logic error |
| `has_capability(name)?` | Whether the policy grants a capability the extension declared as required or optional; an undeclared name is a logic error, so a misspelling never reads as "not granted" |
| `require_capability(name)?` | A permission error unless granted |
| `memory_category(name)?` | The `MemoryCategory` a declared symbolic name resolved to |
| `insert_state(value)`, `state::<S>()` | This extension's runtime state |
| `state_of::<S>(owner)?` | Another extension's state; `owner` must be this extension, a declared `requires`, or a declared `optional` that is in the plan, because the dependency graph, not installation order, guarantees it installed first |

## Building a plan

{{ api_signature(value="fn finalize(self) -> Result<Rc<RuntimePlan>>") }}

```rust
let plan = RuntimePlan::builder()
    .policy(RuntimePolicy::new().capability("network.transport").compat_global("@dream/core", "core"))
    .service(Greeting("hello".to_owned()))
    .extension(Tools)
    .extension(Core)
    .pin_tag("dream.tests.Counter", 99)
    .finalize()?;
```

`RuntimePlanBuilder` takes a `policy`, any number of `service(value)`s matched by Rust type,
`extension(..)` or `boxed_extension(..)`, `pin_tag(key, tag)` to fix a type's tag in every
runtime, and `network_clock(clock)` for the built-in network bridge. Registration order does not
matter.

`finalize` runs every `describe` and resolves the plan, in this order:

1. Adds l3i's own `dream.udp` bridge; the id is reserved, and an extension claiming it fails the plan.
2. Validates every id, rejects an id registered twice and two ids that fold to one debug prefix (`dream.a-b` and `dream.a_b`).
3. Orders extensions by their dependency graph (Kahn's algorithm with lexicographic tie-breaking, so the order depends on the graph only); a missing `requires` or a cycle fails, naming it.
4. Resolves modules: unique paths, unique member names, valid identifiers, no two paths folding to one `Module_` type name, and compat globals one per module and one module per global.
5. Merges userdata: owners first, then augmentations in dependency order. Two owners for one key, one Rust type under two keys, two types sharing one `Userdata::NAME`, an augmentation with the wrong Rust type or without requiring the owner, and a member declared twice on one type all fail.
6. Checks that every member has a signature or is `untyped()`, and that no two keys fold to one class name.
7. Assigns tags: pinned first, then `Required`, then `Preferred` while tags last, from the policy's `first_tag` up to `TAG_LIMIT`, in key order. A `Required` type without a tag fails; a `Never` type with a direct field fails.
8. Assigns Luau's 32 compiler userdata type slots (`COMPILER_TYPE_CAPACITY`): `Required` types first, then `Preferred`, in tag order. A `Required` type without a slot or without a tag fails.
9. Assigns atoms densely from 1 over the sorted method, getter and setter names. A direct field name gets no atom, since Luau rewrites every `obj.name` whose key has an atom into the direct-access opcode, which bypasses the field table; a field whose name is a method or property on another type is served through a plan slot instead (`ResolvedMember::through_slot`).
10. Lays out direct slots: every method, getter and setter of a tagged type takes one, densely.
11. Resolves memory categories to `1..=255`.
12. Checks that every declared service is in the plan and every required capability is granted (a permission error, not a logic error).
13. Validates packed kinds: numbers in the host range, one type per number across the plan, and l3i's own numbers off limits.

Everything the VM or the generated definitions would choke on is rejected here, so
`Runtime::from_plan` has nothing left to discover. The plan is immutable and exposes what it
resolved: `installation_order()`, `userdata_by_key(key)`, `tag_of(key)`, `atom_of(member)`,
`modules()`, `memory_category(name)`, `service::<S>()`, `packed_kinds()`, `debug_roots()`.

## Instantiating runtimes

{{ api_signature(value="fn Runtime::from_plan(plan: &Rc<RuntimePlan>) -> Result<Runtime>") }}

`from_plan` builds the VM from the policy with the plan's atom catalogue and debug roots,
registers l3i's and the plan's packed kinds, names the compiler-typed classes to the code
generator, registers every declared type with its merged members (tagged or untagged as the plan
resolved it), wires the planned direct members to one set of generic VM callbacks, registers the
direct fields, opens the declared modules with their functions and constants, runs every
extension's `install` in order, freezes the modules, registers them for `require`, exposes the
compat globals, derives the compiler's known-library metadata, and sandboxes the globals when the
policy asks. Every runtime from one plan gets the same tags, atoms and slots.

A planned runtime's shape is frozen: `register_packed` and `set_compile_options` are refused with
a logic error; both stay available on a hand-assembled `Runtime::new()`. `Runtime::plan()` returns
the plan, and `Runtime::type_definitions()` the same text the plan renders.

## The policy

`RuntimePolicy` is the VM configuration and what scripts may do. `RuntimePolicy::new()` gives the
defaults; each method takes and returns the policy.

| Method | Default | Effect |
|---|---|---|
| `standard_libraries(bool)` | `true` | Opens Luau's standard libraries |
| `sandbox(bool)` | `false` | `luaL_sandbox` after installation: globals read-only, safe environment on |
| `limits(Limits)` | none | `execution_time` per outermost call and `memory_bytes` for the watchdog |
| `profiler(bool)` | `false` | The profiler |
| `first_tag(tag)` | `1` | The first tag the planner hands out; lower tags stay free for the host |
| `capability(name)` | none | Grants a capability to extensions |
| `compat_global(path, global)` | none | Also exposes the module at `path` as a global, host policy during migrations |
| `debug_root(root)` | none | Extra debug-name roots beside the ones extension ids imply |
| `native_code(NativeCodePolicy)` | `None` | Native code generation (`jit` feature) |

A frozen module exposed as a compat global also becomes a compiler-known library: its constants
fold and its members are typed at compile time. That mechanism keys on a global name, so a module
reached only through `require` is typed by the analyzer but not folded by the compiler.

## Type definitions and the analysis gate

{{ api_signature(value="fn type_definitions(&self) -> String") }}

The plan renders a `.d.luau` for the whole composition, owner plus every augmentation, in Luau's
`declare extern type` grammar. For the worked example below, with `@dream/core` exposed as the
global `core`, the relevant part reads:

```luau
-- dream.tests.Counter (owned by dream.core; tag 2)
-- A counter.
declare extern type dream_tests_Counter with
    function get(self): number
    function add(self, n: number)
    twice: number
    value: integer
    function double(self): number
end

-- module @dream/core (provided by dream.core)
-- Counters.
export type Module__dream_core = {
    new: (n: number) -> dream_tests_Counter,
    ANSWER: number,
    LIMIT: integer,
}
declare core: Module__dream_core
```

A sequence view adds `function __len(self): number`, an indexer `[number]: T?`, and `__iter`
typed with its element; a stream adds `__iter` only. Untyped members render as
`(self, ...any): any`, `(...any) -> ...any`, or `any`.

Three more calls serve the analysis frontend (`analysis` feature):

- `module_stub(path)` is the strict module the analyzer reads for `require("<path>")`: it returns a value of the module's declared type.
- `analysis_sources(inner)` wraps a `SourceProvider` so the plan's module paths resolve to those stubs and everything else comes from `inner`.
- `check_definitions()` is the gate every extension crate's tests run: it loads the definitions into Luau's frontend and type checks a strict script requiring every module. A signature string that is not Luau, or one naming a type that does not exist, fails with the frontend's diagnostics attributed to the declaration.

```rust
#[test]
fn declared_types_check() {
    let plan = RuntimePlan::builder().extension(Core).extension(Tools).finalize().unwrap();
    plan.check_definitions().unwrap();
}
```

`tests/typed_definitions.rs` runs that gate and strict scripts against every built-in module
through `require`, with no compatibility global, so the declared API and the runtime cannot drift
apart.

## Rules the first extensions ran into

- Direct fields carry nil, booleans, numbers, integers, and vectors: Luau's direct-field API has no string setter, so a text field is a getter. Return borrowed text from a getter with `call.push(&text)?` and `StackResults`; a `Return` type cannot borrow from the arguments.
- Module types in the definitions are ordered by reference, so a member may name a module declared later in the plan (`() -> Module__dream_archive_ba2`).
- `Runtime::eval::<R>(source)` runs a chunk and reads what it returns (`f64`, a tuple, `()`), the shape a benchmark harness wants; `load_function` stays for chunks that return a closure. A harness must not hold `Runtime::stack()` across `load_function` or `eval`, which lease the root stack themselves.
- One frame per scope: inside a bound function, read the arguments before opening a frame, or open it from the call; a second frame on the same scope panics with that message.
- `Frame::check(n)` reserves stack for a bulk push (`n` is a `usize` count); the type-error constructors on `ValueView` cover a type, a union in words, and a context prefix (a field path or an API name) without the slot index.
- A `with_required`/`with_optional` body sees the value's slot and nothing else: it can read a scalar or a borrowed string, not walk a table (no frame is reachable there). Tables go through `required_table`/`optional_table`, or the `_expecting` forms when the non-table case should read in the option's own words (`optional_table_expecting("dataDirs", "an array of strings", ..)`).
- Inside `required_table`'s body, an error comes back prefixed with the reader's context and key (`add.inputs: ...`) unless it already starts with that path: a `field_type_error` spelled with the full path reads flat (`ini.importMaps.dataDirs[2]: expected a string, got number`), and a nested `Options::read` under the field's context keeps one segment per level.
- Cargo has no optional dev-dependencies, so a crate that tests its plan with `check_definitions` (feature `analysis`) either pays the analysis build on every `cargo test` or declares l3i as an optional normal dependency with a test feature, `luau-analysis = ["luau", "l3i/analysis"]`, and runs its typed tests with `--features luau-analysis`.

## A worked example

Two extensions: `dream.core` owns a counter type and provides `@dream/core`; `dream.tools`
requires it, adds a method to the counter, and provides `@dream/tools`. The plan is built with
`Tools` registered first, and the dependency graph still installs `Core` first.

```rust
use std::cell::Cell;

use l3i::direct::field::{DirectField, FieldValue};
use l3i::extension::{Extension, ExtensionDescriptor, InstallContext, RuntimePlan, RuntimePolicy, TagPolicy};
use l3i::runtime::MemoryCategory;
use l3i::source::CompileConstant;
use l3i::userdata::{Owned, Userdata};
use l3i::{Result, Runtime};

struct Counter {
    value: Cell<i64>,
}

// SAFETY: plain Rust data; the destructor never touches the Lua API.
unsafe impl Userdata for Counter {
    const NAME: &'static str = "dream.tests.Counter";
}

/// `counter.value`, served straight into the destination register.
struct ValueField;

impl DirectField<Counter> for ValueField {
    fn get(counter: &Counter) -> FieldValue {
        FieldValue::Integer(counter.value.get())
    }
}

/// Per-runtime state, stored at install and never captured.
struct CoreState {
    category: MemoryCategory,
}

struct Core;

impl Extension for Core {
    fn id(&self) -> &'static str {
        "dream.core"
    }

    fn describe(&self, d: &mut ExtensionDescriptor) -> Result<()> {
        let mut counter = d.userdata::<Counter>("dream.tests.Counter");
        counter.tag(TagPolicy::Preferred).doc("A counter.");
        counter.method("get", |c: &Counter| c.value.get()).signature("(self): number");
        counter.method("add", |c: &Counter, n: i64| c.value.set(c.value.get() + n)).signature("(self, n: number)");
        counter
            .property("twice", |c: &Counter| c.value.get() * 2, |c: &Counter, v: i64| c.value.set(v / 2))
            .signature("number");
        counter.field::<ValueField>("value").signature("integer");
        counter.metamethod("__tostring", |c: &Counter| format!("Counter({})", c.value.get()));
        d.module("@dream/core")
            .doc("Counters.")
            .function("new", |n: i64| Owned(Counter { value: Cell::new(n) }))
            .signature("(n: number) -> dream_tests_Counter")
            .constant("ANSWER", CompileConstant::Number(42.0))
            .constant("LIMIT", CompileConstant::Integer(7));
        d.memory_category("dream.core");
        Ok(())
    }

    fn install(&self, cx: &mut InstallContext<'_>) -> Result<()> {
        let category = cx.memory_category("dream.core")?;
        cx.insert_state(CoreState { category });
        Ok(())
    }
}

struct Tools;

impl Extension for Tools {
    fn id(&self) -> &'static str {
        "dream.tools"
    }

    fn describe(&self, d: &mut ExtensionDescriptor) -> Result<()> {
        d.requires("dream.core");
        d.augment_userdata::<Counter>("dream.tests.Counter")
            .method("double", |c: &Counter| c.value.get() * 2)
            .signature("(self): number");
        d.module("@dream/tools").function("version", || 2i64).signature("() -> number");
        Ok(())
    }
}

fn main() -> Result<()> {
    let plan = RuntimePlan::builder()
        .policy(RuntimePolicy::new().compat_global("@dream/core", "core"))
        .extension(Tools)
        .extension(Core)
        .finalize()?;
    assert_eq!(plan.installation_order(), ["dream.core", "dream.tools", "dream.udp"]);
    assert_eq!(plan.tag_of("dream.tests.Counter"), Some(1));

    let runtime = Runtime::from_plan(&plan)?;
    runtime.exec(
        r#"
        local tools = require("@dream/tools")
        local c = core.new(5)
        c:add(2)
        assert(c:get() == 7 and c:double() == 14)
        assert(c.twice == 14)
        c.twice = 20
        assert(c:get() == 10 and c.value == 10i)
        assert(core.ANSWER == 42 and core.LIMIT == 7i and tools.version() == 2)
        assert(tostring(c) == "Counter(10)")
        "#,
    )?;
    assert!(runtime.state_of::<CoreState>("dream.core").is_some());
    // A frozen module refuses writes.
    assert!(runtime.exec("core.new = nil").is_err());
    Ok(())
}
```

Tags follow key order, so the counter gets tag 1 and the network bridge's `dream.udp.Client`,
which keys after it, tag 2. A second plan with `TagPolicy::Never` runs the same script untagged,
and a plan with `first_tag(40)` gives the counter tag 40; the script does not change.

{% callout(kind="tip", title="Reaching a script's value from Rust") %}
`l3i::userdata::check_receiver::<T>(view)` reads the `&T` behind a userdata slot in any runtime the
plan created, whatever tag it resolved to. See [Userdata](@/docs/userdata.md).
{% end %}
