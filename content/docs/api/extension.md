+++
title = "Extensions and primitives"
description = "Extension, ExtensionDescriptor and the declaration builders; RuntimePlan, RuntimePolicy and InstallContext; packed scalars and buffer layouts; sequences and streams; the dream.quat, dream.raster, dream.udp, dream.soft_render and dream.tcp extensions."
weight = 250

[extra]
kind = "api"
+++

Modules `l3i::extension`, `l3i::packed`, `l3i::sequence`, `l3i::quat`, `l3i::raster`,
`l3i::udp` and, with the `soft-render` feature, `l3i::soft_render`.
[Extensions](@/docs/extensions.md), [Primitives](@/docs/primitives.md) and
[Built-in extensions](@/docs/builtin-extensions.md) are the guides.

## The planner

Module `l3i::extension`: describe once, resolve per runtime, instantiate after planning. An
`Extension` declares what it provides in `describe`, callables included, without touching a VM.
`RuntimePlanBuilder::finalize` orders extensions by their dependency graph (deterministically),
merges owners with augmenters into one type per key, assigns tags (pinned, then `Required`,
then `Preferred` while tags last), allocates the compiler's 32 userdata type slots the same way,
assigns atoms densely, lays out direct slots, resolves memory categories, checks services and
capabilities, and validates every name the definitions or the VM would choke on.
`Runtime::from_plan` then builds a VM, registers the merged types, wires the planned direct
members to one set of generic callbacks, opens the declared modules, runs every `install` in
order, freezes modules, registers them for `require`, derives compiler metadata for compat
globals, and publishes. A plan is immutable and instantiates any number of runtimes; each gets
its own tags, atoms and direct plan.

{{ api_signature(value="trait Extension: 'static") }}

| Method | Meaning |
|---|---|
| `fn id(&self) -> &'static str` | The stable public identity, such as `dream.archive`: dot-separated segments of letters, digits, `_` and `-`, not starting with a digit. Other extensions name it in `requires`; profiler and debug identities derive from it |
| `fn describe(&self, descriptor: &mut ExtensionDescriptor) -> Result<()>` | Declares everything the planner must know. Must not touch a VM or create Lua values; the callables run later, in every runtime the plan instantiates |
| `fn install(&self, context: &mut InstallContext<'_>) -> Result<()>` | Default: nothing. Runs once per runtime after the declared types and modules exist, for what needs the live VM or the resolved policy: services, capability-gated module functions, module values built from Lua objects, runtime-owned state |

```rust
use std::cell::Cell;
use l3i::extension::{Extension, ExtensionDescriptor, InstallContext, RuntimePlan, RuntimePolicy};
use l3i::source::CompileConstant;
use l3i::userdata::{Owned, Userdata};
use l3i::{Result, Runtime};

struct Counter { value: Cell<i64> }

unsafe impl Userdata for Counter {
    const NAME: &'static str = "dream.tests.Counter";
}

struct Core;

impl Extension for Core {
    fn id(&self) -> &'static str { "dream.core" }

    fn describe(&self, d: &mut ExtensionDescriptor) -> Result<()> {
        let mut counter = d.userdata::<Counter>("dream.tests.Counter");
        counter.doc("A counter.");
        counter.method("get", |c: &Counter| c.value.get()).signature("(self): number");
        counter.method("add", |c: &Counter, n: i64| c.value.set(c.value.get() + n)).untyped();
        counter
            .property("twice", |c: &Counter| c.value.get() * 2, |c: &Counter, v: i64| c.value.set(v / 2))
            .signature("number");
        counter.metamethod("__tostring", |c: &Counter| format!("Counter({})", c.value.get()));
        d.module("@dream/core")
            .doc("Counters.")
            .function("new", |n: i64| Owned(Counter { value: Cell::new(n) }))
            .signature("(n: number) -> dream_tests_Counter")
            .constant("ANSWER", CompileConstant::Number(42.0));
        d.memory_category("dream.core");
        Ok(())
    }

    fn install(&self, cx: &mut InstallContext<'_>) -> Result<()> {
        let _category = cx.memory_category("dream.core")?;
        Ok(())
    }
}

fn main() -> Result<()> {
    let plan = RuntimePlan::builder().policy(RuntimePolicy::new()).extension(Core).finalize()?;
    let runtime = Runtime::from_plan(&plan)?;
    runtime.exec("local core = require('@dream/core') local c = core.new(5) c:add(2) assert(c:get() == 7) assert(c.twice == 14)")?;
    Ok(())
}
```

### ExtensionDescriptor

{{ api_signature(value="struct ExtensionDescriptor") }}

Everything one extension declares in `describe`. `Debug`.

| Method | Meaning |
|---|---|
| `fn id(&self) -> &'static str` | |
| `fn requires(&mut self, id: &str) -> &mut Self` | Another extension that must be planned and installed before this one |
| `fn optional(&mut self, id: &str) -> &mut Self` | Another extension this one integrates with when present; it installs first if it is |
| `fn module(&mut self, path: &str) -> &mut ModuleDecl` | The native module at `path` (frozen by default), created on first mention |
| `fn userdata<T: Userdata>(&mut self, key: &str) -> UserdataBuilder<'_, T>` | Ownership of the userdata type `T` under the stable `key` (ASCII letters, digits, `.`, `_`, `-`) |
| `fn augment_userdata<T: Userdata>(&mut self, key: &str) -> UserdataBuilder<'_, T>` | Members for a type another extension owns, which this one must `require` |
| `fn sequence<S: SequenceSource>(&mut self, key: &str) -> UserdataBuilder<'_, Sequence<S>>` | A `Sequence` over `S`: a userdata type with `toTable`, `#`, `[i]` and `for`, fully bound |
| `fn stream<S: StreamSource>(&mut self, key: &str) -> UserdataBuilder<'_, Stream<S>>` | A `Stream` over `S` (`for` only) |
| `fn service<S: 'static>(&mut self) -> &mut Self` | A host service of type `S` this extension reads at install time |
| `fn capability(&mut self, name: &str) -> &mut Self` | A capability the policy must grant, such as `filesystem.read` |
| `fn optional_capability(&mut self, name: &str) -> &mut Self` | A capability checked at install time but not needed to install |
| `fn memory_category(&mut self, name: &str) -> &mut Self` | A symbolic memory category the planner maps to a Luau category number |
| `fn packed<T: PackedScalar>(&mut self) -> &mut Self` | A packed kind this extension's members use, so every runtime registers `T` as its owner |
| `fn type_alias(&mut self, name: &str, definition: impl Into<String>) -> &mut Self` | A named Luau type the signatures refer to, for values that are plain tables (a parse tree, an options record); rendered `export type <name> = <definition>` ahead of every module, so aliases may refer to one another in any order. The plan refuses a name that is not an identifier, is declared twice, or is a userdata class name |
| `fn widen_type_alias(&mut self, name: &str, member: impl Into<String>) -> &mut Self` | Widens a union another extension declares: rendered `<definition> \| <member>` when the declaring extension is in the plan, and ignored when it is not. How an integration adds its handle type to a parameter of an extension that cannot know it, such as `dream_tcp_Watchable` |
| `fn native_hooks(&mut self, hooks: impl NativeCodeHooks) -> &mut Self` | Feature `jit`: lowering hooks for this extension's types |
| `fn dependencies(&self)`, `optional_dependencies`, `modules`, `owned_userdata`, `augmentations`, `services`, `capabilities`, `optional_capabilities`, `memory_categories`, `packed_kinds`, `type_aliases`, `type_alias_widenings`, `native_hook_sets` | Read back what was declared |

### UserdataBuilder and MemberDecl

{{ api_signature(value="struct UserdataBuilder<'a, T: Userdata>") }}

The typed view of a `UserdataDecl` under construction: declares a member and binds its callable
in one call. Callables are `Clone` because a plan instantiates any number of runtimes and binds
each member once per VM; whatever a callable captures is shared by every one of those runtimes.
Mutable per-runtime state belongs in `InstallContext::insert_state`.

| Method | Meaning |
|---|---|
| `fn tag(&mut self, policy: TagPolicy) -> &mut Self` | Owner only; augmentations inherit |
| `fn compiler_type(&mut self, policy: CompilerTypePolicy) -> &mut Self` | Owner only. A type with native lowering declares `Required` |
| `fn doc(&mut self, doc: impl Into<String>) -> &mut Self` | |
| `fn decl(&mut self) -> &mut UserdataDecl` | The declaration being built |
| `fn method<F: Binding<M> + Clone + 'static, M: 'static>(&mut self, name: &str, callable: F) -> &mut MemberDecl` | `obj:name(...)` |
| `fn getter<G: Binding<MG> + Clone + 'static, MG: 'static>(&mut self, name: &str, getter: G) -> &mut MemberDecl` | `obj.name`, read-only |
| `fn property<G, MG, S, MS>(&mut self, name: &str, getter: G, setter: S) -> &mut MemberDecl` | A getter/setter pair; the returned declaration is the getter's, and a signature on it is the property's type |
| `fn field<H: DirectField<T>>(&mut self, name: &str) -> &mut MemberDecl` | A direct primitive field served by `H`, installed as the canonical property too. Makes the tag policy effectively `Required` |
| `fn metamethod<F: Binding<M> + Clone + 'static, M: 'static>(&mut self, name: &str, callable: F) -> &mut Self` | `__tostring`, `__eq`, `__len` and the rest: not a dispatch member |
| `fn item_type(&mut self, ty: &str) -> &mut Self` | On `UserdataBuilder<Sequence<S>>` and `UserdataBuilder<Stream<S>>` only: the Luau type of one element, so the definitions declare `#`, `[i]`, `for` and `toTable` with it instead of `any` |

{{ api_signature(value="struct MemberDecl { pub name: String, pub kind: MemberKind, pub signature: Option<String>, pub untyped: bool, pub doc: Option<String>, pub contributor: &'static str }") }}

One declared member. `fn signature(&mut self, signature: impl Into<String>) -> &mut Self` sets
the Luau signature or type for definition output (`(self, x: number): integer` for a method, a
type for a getter, setter or field); `fn untyped(&mut self) -> &mut Self` leaves it without a
type on purpose (the definitions say `...any`); `fn doc(&mut self, doc: impl Into<String>) ->
&mut Self`. A member with neither a signature nor `untyped` fails the plan: types are never
accidental. `Clone`, `Debug`.

{{ api_signature(value="enum MemberKind { Method, Getter, Setter, Field }") }}

`Method` is reachable by `__namecall` and, on a tagged type, the direct namecall path; `Getter`
and `Setter` through bound accessors; `Field` through a direct primitive getter, which needs a
tag. `Clone`, `Copy`, `Debug`, `Eq`, `Hash`.

{{ api_signature(value="enum TagPolicy { Required, Preferred, Never }") }}

How much a type wants a Luau tag: planning fails without one; take one when free and fall back
to the canonical untagged metatable otherwise; never tag. Default `Preferred`. `Clone`, `Copy`,
`Debug`, `Eq`.

{{ api_signature(value="enum CompilerTypePolicy { Required, Preferred, Never }") }}

{{ api_signature(value="const COMPILER_TYPE_CAPACITY: usize = 32") }}

Whether a type takes one of the compiler's userdata type slots. Luau's compiler and code
generator tell at most 32 userdata types apart per VM; a type whose methods lower natively needs
one (and a tag), and a plan that cannot give a `Required` type its slot does not finalize rather
than leave that path interpreted. Default `Preferred`.

{{ api_signature(value="struct UserdataDecl { pub key: String, pub type_id: TypeId, pub type_name: &'static str, pub tag: TagPolicy, pub compiler_type: CompilerTypePolicy, pub members: Vec<MemberDecl>, pub doc: Option<String>, pub view: Option<ViewDecl>, .. }") }}

A userdata type one extension owns or augments. `Clone`, `Debug`.

{{ api_signature(value="struct ViewDecl { pub kind: ViewKind, pub item: String }") }}

{{ api_signature(value="enum ViewKind { Sequence, Stream }") }}

What a sequence or stream view is, for the generated definitions.

{{ api_signature(value="struct ServiceRequirement { pub type_id: TypeId, pub type_name: &'static str }") }}

A host service an extension needs, matched by Rust type. `Clone`, `Debug`.

### ModuleDecl

{{ api_signature(value="struct ModuleDecl { pub path: String, pub frozen: bool, pub doc: Option<String>, .. }") }}

A native module the extension provides, `require`d by its path (an optional leading `@`, then
ASCII letters, digits, `/`, `.`, `_`, `-`). `Clone`, `Debug`.

| Method | Meaning |
|---|---|
| `fn frozen(&mut self) -> &mut Self`, `fn mutable(&mut self) -> &mut Self` | Read-only after install is the default; a mutable module needs a stated reason |
| `fn doc(&mut self, doc: impl Into<String>) -> &mut Self` | |
| `fn members(&self) -> &[ModuleMemberDecl]` | |
| `fn function<F: Binding<M> + Clone + 'static, M: 'static>(&mut self, name: &str, callable: F) -> ModuleMemberBuilder<'_>` | A function bound in every runtime |
| `fn constant(&mut self, name: &str, value: CompileConstant) -> ModuleMemberBuilder<'_>` | A constant the compiler may fold when the module is a known global library |
| `fn installed(&mut self, name: &str) -> ModuleMemberBuilder<'_>` | A member `install` provides per runtime through `InstallContext::module(..).function` or `.set`; instantiation fails if it does not |

{{ api_signature(value="struct ModuleMemberBuilder<'m>") }}

The member just declared. `fn signature(self, signature: impl Into<String>) -> Self`,
`fn doc(self, doc: impl Into<String>) -> Self`, `fn untyped(self) -> Self`, `fn module(self) ->
&'m mut ModuleDecl`, and `function`, `constant`, `installed` to continue the chain.

{{ api_signature(value="struct ModuleMemberDecl { pub name: String, pub kind: ModuleMemberKind, pub signature: Option<String>, pub untyped: bool, pub doc: Option<String>, .. }") }}

{{ api_signature(value="enum ModuleMemberKind { Function, Constant(CompileConstant), Installed }") }}

The vocabulary for signatures follows the runtime, where Luau's checker keeps `integer` and
`number` apart: a packed value, a `Bits64` or an `Integer` result is `integer`; a count, a size
or an `f64` is `number`; a class is its generated name, the key with dots replaced
(`dream_udp_Client`); a module type is `Module__dream_archive_ba2`.

### RuntimePolicy

{{ api_signature(value="struct RuntimePolicy { pub debug_roots: Vec<String>, pub standard_libraries: bool, pub sandbox: bool, pub limits: Limits, pub profiler: bool, pub pointer_encoding: bool, pub first_tag: RuntimeTag, pub capabilities: BTreeSet<String>, pub compat_globals: Vec<(String, String)>, pub native_code: Option<NativeCodePolicy> }") }}

Execution policy for runtimes made from a plan: the VM's configuration and what scripts may do.
`native_code` exists with the `jit` feature. `Clone`, `Debug`, `Default`.

| Field | Default | Meaning |
|---|---|---|
| `debug_roots` | empty | Extra roots beside the ones extension identities imply |
| `standard_libraries` | `true` | |
| `sandbox` | `false` | `luaL_sandbox` after installation |
| `limits` | none | Watchdog limits |
| `profiler` | `false` | |
| `pointer_encoding` | `true` | |
| `first_tag` | 1 | The first tag the planner hands out; lower tags stay free for the host |
| `capabilities` | empty | Granted to extensions, checked at finalisation and at install |
| `compat_globals` | empty | Module paths also exposed as globals, host policy during migrations: `("@dream/archive", "dreamArchive")`. Frozen modules exposed this way also become compiler-known libraries |
| `native_code` | `None` | Native code generation for scripts |

Builder methods, each `self -> Self`: `new()`, `capability(name)`, `compat_global(module_path,
global)`, `sandbox(enabled)`, `standard_libraries(enabled)`, `limits(limits)`,
`profiler(enabled)`, `first_tag(tag)`, `debug_root(root)`, `native_code(policy)`; and
`fn grants(&self, capability: &str) -> bool`.

{{ api_signature(value="struct NativeCodePolicy { pub mode: NativeCodeMode, pub max_total_size: usize, pub record_counters: bool, pub nop_padding: bool, pub hooks: Vec<Rc<dyn NativeCodeHooks>> }") }}

Feature `jit`: native code settings a plan reuses for every runtime it creates. `hooks` are the
host's lowering hooks, asked after the defaults and before the extensions' own. `Clone`,
`Debug`, `Default`.

### RuntimePlanBuilder and RuntimePlan

{{ api_signature(value="struct RuntimePlanBuilder") }}

Collects extensions, services and policy. `Default`.

| Method | Meaning |
|---|---|
| `fn new() -> Self` | |
| `fn policy(self, policy: RuntimePolicy) -> Self` | |
| `fn service<S: 'static>(self, service: S) -> Self` | A host service extensions may look up by type at install time |
| `fn pin_tag(self, key: &str, tag: RuntimeTag) -> Self` | Pins the type with stable `key` to `tag` in every runtime from this plan |
| `fn network_clock(self, clock: udp::Clock) -> Self` | The transport clock the network bridge reads instead of a monotonic clock started at creation; scripts never see or set it |
| `fn extension(self, extension: impl Extension) -> Self`, `fn boxed_extension(self, extension: Box<dyn Extension>) -> Self` | |
| `fn finalize(self) -> Result<Rc<RuntimePlan>>` | Adds l3i's `dream.udp` bridge, runs every `describe`, resolves and freezes |

Finalize errors (`Error::Logic`, or `Error::Permission` for a missing capability): an extension
registered twice or claiming the reserved `dream.udp` id; two ids folding to one debug prefix; a
missing or cyclic `requires`; a module provided twice, a member declared twice, a compat global
for a path nothing provides; a type owned twice, a Rust type under two keys, two types sharing a
`Userdata::NAME` or a generated class name; an augmentation of a type its extension does not
require; a member with neither a signature nor `untyped()`; a key, path, member or global spelled
in a way Luau or the definitions would refuse; no tag left for a `Required` type, a pinned tag on
a `Never` type, direct fields on a `Never` type; no compiler slot for a `Required` type; more
than 4096 atoms or slots; more than 255 memory categories; a missing service; two types on one
packed kind.

{{ api_signature(value="struct RuntimePlan") }}

An immutable, reusable description of one runtime composition. `Debug`.

| Method | Meaning |
|---|---|
| `fn builder() -> RuntimePlanBuilder` | |
| `fn policy(&self) -> &RuntimePolicy` | |
| `fn installation_order(&self) -> Vec<&'static str>` | Extension ids in installation order |
| `fn descriptors(&self) -> &[ExtensionDescriptor]` | |
| `fn userdata(&self) -> &[ResolvedUserdata]`, `fn userdata_by_key(&self, key: &str) -> Option<&ResolvedUserdata>`, `fn userdata_of_type<T: 'static>(&self) -> Option<&ResolvedUserdata>` | |
| `fn modules(&self) -> &[ResolvedModule]` | |
| `fn tag_of(&self, key: &str) -> Option<RuntimeTag>` | The tag runtimes from this plan give the type with `key` |
| `fn atom_of(&self, member: &str) -> Option<Atom>`, `fn atoms(&self) -> &AtomCatalogue` | |
| `fn memory_category(&self, name: &str) -> Option<MemoryCategory>`, `fn memory_categories(&self) -> &BTreeMap<String, MemoryCategory>` | Symbolic categories resolve to `1..=255` |
| `fn service<S: 'static>(&self) -> Option<Rc<S>>` | |
| `fn packed_kinds(&self) -> &[PackedKind]` | The kinds the plan's extensions declared (l3i's own are implicit) |
| `fn debug_roots(&self) -> &[Box<str>]` | |
| `fn type_definitions(&self) -> String` | The `.d.luau` for every module and userdata type after composition, in Luau's `declare extern type` grammar |
| `fn module_stub(&self, path: &str) -> Option<String>` | The analysis stub for the module at `path`: a strict module returning a value of the module's declared type |
| `fn analysis_sources<P: SourceProvider>(self: &Rc<Self>, inner: P) -> PlanSources<P>` | Feature `analysis`: a source provider serving this plan's modules as stubs and everything else from `inner` |
| `fn check_definitions(self: &Rc<Self>) -> Result<()>` | Feature `analysis`: proves the declared types with Luau's frontend. The definitions parse and type check, and a strict script requiring every module checks against the stubs; a signature that is not Luau, or names a type that does not exist, fails with the frontend's diagnostics attributed to the declaration. Every extension crate's tests should call this |

{{ api_signature(value="struct ResolvedUserdata { pub key: String, pub type_id: TypeId, pub type_name: &'static str, pub owner: &'static str, pub policy: TagPolicy, pub tag: Option<RuntimeTag>, pub compiler_type: CompilerTypePolicy, pub bytecode_type: Option<u8>, pub members: Vec<ResolvedMember>, pub doc: Option<String>, pub view: Option<ViewDecl>, .. }") }}

A type after ownership and augmentations merged: `tag` is what this plan assigned or `None` for
the canonical untagged metatable; `bytecode_type` is `TAGGED_USERDATA_BASE + slot` when the plan
gave it a compiler slot. `fn member(&self, name: &str) -> Option<&ResolvedMember>` and `fn
has_direct_slots(&self) -> bool`. `Clone`, `Debug`.

{{ api_signature(value="struct ResolvedMember { pub name: String, pub kind: MemberKind, pub atom: Atom, pub slot: Option<u16>, pub through_slot: bool, pub signature: Option<String>, pub untyped: bool, pub doc: Option<String>, pub contributor: &'static str }") }}

A member with its VM-local resolution. Every method, getter and setter of a tagged type has a
slot; a direct field has none unless `through_slot`: its name is also a method, getter or setter
elsewhere in the plan, and since Luau rewrites every `obj.name` whose key has an atom into the
direct-access opcode, such a field is served through a slot like a getter rather than failing the
plan. `fn access_kind(&self) -> AccessKind`. `Clone`, `Debug`.

{{ api_signature(value="struct ResolvedModule { pub path: String, pub frozen: bool, pub provider: &'static str, pub doc: Option<String>, pub global: Option<String>, pub members: Vec<ModuleMemberDecl> }") }}

A module after planning; `global` is the compatibility global the policy exposes it as.
`Clone`, `Debug`.

### InstallContext and ModuleInstaller

{{ api_signature(value="struct InstallContext<'r>") }}

The install-phase view of the runtime one extension receives.

| Method | Meaning |
|---|---|
| `fn runtime(&self) -> &'r Runtime`, `fn plan(&self) -> &'r RuntimePlan` | |
| `fn extension_id(&self) -> &'static str` | The extension being installed |
| `fn module(&mut self, path: &str) -> Result<&mut ModuleInstaller<'r>>` | The module at `path`, which this extension declared, with its declared functions and constants in place. A path the plan does not know, or another extension provides, is a logic error |
| `fn service<S: 'static>(&self) -> Result<Rc<S>>` | A host service this extension declared it needs |
| `fn has_capability(&self, capability: &str) -> Result<bool>` | Whether the policy grants `capability`, which must have been declared with `capability` or `optional_capability`: an undeclared name is a logic error, so a misspelling surfaces instead of reading as not granted |
| `fn require_capability(&self, capability: &str) -> Result<()>` | `Error::Permission` unless granted |
| `fn memory_category(&self, name: &str) -> Result<MemoryCategory>` | The category for a declared symbolic name |
| `fn insert_state<S: 'static>(&self, state: S) -> Rc<S>` | This extension's runtime state, one value per type per extension, dropped before the VM closes |
| `fn state<S: 'static>(&self) -> Option<Rc<S>>` | |
| `fn state_of<S: 'static>(&self, owner: &'static str) -> Result<Option<Rc<S>>>` | The state extension `owner` stored; `owner` must be this extension, a declared `requires`, or a declared `optional` that is in the plan |

{{ api_signature(value="struct ModuleInstaller<'c>") }}

One module table under construction: the declared functions and constants first, then what the
provider's `install` adds, then frozen by the planner.

| Method | Meaning |
|---|---|
| `fn path(&self) -> &str`, `fn table(&self) -> &Table` | |
| `fn function<F: Binding<M>, M>(&mut self, name: &str, callable: F) -> Result<&mut Self>` | Binds `callable` as the declared install-time member `name` |
| `fn set<T: Push + ?Sized>(&mut self, name: &str, value: &T) -> Result<&mut Self>` | Any other value for a declared install-time member (a nested table, a userdata instance); unknown to the compiler |

A name the plan did not declare with `installed`, a declared function or constant, or a member
set twice is a logic error.

## Packed values

Module `l3i::packed`: fixed-size byte layouts for buffers and semantic 64-bit scalars.

{{ api_signature(value="trait BufferPack: Sized { const SIZE: usize; fn read_from(bytes: &[u8]) -> Result<Self>; fn write_to(&self, bytes: &mut [u8]) -> Result<()>; }") }}

A value with a fixed little-endian byte layout inside a Luau buffer. Implemented for the Rust
integers, `f32`, `f64`, `Vector3` (12 bytes), `Packed<T>` (8 bytes), `Color` (4 bytes) and
`Color16` (8 bytes). `BufferView::read_packed` and `write_packed` cross the boundary with one
bounds check and one copy, so `read_from` never sees a slice of Luau's storage.

{{ api_signature(value="trait PackedScalar: Sized + 'static { const KIND: u8; const NAME: &'static str; fn pack(&self) -> (u64, u8); fn unpack(payload: u64, flags: u8) -> Result<Self>; }") }}

A semantic value that lives in one Luau integer: a 4-bit kind, 4 flag bits, a 56-bit payload.
The kind is checked on every read, so untyped script code handing the wrong integer to a native
operation fails with a type error instead of decoding garbage. Kinds are a registry: `1..=4`
are l3i's own and fixed for good (a packed integer is a file and wire format), `5..=15` are the
application's, declared per extension with `ExtensionDescriptor::packed` or per runtime with
`Runtime::register_packed`. A `Packed<T>` crossing a VM where `T` is not the kind's registered
owner is a logic error.

| Constant | Value |
|---|---|
| `PAYLOAD_BITS: u32` | 56 |
| `PAYLOAD_MASK: u64` | the low 56 bits |
| `KIND_BITS: u32`, `FLAG_BITS: u32` | 4 and 4 |
| `HOST_KIND_FIRST: u8` | 5 |
| `LAST_KIND: u8` | 15 |
| `BUILTIN_KINDS: [u8; 4]` | `[1, 2, 3, 4]`: `quat::Quaternion`, `quat::AnimationKey`, `raster::Color`, `raster::ClipRect` |

{{ api_signature(value="struct PackedKind { pub kind: u8, pub type_id: TypeId, pub name: &'static str }") }}

One registered kind. `fn of<T: PackedScalar>() -> PackedKind` and `fn is_builtin(&self) ->
bool`. `Clone`, `Copy`, `Debug`, `Eq`.

{{ api_signature(value="fn builtin_kinds() -> [PackedKind; 4]") }}

l3i's own kinds in kind order from 1.

{{ api_signature(value="struct Packed<T>(pub T)") }}

The Luau integer carrying a packed scalar of kind `T`. `FromView` (an integer of kind `T::KIND`,
after the registry check), `Push`, `Param`, `Return`, `BufferPack`, `SequenceItem`. `Clone`,
`Copy`, `Debug`, `Eq`, `Hash`.

| Method | Meaning |
|---|---|
| `fn bits(&self) -> Result<i64>` | The integer bit pattern; an error if `T::pack` returned more bits than its fields hold |
| `fn from_bits(bits: i64) -> Result<Self>` | Decodes, checking the kind first: `expected a packed Quaternion, got kind 3` |

{{ api_signature(value="fn encode(kind: u8, flags: u8, payload: u64) -> Result<i64>") }}

{{ api_signature(value="fn decode(bits: i64) -> (u8, u8, u64)") }}

The bit pattern for `kind`, `flags` and `payload`, refusing a kind outside `1..=15`, flags above
four bits or a payload above 56 bits rather than truncating; and the split back into `(kind,
flags, payload)`.

## Sequences and streams

Module `l3i::sequence`. A `Sequence` wraps a Rust collection and shows it to scripts as
`#items`, `items[i]` (1-based, nil past the end), `for item in items do` and `items:toTable()`;
only the item a script touches is pushed. A `Stream` is the cursor-backed variant for results
that cannot be indexed: `for item in stream do` opens a private cursor per loop, nothing else.
Both are ordinary userdata types declared through the planner (`ExtensionDescriptor::sequence`,
`stream`) or configured by hand.

{{ api_signature(value="trait SequenceSource: 'static { const NAME: &'static str; type Item: SequenceItem; fn len(&self) -> usize; fn is_empty(&self) -> bool; fn get(&self, index: usize) -> Option<Self::Item>; }") }}

A random-access native collection. `NAME` is the userdata `__type`, a debug name such as
`dream.archive.Entries`; `get` is 0-based.

{{ api_signature(value="trait StreamSource: 'static { const NAME: &'static str; type Item: SequenceItem; type Cursor: 'static; fn open(&self) -> Self::Cursor; fn next(cursor: &Self::Cursor) -> Option<Self::Item>; }") }}

A collection consumed through a per-loop cursor with interior mutability inside.

{{ api_signature(value="struct Sequence<S>(pub S)") }}

{{ api_signature(value="struct Stream<S>(pub S)") }}

The userdata payloads; both implement `Userdata` with `NAME = S::NAME`. Each has `fn push<'s>(scope:
&'s impl Scope, source: S) -> Result<ValueView<'s>>` (the type must be registered in this VM).

{{ api_signature(value="trait SequenceItem { fn push_item<S: Scope>(self, scope: &S) -> Result<()>; }") }}

An element pushed by value: every `Push` scalar, `Value`, `Table`, `Function`, `Packed<T>`,
`Option<T: SequenceItem>`, and `Owned<T>` for any registered `T`, which moves the row into a
fresh userdata without needing `Clone`.

{{ api_signature(value="struct IterStep<T: SequenceItem>(pub i64, pub T)") }}

One step of a view's iterator as a `Return`: the next control value and the element.

{{ api_signature(value="fn configure_sequence<S: SequenceSource>(ty: &mut MetatableBuilder<'_>) -> Result<()>") }}

{{ api_signature(value="fn configure_stream<S: StreamSource>(ty: &mut MetatableBuilder<'_>) -> Result<()>") }}

Configure a metatable whose `__type` is `S::NAME` by hand: `toTable`, `__len`, `__iter` and an
integer `__index` over the methods table (call after any extra methods); or `__iter` opening one
cursor per loop. Indexing reads the receiver and the key straight from Luau's value layout; an
exact integer key selects an element, a fractional or out-of-range number is nil, never a
rounded neighbour.

## dream.quat

Module `l3i::quat`: rotations as packed Luau integers. A unit quaternion is compressed
smallest-three into the 56-bit payload: two bits name the largest component, the other three are
18-bit lanes over `[-1/√2, 1/√2]`, and the omitted one is rebuilt from the unit norm. The
identity and axis-aligned rotations round-trip exactly; a random rotation comes back within
1.6e-5 rad. The packed form is storage and transport, never the live accumulator: keep long-lived
rotation state as `Quat` on the host.

{{ api_signature(value="struct QuatExtension") }}

Extension `dream.quat`, module `@dream/quat`: `IDENTITY` (a folded constant), `axisAngle(axis,
angle)`, `fromXYZW(x, y, z, w)`, `toXYZW(q)`, `mul(a, b)`, `inverse(q)`, `slerp(a, b, t)`,
`rotate(q, v)`, `angleTo(a, b)`, `key(q, flags)`, `keyRotation(k)`, `keyFlags(k)`. `axisAngle`
and `fromXYZW` refuse a zero or non-finite input, `slerp` a non-finite weight, and `key` takes the
low four bits of an exact integer. With `jit`, `math()` returns a `dream.quat.Math` receiver
(class `dream_quat_Math`) whose `rotate`, `mul`, `slerp`, `fromXYZW`, `key`, `keyRotation` and
`keyFlags` lower to IR when the script annotates it.

{{ api_signature(value="struct Quat { pub x: f64, pub y: f64, pub z: f64, pub w: f64 }") }}

A unit quaternion in f64: the reference representation and the host's accumulator. `Mul` is the
Hamilton product `a * b` (apply `b`, then `a`).

| Method | Meaning |
|---|---|
| `const IDENTITY: Quat` | |
| `fn from_axis_angle(axis: [f64; 3], angle: f64) -> Quat` | The rotation of `angle` radians about `axis` |
| `fn try_from_axis_angle(axis: [f64; 3], angle: f64) -> Option<Quat>` | `None` for a non-finite or zero axis or a non-finite angle |
| `fn normalize(self) -> Quat`, `fn try_normalize(self) -> Option<Quat>` | |
| `fn inverse(self) -> Quat` | The conjugate |
| `fn dot(self, o: Quat) -> f64` | |
| `fn slerp(self, o: Quat, t: f64) -> Quat` | Along the shorter arc, `t` clamped to `0..=1`, with polynomial trigonometry shared by the native lowering |
| `fn rotate(self, v: [f64; 3]) -> [f64; 3]` | |
| `fn angle_to(self, o: Quat) -> f64` | The rotation angle between two unit quaternions |

| Item | Meaning |
|---|---|
| `const ACOS_COEFFICIENTS: [f64; 8]`, `fn acos_poly(x: f64) -> f64` | `acos` for `0 <= x <= 1`, absolute error below 2e-8 |
| `const SIN_COEFFICIENTS: [f64; 6]`, `fn sin_poly(x: f64) -> f64` | `sin` for `0 <= x <= pi/2`, absolute error below 6e-8 |
| `const COMPONENT_BITS: u32 = 18`, `const COMPONENT_MAX: f64`, `const RANGE: f64` | The lane layout |

{{ api_signature(value="struct PackedRotation(pub u64)") }}

A rotation packed into 56 bits. `fn encode(q: Quat) -> PackedRotation` normalises first only
when `q` is not already unit; `fn decode(self) -> Quat`.

{{ api_signature(value="struct Quaternion(pub Quat)") }}

The packed scalar kind of a rotation (kind 1, flags always zero). `fn pack(q: Quat) ->
Packed<Quaternion>`.

{{ api_signature(value="struct AnimationKey { pub rotation: Quat, pub flags: u8 }") }}

Kind 2: a rotation plus four opaque flag bits (higher bits are dropped when packing). `fn
pack(rotation: Quat, flags: u8) -> Packed<AnimationKey>`.

Module `l3i::quat::lowering` (feature `jit`): `struct Math`, the payload-free tagged receiver;
`struct Lowering`, the `NativeCodeHooks` set the extension registers; `fn lowered_sites() ->
usize`, a diagnostic for tests; and the interpreter oracles `fn rotate(q: Packed<Quaternion>, v:
Vector3) -> Vector3`, `fn mul(a: Packed<Quaternion>, b: Packed<Quaternion>) ->
Packed<Quaternion>` and `fn slerp(a: Packed<Quaternion>, b: Packed<Quaternion>, t: f64) ->
Result<Packed<Quaternion>>`. Only single-result, fixed-arity call sites lower: bind a nested
call's result to a local first.

## dream.raster

Module `l3i::raster`: raster scalars as packed integers.

{{ api_signature(value="struct RasterExtension") }}

{{ api_signature(value='const EXTENSION_ID: &str = "dream.raster"') }}

{{ api_signature(value='const MODULE: &str = "@dream/raster"') }}

The module: the folded constants `TRANSPARENT`, `BLACK`, `WHITE`, `CLIP_ALL`, `CLIP_MAX_COORD`,
`TRANSPARENT16`, `BLACK16`, `WHITE16`; the strict constructors `rgba8(r, g, b, a)` and `rgb8(r,
g, b)` (an integer outside `0..=255` is an error); `channels`, `packed`, `withAlpha`, `lerp`,
`mul`, `add`, `scale`, `premultiply`, `clip`, `clipBounds`; and `math()`, a `dream.raster.Math`
receiver whose methods (`rgba8`, `rgb8`, `red`, `green`, `blue`, `alpha`, `channels`,
`withAlpha`, `lerp`, `mul`, `add`, `scale`, `premultiply`, each also in a `16` form, plus
`widen` and `narrow`) clamp their inputs and lower to native code under `jit` when annotated
(`local C: dream_raster_Math = raster.math()`). Shader semantics: inputs clamp, results round to
nearest, NaN gives channel 0.

{{ api_signature(value="struct Color { pub r: u8, pub g: u8, pub b: u8, pub a: u8 }") }}

An RGBA8 color, kind 3. Its 32-bit form, red in bits 0..7 through alpha in bits 24..31, is
exactly the four bytes `[r, g, b, a]` a vertex color or a texel holds; every `u32` is a color.
Channel meaning (straight or premultiplied) belongs to the consumer. `BufferPack` as four bytes.

| Method | Meaning |
|---|---|
| `const TRANSPARENT`, `BLACK`, `WHITE` | |
| `const fn rgba(r: u8, g: u8, b: u8, a: u8) -> Color` | |
| `const fn packed(self) -> u32`, `const fn from_packed(bits: u32) -> Color` | The fixed 32-bit layout |
| `const fn to_array(self) -> [u8; 4]` | |
| `const fn pack(self) -> Packed<Color>` | The Luau integer |
| `fn channel(value: f64) -> u8` | Clamped to `0..=255`, rounded to nearest, NaN to 0 |
| `fn from_numbers(r: f64, g: f64, b: f64, a: f64) -> Color` | Each through `channel` |
| `fn with_alpha(self, a: f64) -> Color` | |
| `fn lerp(self, other: Color, t: f64) -> Color` | Per channel; `t` clamped to `0..=1` |
| `fn modulate(self, other: Color) -> Color` | `a * b / 255` per channel |
| `fn saturating_add(self, other: Color) -> Color` | |
| `fn scale(self, factor: f64) -> Color` | Color channels scaled, alpha unchanged |
| `fn premultiply(self) -> Color` | Straight alpha to premultiplied, `(c * a + 127) / 255` |

{{ api_signature(value="struct Color16 { pub r: u16, pub g: u16, pub b: u16, pub a: u16 }") }}

The wide form for formats that require 16 bits per channel: red in bits 0..15 through alpha in
bits 48..63, the little-endian `u64` being an RGBA16 pixel. It fills the whole Luau integer and
so has no kind nibble: any integer is accepted as a `Color16`, and nothing at runtime tells it
from an RGBA8 color or an id. The same methods as `Color` over `u16`, plus `const WIDEN: f64 =
257.0`, `const fn bits(self) -> i64`, `fn widen(color: Color) -> Color16` (`x * 257`) and `fn
narrow(self) -> Color` (`round(x / 257)`). `FromView`, `Push`, `Param`, `Return`, `BufferPack`.

{{ api_signature(value="struct ClipRect { pub min_x: u32, pub min_y: u32, pub max_x: u32, pub max_y: u32 }") }}

An integer pixel rectangle, kind 4: four 14-bit fields, so a coordinate is at most
`MAX_COORD` (16383). A deliberate limit of the packed form, not of a renderer: a surface past 16K
on an axis needs a clip type of its own.

| Method | Meaning |
|---|---|
| `const FIELD_BITS: u32 = 14`, `const MAX_COORD: u32` | |
| `const ALL: ClipRect` | Every field at the maximum, which a clamping renderer treats as no clip |
| `fn new(min_x: u32, min_y: u32, max_x: u32, max_y: u32) -> Result<ClipRect>` | Fails when a value exceeds `MAX_COORD` or `min > max` on an axis |
| `const fn pack(self) -> Packed<ClipRect>` | |
| `const fn reaches_limit(self) -> bool` | Both max fields at `MAX_COORD` |

{{ api_signature(value="struct Math") }}

The color arithmetic receiver (`dream.raster.Math`). Module `l3i::raster::lowering` (feature
`jit`): `struct ColorMath`, the hook set, and `fn lowered_sites() -> usize`.

## dream.udp

Module `l3i::udp`: the dream-net bridge, extension `dream.udp`, module `@dream/udp`, types
`dream.udp.Server`, `dream.udp.Client` and `dream.udp.Schema`. Every `RuntimePlan` carries it
and the id is reserved. Peer, event, channel and client ids are Luau integers; sizes and counters
plain numbers; payloads Luau buffers (or strings on send), copied on the way in and out so no
Lua memory is retained by the transport; the transport clock is the plan's; the server private
key never reaches Luau; nothing calls into Luau from inside dream-net.

| Constant | Value |
|---|---|
| `EXTENSION_ID: &str` | `dream.udp` |
| `MODULE: &str` | `@dream/udp` |
| `TRANSPORT_CAPABILITY: &str` | `network.transport`: lets scripts create transport objects with `udp.client` |

{{ api_signature(value="type Clock = Rc<dyn Fn() -> f64>") }}

{{ api_signature(value="fn monotonic_clock() -> Clock") }}

A monotonic clock the transport reads on `update()`, seconds as `f64`; one counting from its
creation.

{{ api_signature(value="struct UdpExtension") }}

The extension, added by the planner. `fn new() -> Self` (a monotonic clock started now), `fn
with_clock(clock: Clock) -> Self`, `Default`. The module declares `schema(options)` (a strict
option table: `version`, `maxMessagesPerPacket?`, `channels`, `events`), the constants
`CONNECT_TOKEN_BYTES`, `MAX_EVENT_PAYLOAD`, `MAX_CHANNELS`, and the installed `client{ schema,
bind? }`, which raises `Error::Permission` unless the policy grants `network.transport`.

{{ api_signature(value="struct UdpSchema(pub Schema)") }}

`dream.udp.Schema`, untagged: getters `version`, `fingerprint`, `eventCount`, `channelCount`;
methods `eventId(name)`, `channelId(name)`, `eventName(id)`, `channelName(id)`.

{{ api_signature(value="struct Server") }}

`dream.udp.Server`: a dream-net server as scripts see it. Created by the host in Rust, which
keeps the private key. Scripts get `update()`, `pollInto(buffer)` returning `kind, peer, a, b,
c` with one payload copy and no allocation, `sendEvent(peer, eventId, bytes, offset?, length?)`,
`flush()`, and the per-peer statistics methods.

| Method | Meaning |
|---|---|
| `fn new(server: dream_net::Server, clock: Clock) -> Server` | Wraps a server the host created, with the clock its `update()` reads |
| `fn push<'s>(scope: &'s impl Scope, server: dream_net::Server, clock: Clock) -> Result<ValueView<'s>>` | Pushes a server handle; the extension must be installed |
| `fn with(&self, body: impl FnOnce(&mut dream_net::Server))` | Runs `body` with the wrapped server borrowed mutably, for host code |

{{ api_signature(value="struct UdpClient") }}

`dream.udp.Client`, with `fn new(client: Client, clock: Clock) -> UdpClient`, `fn push<'s>(scope:
&'s impl Scope, client: Client, clock: Clock) -> Result<ValueView<'s>>` and `fn with(&self, body:
impl FnOnce(&mut Client))`. Connection statistics are direct fields on the client.

{{ api_signature(value="fn delivery_name(delivery: Delivery) -> &'static str") }}

`Delivery` names as scripts spell them.

## dream.soft_render

Module `l3i::soft_render`, feature `soft-render`: dream-soft-render's CPU rasterizer as a small
software rendering device. Rectangles, textured rectangles and triangle meshes from Luau buffers
are drawn immediately in call order into a renderer-owned RGBA8 surface. Colors are `raster`
integers the renderer reads as premultiplied; clip rectangles are `ClipRect` integers; vertex
data is a buffer of 20-byte vertices and an index buffer of `u32`, borrowed for one draw call and
never kept. Malformed input is an error in the renderer's own words, never a quietly clipped draw.

| Constant | Value |
|---|---|
| `EXTENSION_ID: &str` | `dream.soft_render` |
| `MODULE: &str` | `@dream/soft-render` |
| `VERTEX_BYTES: usize` | The size of one vertex (20) |
| `INDEX_BYTES: usize` | The size of one index (4) |

{{ api_signature(value="struct SoftRenderExtension") }}

Requires `dream.raster` in the same plan. The module: `renderer()`, `vertices()`,
`premultiply(color)`, and the folded constants `MAX_SURFACE_PIXELS`, `MAX_TEXTURE_BYTES`,
`VERTEX_BYTES`.

{{ api_signature(value="struct Renderer") }}

`dream.soft_render.Renderer`: one surface and a texture store. Scripts get `beginFrame(width,
height)`, `createTexture(width, height, pixels)`, `readInto(buffer, offset?)` and the direct
fields `width` and `height`. `fn new() -> Renderer` (`Default`); `fn with<R>(&self, body: impl
FnOnce(&mut SoftwareRenderer) -> R) -> Result<R>` runs `body` on the underlying renderer for host
code and fails if a script call on this renderer is in progress.

{{ api_signature(value="struct Frame") }}

`dream.soft_render.Frame`, from `beginFrame`: `clear(color)`, `rect(min, max, color, clip)`,
`image(min, max, uvMin, uvMax, texture, tint, clip)`, `mesh(vertexBuffer, indexBuffer, texture?,
clip)`, `finish()`. A token for one frame, stale after `finish()` or the next `beginFrame`.

{{ api_signature(value="struct Texture") }}

`dream.soft_render.Texture`: `update(...)` and `free()`; storage is freed on `free()` or when
collected.

{{ api_signature(value="struct Vertices") }}

`dream.soft_render.Vertices`, from `vertices()`: `write(buffer, offset, pos, uv, color)` packs a
vertex with one bounds check and returns the next offset. With `jit` the call lowers to native
stores.

{{ api_signature(value="fn write_vertex(buffer: BufferView<'_>, offset: f64, pos: Vector3, uv: Vector3, color: Packed<Color>) -> Result<f64>") }}

The interpreter path of `Vertices:write`, and the oracle its lowering must match. The offset
truncates toward zero like the buffer library's; anything outside the buffer is `buffer access
out of bounds`.

Module `l3i::soft_render::lowering` (feature `jit`): `struct VertexWriter`, the hook set, and
`fn lowered_sites() -> usize`.

## dream.bytes

Module `l3i::bytes`, feature `bytes`; the module functions, the receiver and the codecs are
described from the script's side in [Built-in extensions](@/docs/builtin-extensions.md#dream-bytes).

{{ api_signature(value="pub struct BytesExtension") }}

The extension; `id()` is `"dream.bytes"`, the module path is `MODULE` (`"@dream/bytes"`).
`Clone`, `Copy`, `Debug`, `Default`.

{{ api_signature(value="pub struct Math") }}

The receiver behind `bytes.math()` (`Userdata::NAME` `"dream.bytes.Math"`), tagged and given a
compiler type slot, both `Required`.

{{ api_signature(value="pub fn to_hex(data: &[u8]) -> String") }}

Lower-case hex, the module's `toHex`.

| Module | Feature | Items |
|---|---|---|
| `bytes::numeric` | `bytes` | `f16_to_f32(bits: u16) -> f32`, `f32_to_f16(value: f32) -> u16` (round to nearest even, overflow to infinity), the read and write implementations |
| `bytes::codecs` | `bytes-codecs` | `DEFAULT_MAX_SIZE` (1 GiB), the codec bindings |
| `bytes::digests` | `bytes-digests` | `fnv1a32(&[u8]) -> u32`, `fnv1a64(&[u8]) -> u64`, `Hasher` (`Userdata::NAME` `"dream.bytes.Hasher"`, untagged) |
| `bytes::text` | `bytes-text` | The text bindings |
| `bytes::lowering` | `jit` | `ByteMath`, the `NativeCodeHooks` set that lowers the receiver's integer methods; `lowered_sites()` |

{{ api_signature(value="pub struct NewBuffer(pub Vec<u8>)") }}

In `l3i::convert`: the return type for bytes a script receives as a new `buffer`. Pushing
allocates the buffer at the vector's length and copies once; returning a `Vec<u8>` pushes a
string instead. `Push` and a single `Return`.

## dream.intern

Module `l3i::intern`, feature `intern`; the Luau side is in
[Built-in extensions](@/docs/builtin-extensions.md#dream-intern).

{{ api_signature(value="pub struct InternExtension") }}

The extension; `id()` is `"dream.intern"`, the module path is `MODULE` (`"@dream/intern"`).
`Clone`, `Copy`, `Debug`, `Default`.

{{ api_signature(value="pub enum Policy { Exact, AsciiNoCase }") }}

When two byte sequences are one identity. `parse("exact" | "ascii-nocase")`, `name()`.

{{ api_signature(value="pub struct Interner") }}

The pool without Luau, for hosts that intern from Rust. `new(policy)`, `intern(&[u8]) ->
Result<u32>` (the token, added on first sight; an error only past 2^32 - 1 identities or 4 GiB
of text), `find(&[u8]) -> Option<u32>`, `resolve(i64) -> Option<&[u8]>` (the first spelling),
`len()`, `is_empty()`, `policy()`, `memory()` (native bytes held).

{{ api_signature(value="pub struct Pool") }}

The userdata behind `intern.new` (`Userdata::NAME` `"dream.intern.Pool"`, tagged and given a
compiler type slot, both `Required`): an `Interner` shared with the functions `Pool:interner()`
binds, which keep it alive, laid out `#[repr(C)]` for native code.

| Module | Feature | Items |
|---|---|---|
| `intern::lowering` | `intern`, `jit` | `InternLowering`, the `NativeCodeHooks` set that lowers `Pool:intern` and `Pool:find`; `lowered_sites()`, `binder_calls()` |

## dream.intl

Module `l3i::intl`, feature `intl`; the Luau side is in
[Built-in extensions](@/docs/builtin-extensions.md#dream-intl). Every type works from Rust
without a VM.

{{ api_signature(value="pub struct IntlExtension") }}

The extension; `id()` is `"dream.intl"`, the module path is `MODULE` (`"@dream/intl"`).
`Clone`, `Copy`, `Debug`, `Default`.

{{ api_signature(value="pub struct Locale") }}

A BCP 47 tag, validated and spelled canonically; also the userdata behind `intl.locale`
(`"dream.intl.Locale"`). `parse(&str) -> Result<Locale, LocaleError>`, `as_str()` (the
canonical tag), `base_name()`, `language()`, `script()`, `region()`, `variants()`. Equality and
hashing are the canonical tag's. `Display`, `FromStr`. `canonicalize(&str) -> Result<String,
LocaleError>` and `with_canonical(&str, f)`, which hands the spelling to `f` without allocating
for a tag of up to 64 bytes without extensions, need no `Locale`. `LocaleError` is `Empty` or
`Invalid { input, reason }`.

{{ api_signature(value="pub enum Operand<'a> { Integer(i64), Number(f64), Decimal(&'a str) }") }}

A number to select a category for or format. `decimal()` is the exact `fixed_decimal::Decimal`;
a `Decimal` string keeps its visible fraction digits, a `Number` reads as its shortest
round-trip decimal. Also a binding parameter (`number | integer | string`). `NumberError` is
`NotFinite(f64)`, `Malformed(String)` or `TooLong(String)`.

{{ api_signature(value="pub struct PluralRules") }}

One locale's rules; also the userdata behind `intl.pluralRules` (`"dream.intl.PluralRules"`).
`new(&Locale, PluralKind) -> Result<PluralRules, UnsupportedLocale>`, `category(Operand) ->
Result<Category, NumberError>`, `categories()`, `locale()`, `kind()`. `PluralKind` is
`Cardinal` or `Ordinal`; `Category` is `Zero`, `One`, `Two`, `Few`, `Many` or `Other`, with
`name()` the CLDR keyword.

{{ api_signature(value="pub struct DecimalFormatter") }}

One locale's decimal format under `DecimalOptions { grouping, min_fraction_digits,
max_fraction_digits }` (`Grouping` is `Auto`, `Never` or `Min2`; the digit counts are
`Option<u8>`, at most `MAX_FRACTION_DIGITS`, 100). `new(&Locale, DecimalOptions) ->
Result<DecimalFormatter, FormatterError>`, `format_to(Operand, &mut String) -> Result<(),
NumberError>`, `format(Operand)`, `locale()`, `grouping()`, `min_fraction_digits()`,
`max_fraction_digits()` (resolved). `FormatterError` is `FractionDigitsOutOfRange`,
`FractionDigitsInverted` or `Unsupported(UnsupportedLocale)`. The Luau handle,
`"dream.intl.DecimalFormatter"`, wraps one with the buffer `format` writes into.

`IrBuilder::namecall_call(pcpos)` emits the namecall and call Luau would have emitted for the
pair at `pcpos`: a userdata namecall hook that lowers a fast path keeps the bound method as its
slow path with it, without a VM exit, since returning true skips both instructions.

## dream.luau

Module `l3i::syntax`, feature `syntax`; the Luau side is in
[Built-in extensions](@/docs/builtin-extensions.md#dream-luau).

{{ api_signature(value="pub struct SyntaxExtension") }}

The extension; `id()` is `"dream.luau"` (`EXTENSION_ID`), the module path is `MODULE`
(`"@dream/luau"`). `Clone`, `Copy`, `Debug`, `Default`.

| Item | What it is |
|---|---|
| `TYPES: &[(&str, &str)]` | Every `dream_luau_*` type the definitions declare, name and definition, in order: the node kinds, the unions over them, the result |
| `TOKEN_KINDS: [&str; 14]` | The token kind names, numbered from 1 in this order as `luau.tokenKinds` numbers them |


## dream.tcp

Module `l3i::tcp`, feature `tcp`; the Luau side is in
[Built-in extensions](@/docs/builtin-extensions.md#dream-tcp).

{{ api_signature(value="pub struct TcpExtension") }}

The extension; `id()` is `"dream.tcp"` (`EXTENSION_ID`), the module path is `MODULE`
(`"@dream/tcp"`). `Clone`, `Copy`, `Debug`, `Default`.

| Item | What it is |
|---|---|
| `CONNECT_CAPABILITY` | `"network.tcp.connect"`: `tcp.connect` |
| `LISTEN_CAPABILITY` | `"network.tcp.listen"`: `tcp.listen` on loopback |
| `PUBLIC_CAPABILITY` | `"network.tcp.public"`: with listen, wildcard and non-loopback binds |
| `MAX_WAIT_MS: u32` | 60000, the longest `Poller:wait` any poller allows |
| `MAX_POLLER_LIMIT: u32` | 65536, the most `maxEvents` and `maxWatches` |
| `MAX_STREAM_LIMIT: u32` | 65536, the most `maxStreams` |

{{ api_signature(value="pub struct Listener") }}

The userdata behind `tcp.listen` (`"dream.tcp.Listener"`). `from_std(std::net::TcpListener,
max_streams: u32) -> io::Result<Listener>` wraps a listener the host bound and makes it
nonblocking; `push(scope, listener)` hands it to a script, which needs no capability to use
it. `local_address()`, `is_closed()`, `close()`.

{{ api_signature(value="pub struct Stream") }}

The userdata behind `tcp.connect` and `Listener:accept` (`"dream.tcp.Stream"`).
`from_std(std::net::TcpStream) -> io::Result<Stream>` wraps a connected stream the host made;
`push(scope, stream)`. `state() -> StreamState` (`Connecting`, `Connected`, `Failed`, `Closed`,
with `name()` the script's spelling), `peer_address()`, `local_address()`, `close()`.

{{ api_signature(value="pub struct Poller") }}

The userdata behind `tcp.poller` (`"dream.tcp.Poller"`); `close()` releases every watch and
the OS poller. Listeners, streams and pollers are `!Send`: they belong to the runtime that made
them.
