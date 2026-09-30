+++
title = "Runtime"
description = "Runtime, RuntimeBuilder, call scopes and the profiler, sandboxes and templates, compile options, the flag policy, Error, diagnostics and debug names."
weight = 210

[extra]
kind = "api"
+++

Modules `l3i::runtime`, `l3i::runtime::profiler`, `l3i::sandbox`, `l3i::source`, `l3i::flags`,
`l3i::error`, `l3i::diagnostics` and `l3i::debug_name`. `Runtime`, `Error` and `Result` are
re-exported from the root. [Start here](@/docs/start-here.md) shows a runtime in use and
[Runtime options](@/docs/runtime-options.md) covers the watchdog, memory categories and the
profiler in prose.

## RuntimeBuilder

{{ api_signature(value="struct RuntimeBuilder") }}

Host choices made before the VM exists. Made by `Runtime::builder()`; defaults: debug root
`dreamweave`, pointer encoding seeded, standard libraries opened, no limits, profiler off,
initialization category 0, no atom catalogue, no buffer cage, no native code.

| Method | Meaning |
|---|---|
| `fn debug_roots<R: AsRef<str>>(self, roots: &[R]) -> Self` | Root vocabulary for debug names. Every native function and userdata type registered into the VM must be named under one of these |
| `fn pointer_encoding(self, enabled: bool) -> Self` | Seed Luau's pointer-encoding key from OS entropy right after the state is created. Off leaves Luau's identity mapping, for deterministic test fixtures only |
| `fn standard_libraries(self, enabled: bool) -> Self` | Whether `build` opens Luau's standard libraries. Hosts that must install callbacks first turn it off and call `Runtime::open_standard_libraries` |
| `fn execution_time_limit(self, limit: Duration) -> Self` | Wall-clock budget per outermost script call, enforced at Luau safepoints. Zero disables |
| `fn memory_limit(self, bytes: usize) -> Self` | Heap ceiling in bytes across every memory category, polled every 64th safepoint, not enforced by the allocator: a script can overshoot by what it allocates between two polls. Zero disables |
| `fn profiler(self, enabled: bool) -> Self` | Time script calls, switch memory categories per call, record allocation activity |
| `fn initialization_category(self, category: MemoryCategory) -> Self` | The category charged while sandboxes and templates are set up. Default 0 |
| `fn atom_catalogue(self, catalogue: AtomCatalogue) -> Self` | This VM's [atom catalogue](@/docs/api/direct.md), installed before the standard libraries open |
| `fn buffer_cage(self, cage: impl BufferCage) -> Self` | Routes every `buffer` allocation through `cage` (`lua_setbuffercage`), installed before any buffer exists |
| `fn native_code(self, options: NativeCodeOptions) -> Self` | Feature `jit`: enables native code generation with `options`. Off by default |
| `fn build(self) -> Result<Runtime>` | Creates the VM in OpenMW's order: flag policy, `luaL_newstate`, pointer key, buffer cage, per-VM state and callbacks, native code, atom catalogue, standard libraries, the value-layout self test, l3i's packed kinds |

Errors from `build`: the flag policy names a flag this Luau build lacks (`Error::Logic`), the
state cannot be allocated (`Error::Runtime`), the atom catalogue does not resolve, or the value
layout mirror disagrees with the API.

## Runtime

{{ api_signature(value="struct Runtime") }}

Owns one Luau VM. Dropping it closes the VM after dropping every runtime-owned state value and
ending the lifetime token that pinned `Value`s check, so a value that outlives its runtime
becomes invalid instead of touching a closed VM. Not `Clone`, not `Send`.

### Creating

{{ api_signature(value="fn new() -> Result<Runtime>") }}

{{ api_signature(value="fn builder() -> RuntimeBuilder") }}

{{ api_signature(value="fn from_plan(plan: &Rc<RuntimePlan>) -> Result<Runtime>") }}

A VM with the default configuration; the builder; or a VM built from a finalised
[extension plan](@/docs/api/extension.md): every extension installed in dependency order, direct
dispatch, modules and compiler metadata published together.

### Running code

{{ api_signature(value="fn exec(&self, source: &str) -> Result<()>") }}

{{ api_signature(value="fn eval<R: CallResults>(&self, source: &str) -> Result<R>") }}

{{ api_signature(value="fn load_function(&self, source: &str) -> Result<Function>") }}

Compile and run `source` on the main thread with this runtime's compile options: discarding
results; reading what the chunk returns (`runtime.eval::<f64>("return 1 + 1")`, a tuple for
several results, `()` for none); or running a chunk that returns one function and pinning it.
Each leases the root stack, so do not hold `Runtime::stack` across them. A compile error or a
Lua error is `Error::Runtime` with Luau's message.

{{ api_signature(value="fn load<'f>(&self, frame: &'f Frame<'_>, chunk_name: &str, source: &str, options: &CompileOptions) -> Result<ValueView<'f>>") }}

{{ api_signature(value="fn load_with_env<'f>(&self, frame: &'f Frame<'_>, chunk_name: &str, source: &str, options: &CompileOptions, env: &Table) -> Result<ValueView<'f>>") }}

Compiles `source` and pushes the chunk function onto `frame`, which may be on any thread of this
VM; the second form sets the chunk's environment to `env` so its globals resolve through that
table. A frame from another VM or a chunk name containing NUL is a logic error.

```rust
use l3i::Runtime;

fn main() -> l3i::Result<()> {
    let runtime = Runtime::new()?;
    runtime.exec("assert(typeof(42i) == 'integer')")?;
    let sum: f64 = runtime.eval("return 1 + 1")?;
    assert_eq!(sum, 2.0);
    let double = runtime.load_function("return function(n) return n * 2 end")?;
    assert_eq!(double.invoke::<i32, _>(&runtime.stack(), (21,))?, 42);
    Ok(())
}
```

### Globals, functions and modules

{{ api_signature(value="fn bind_function<F: Binding<M>, M>(&self, debug_name: &str, callable: F) -> Result<Function>") }}

Binds `callable` as a Lua function named `debug_name` under this runtime's debug roots and
returns it pinned. The shapes a callable may take are on [Binding and userdata](@/docs/api/bind.md).

{{ api_signature(value="fn set_global<T: Push + ?Sized>(&self, name: &str, value: &T) -> Result<()>") }}

{{ api_signature(value="fn global(&self, name: &str) -> Result<Value>") }}

Stores any pushable value as the global `name` (fails when the globals table is read-only), or
pins the global `name` (nil pins as a valid nil).

{{ api_signature(value="fn module(&self, path: &str) -> Result<ModuleBuilder<'_>>") }}

{{ api_signature(value="fn register_module<M: LuauModule>(&self) -> Result<Table>") }}

{{ api_signature(value="fn require_debug_name(&self, name: &str) -> Result<()>") }}

A package table at `path`; a `LuauModule` registered and frozen; a check that `name` is a valid
debug name under the roots. See `module` on [Binding and userdata](@/docs/api/bind.md).

{{ api_signature(value="fn debug_roots(&self) -> &[Box<str>]") }}

{{ api_signature(value="fn open_standard_libraries(&self)") }}

{{ api_signature(value="fn sandbox_globals(&self)") }}

The configured roots; `luaL_openlibs` (call once); `luaL_sandbox`, which makes the globals and
the standard library tables read-only and the globals table a safe environment, for hosts that
do not use `Sandbox`.

### Compile options and the plan

{{ api_signature(value="fn compile_options(&self) -> CompileOptions") }}

{{ api_signature(value="fn set_compile_options(&self, options: CompileOptions) -> Result<()>") }}

The options `exec`, `eval` and `load_function` use. A runtime made from a plan derives them
from the plan and refuses to replace them (`Error::Logic`).

{{ api_signature(value="fn plan(&self) -> Option<Rc<RuntimePlan>>") }}

{{ api_signature(value="fn type_definitions(&self) -> Option<String>") }}

{{ api_signature(value="fn register_packed<T: PackedScalar>(&self) -> Result<()>") }}

The plan this runtime came from; the `.d.luau` for that plan including installed module
members; and, for a hand-assembled runtime only, registering `T` as the owner of its
[packed kind](@/docs/api/extension.md). Registering the same type twice is a no-op; another
type on the same kind number, or any registration on a planned runtime, is a logic error.

### Host state

{{ api_signature(value="fn insert_state<S: 'static>(&self, state: S) -> Rc<S>") }}

{{ api_signature(value="fn host_state<S: 'static>(&self) -> Option<Rc<S>>") }}

{{ api_signature(value="fn state_of<S: 'static>(&self, owner: &'static str) -> Option<Rc<S>>") }}

Runtime-owned state by type, one value per type per namespace, dropped before the VM closes.
The host's namespace is `insert_state`/`host_state`; extensions store theirs through
`InstallContext::insert_state` and `state_of` reads the state extension `owner` stored.

### Limits, categories and the profiler

{{ api_signature(value="fn limits(&self) -> Limits") }}

{{ api_signature(value="fn set_limits(&self, limits: Limits)") }}

The watchdog limits; a change takes effect for the next call scope.

{{ api_signature(value="fn call_scope(&self, context: CallContext, kind: CallKind) -> CallScope<'_>") }}

Opens a call scope for `context`; scopes must nest strictly. See `CallScope` below.

{{ api_signature(value="fn initialization_context(&self) -> CallContext") }}

`INITIALIZATION_CONTEXT` in the configured initialization category, the context sandbox and
template setup runs under.

{{ api_signature(value="fn total_bytes(&self) -> usize") }}

{{ api_signature(value="fn total_bytes_in(&self, category: MemoryCategory) -> usize") }}

{{ api_signature(value="fn set_memory_category(&self, category: MemoryCategory)") }}

Bytes allocated across all categories, or in one; and the main thread's active category, which
allocations are attributed to.

{{ api_signature(value="fn call_stats(&self) -> CallStats") }}

{{ api_signature(value="fn set_sampled_context(&self, context: Option<u64>)") }}

{{ api_signature(value="fn sampled_context(&self) -> Option<u64>") }}

{{ api_signature(value="fn samples(&self) -> Samples") }}

Accumulated call statistics (profiler only); selecting one context to sample every 32nd
safepoint while it is the innermost active call (changing the selection clears the samples,
and nothing costs anything while nothing is sampled); and a copy of the samples so far.

{{ api_signature(value="fn stats_frame(&self) -> u64") }}

{{ api_signature(value="fn advance_stats_frame(&self)") }}

The host's frame counter for statistics, which `profiler::FrameStats::catch_up` folds against.

{{ api_signature(value="fn caller_location(&self) -> String") }}

`source:line` of the innermost Lua code running on the main thread; empty when none is.

### The collector

{{ api_signature(value="fn collect_garbage(&self)") }}

{{ api_signature(value="fn gc_step(&self, steps: i32) -> bool") }}

{{ api_signature(value="fn gc_step_timed(&self, steps: i32) -> (bool, Duration)") }}

A full collection (Rust destructors of unreachable userdata run inside); one incremental step
of `steps` kilobytes (0 is one basic step), true when a cycle finished; the same with its wall
time. `Runtime::gc(GcControl)` and the rest of the collector controls are on
[Direct access and the VM](@/docs/api/direct.md).

### The stack

{{ api_signature(value="fn stack(&self) -> Stack<'_>") }}

The main thread's root stack at host level, valid while the runtime is. Panics if a root stack
from this runtime is alive and not suspended inside a Lua call. Inside a bound function use the
`Call` scope instead.

### Also on `Runtime`

Methods other modules add to `Runtime` are documented with their module:
`sandbox` (below); `new_thread`, `set_hooks`, `clear_hooks`, `gc`, `allocation_rate`, `clock`,
`encode_pointer`, `memory_dump`, `gc_dump`, `set_light_userdata_name`, `light_userdata_name`,
`weak_ref`, `set_embedder_gc`, `clear_embedder_gc`, `enable_coroutine_finalizers`,
`finalizer_function`, `atom_catalogue`, `atom_of`, `install_vector_buffer_writer` on
[Direct access and the VM](@/docs/api/direct.md); `open_library`, `sandbox_luau`,
`register_library`, `find_table`, `set_jit_inliner`, `install_require`, `require_function`,
`proxy_require_function`, `register_require_module`, `clear_require_cache_entry`,
`clear_require_cache` on [Binding and userdata](@/docs/api/bind.md); `native_code` on
[Native code and analysis](@/docs/api/native-code.md).

## PointerEncodingKey

{{ api_signature(value="struct PointerEncodingKey(pub [u64; 4])") }}

Four 64-bit words for `lua_setpointerencodekey`. `Clone`, `Copy`, `Debug`, `Eq`.

{{ api_signature(value="fn is_identity(&self) -> bool") }}

{{ api_signature(value="fn random() -> Self") }}

Whether the key reproduces Luau's default identity map (`lua_setpointerencodekey` clears `a`'s
low bit and sets `b`'s); a fresh key from OS entropy, redrawn while it would be the identity.

## INITIALIZATION_CONTEXT

{{ api_signature(value="const INITIALIZATION_CONTEXT: u64 = u64::MAX") }}

The context id call scopes use for sandbox and template setup.

## caller_location

{{ api_signature(value="fn caller_location(scope: &impl Scope) -> String") }}

`source:line` of the innermost Lua code running on `scope`'s thread; from inside a bound
function, the script line that called it. Empty when no Lua code is running there.

## CallKind, CallContext and CallScope

{{ api_signature(value="enum CallKind { ScriptCall, Initialization, HostInterface }") }}

What kind of call a scope wraps. `Clone`, `Copy`, `Debug`, `Eq`.

| Variant | Timed | Time-limited |
|---|---|---|
| `ScriptCall` | yes | yes |
| `Initialization` | no | no |
| `HostInterface` | no | yes |

{{ api_signature(value="struct CallContext { pub id: u64, pub category: MemoryCategory }") }}

Who is running, for accounting: a host-defined context id and the memory category allocations
are attributed to. `Clone`, `Copy`, `Debug`, `Eq`.

{{ api_signature(value="struct CallScope<'r>") }}

An active script call. Opening one pushes an active-call context so the interrupt and
allocation callbacks know a script is running, switches the memory category when the profiler
is on, and arms the watchdog deadline (the outermost or earliest one wins); dropping it records
self time (elapsed minus nested), allocation activity, and restores the previous category and
deadline. A scope is a no-op when neither the profiler nor a limit needs it.

```rust
use l3i::Runtime;
use l3i::runtime::{CallContext, CallKind, MemoryCategory};

fn main() -> l3i::Result<()> {
    let runtime = Runtime::builder().profiler(true).build()?;
    let script = runtime.load_function("return function() local s = 0 for i = 1, 1000 do s = s + i end end")?;
    {
        let _scope = runtime.call_scope(CallContext { id: 7, category: MemoryCategory(2) }, CallKind::ScriptCall);
        script.invoke::<(), _>(&runtime.stack(), ())?;
    }
    assert_eq!(runtime.call_stats().timed_calls, 1);
    Ok(())
}
```

## MemoryCategory and Limits

{{ api_signature(value="struct MemoryCategory(pub u8)") }}

A Luau memory category (0..256). Ids and their meanings are host data; OpenMW uses 0 for
shared, 1 global, 2 menu, 3 player. `Clone`, `Copy`, `Debug`, `Default`, `Eq`, `Hash`.

{{ api_signature(value="struct Limits { pub execution_time: Duration, pub memory_bytes: usize }") }}

Watchdog limits; zero disables a limit. `Clone`, `Copy`, `Debug`, `Default`, `Eq`.

## CallStats, Samples and SampledLocation

{{ api_signature(value="struct CallStats { pub total_script_ms: f64, pub time_by_context_ms: HashMap<u64, f64>, pub allocation_by_context: HashMap<u64, u64>, pub timed_calls: u64, pub overhead_samples: u64, pub overhead_total_ms: f64 }") }}

Accumulated profiler statistics: total measured self time of script calls, self time and
allocation activity (bytes grown) per host context id, timed calls, and the overhead samples
taken every 64th call. `Clone`, `Debug`, `Default`, `PartialEq`.

{{ api_signature(value="struct Samples { pub lines: BTreeMap<String, SampledLocation>, pub functions: BTreeMap<String, SampledLocation>, pub count: u64 }") }}

{{ api_signature(value="struct SampledLocation { pub function: String, pub samples: u64 }") }}

Where a sampled context spends its time: innermost Lua frames by `source:line`, and every Lua
function on the sampled stacks by `source:linedefined` (counted once per sample however deep it
recurses). `count` is the samples that found at least one Lua frame. Both `Clone`, `Debug`,
`Default`, `Eq`.

## Profiler statistics

Module `l3i::runtime::profiler`: the averaging and peak-decay rules OpenMW's script statistics
use, as pure data with no VM involved. Report text and UI stay in the host.

| Constant | Value | Meaning |
|---|---|---|
| `AVERAGE_COEFFICIENT: f64` | `1.0 / 30.0` | Averaging weight: approximately the last 30 frames |
| `PEAK_DECAY: f64` | `0.983` | Per-frame peak decay: a spike fades to about 5% over approximately 180 frames |

{{ api_signature(value="struct RollingAverage") }}

A rolling average seeded by its first sample. `Clone`, `Copy`, `Debug`, `Default`, `PartialEq`.

| Method | Meaning |
|---|---|
| `const fn new() -> RollingAverage` | Unseeded |
| `fn update(&mut self, sample: f64)` | The first sample seeds; later ones blend with `AVERAGE_COEFFICIENT` |
| `fn get(&self) -> f64` | The current average |
| `fn is_seeded(&self) -> bool` | Whether a sample has arrived |

{{ api_signature(value="struct PhaseStats { pub total_ms: f64, pub scripts_ms: f64, pub last_total_ms: f64, pub last_scripts_ms: f64 }") }}

Wall time of a phase and the script time within it, averaged together so both cover the same
frames. `fn record(&mut self, total_ms: f64, scripts_ms: f64)` folds one frame. `Clone`,
`Copy`, `Debug`, `Default`, `PartialEq`.

{{ api_signature(value="struct FrameStats { pub avg_call_time_ms: f64, pub avg_queued_time_ms: f64, pub avg_allocation_activity: f64, pub avg_calls: f64, pub peak_call_time_ms: f64, pub call_time_this_frame_ms: f64, pub queued_time_this_frame_ms: f64, pub allocation_activity_this_frame: u64, pub calls_this_frame: u64, pub frame: u64 }") }}

Per-context statistics kept across frames: this frame's accumulators plus the averages and peak
they fold into. `Clone`, `Copy`, `Debug`, `Default`, `PartialEq`.

| Method | Meaning |
|---|---|
| `fn new(frame: u64) -> FrameStats` | Fresh statistics attributed to `frame` |
| `fn add_call(&mut self, milliseconds: f64)` | A call that ran during the current frame |
| `fn add_queued(&mut self, milliseconds: f64)` | Time in queued (deferred) work attributed to this context |
| `fn add_allocation_activity(&mut self, bytes: u64)` | Bytes grown this frame |
| `fn catch_up(&mut self, frame: u64)` | Folds the recorded frame into the averages and peak, then decays them for the idle frames in between as if each had been folded; nothing when already at or past `frame` |

## Sandbox

Module `l3i::sandbox`. [Modules and sandboxes](@/docs/modules-and-sandboxes.md) explains the
model: a frozen base environment copying every string-keyed global except `_G`, `getfenv`,
`setfenv`, `newproxy`, `getmetatable`, `print` and `require`, with tables as read-only views; a
writable instance environment per script whose frozen metatable indexes the base env; and
templates compiled once on a loader thread and cloned per script.

{{ api_signature(value="struct SandboxOptions { pub compile_options: CompileOptions, pub compat_iterators: bool, pub compat_string_format: bool, pub neuter_randomseed: bool }") }}

| Field | Default | Meaning |
|---|---|---|
| `compile_options` | `CompileOptions::default()` | Compile options for templates |
| `compat_iterators` | `true` | Replace the global `pairs`/`ipairs` with versions honouring `__pairs`/`__ipairs` (stock Luau ignores both). Read-only views and iterable userdata depend on it |
| `compat_string_format` | `false` | Wrap `string.format` so `%s` applies `tostring` to any value, as LuaJIT does |
| `neuter_randomseed` | `false` | Seed `math.random` once from the clock and make `math.randomseed` a no-op |

`Clone`, `Debug`, `Default`.

{{ api_signature(value="fn sandbox(&self, log: impl Fn(&str) + 'static, options: SandboxOptions) -> Result<Sandbox>") }}

On `Runtime`: prepares a sandbox on this VM, installing the prelude and the chosen
compatibility shims into the real globals. Call once, after every module the base env should
expose is registered. `log` receives each `print` line, already tab-joined and prefixed with the
instance name. Needs at least one debug root.

{{ api_signature(value="struct Sandbox") }}

A prepared sandbox for one VM: the base environment and the generators.

| Method | Meaning |
|---|---|
| `fn base_env(&self) -> &Table` | The frozen base environment every instance indexes |
| `fn common_packages(&self) -> &BTreeMap<String, Value>` | Packages every instance receives: the base env's tables and userdata plus those added below |
| `fn add_common_package(&mut self, runtime: &Runtime, name: impl Into<String>, package: Value) -> Result<()>` | Adds or replaces a package every instance receives. Tables are frozen in place; a function is a factory called per instance with the hidden data |
| `fn new_instance(&self, runtime: &Runtime, spec: &InstanceSpec<'_>) -> Result<Instance>` | One instance environment, built inside an initialization call scope |
| `fn load_template(&self, runtime: &Runtime, chunk_name: &str, source: &str) -> Result<Template>` | Compiles `source` once and loads it on the loader thread, whose globals are the base env, so Luau resolves the chunk's builtin imports against it. Binary chunks are rejected. With `jit` and a runtime built with native code, the closure is compiled natively and the outcome kept on the template. Takes the root stack |
| `fn load_template_in(&self, scope: &impl Scope, chunk_name: &str, source: &str) -> Result<Template>` | `load_template` on any scope of the VM, for a `require` loader written in Rust; needs no `Runtime`, since the initialization context and the native code generator live with the VM |
| `fn instantiate(&self, runtime: &Runtime, template: &Template, env: Option<&Table>) -> Result<Function>` | A fresh closure sharing the template's prototype (`lua_clonefunction` + `lua_setfenv`), running in `env` or the real globals. Takes the root stack |
| `fn instantiate_in(&self, scope: &impl Scope, template: &Template, env: Option<&Table>) -> Result<Function>` | `instantiate` on an existing scope of the template's VM |
| `fn run(&self, runtime: &Runtime, template: &Template, instance: &Instance, context: CallContext) -> Result<Vec<Value>>` | Instantiates `template` in `instance` and runs it inside a `ScriptCall` scope for `context`, returning everything the chunk returned |

Errors: a package, loader, hidden value, template or environment from another VM is
`Error::Logic`; a script error from `run` is `Error::Runtime` with `Lua error: ` and the message.

{{ api_signature(value="struct InstanceSpec<'a> { pub name: &'a str, pub packages: &'a [(&'a str, &'a Value)], pub hidden_data: Option<&'a Value>, pub loader: &'a Function }") }}

| Field | Meaning |
|---|---|
| `name` | The instance's name; `print` prefixes its output with `name:` |
| `packages` | Per-instance packages, added after the common ones and overriding them by name. Functions are factories called once with the hidden data |
| `hidden_data` | Passed to package factories; nil when absent |
| `loader` | Behind `require`: called as `loader(name, env)` for a name missing from `loaded`, and must return a function that, called with `name`, produces the package |

{{ api_signature(value="struct Instance { pub env: Table, pub loaded: Table }") }}

One script's environment and its `loaded` packages table.

{{ api_signature(value="struct Template") }}

A compiled script, loaded once, instantiated many times. `Debug`.

| Method | Meaning |
|---|---|
| `fn chunk_name(&self) -> &str` | The name it was loaded under |
| `fn native_code(&self) -> Option<NativeCodeResult>` | Feature `jit`: how native compilation went, when the runtime has a generator |

```rust
use l3i::Runtime;
use l3i::runtime::{CallContext, MemoryCategory};
use l3i::sandbox::{InstanceSpec, SandboxOptions};

fn main() -> l3i::Result<()> {
    let runtime = Runtime::new()?;
    let sandbox = runtime.sandbox(|line| println!("{line}"), SandboxOptions::default())?;
    let loader = runtime.load_function(
        "return function(name, env) return function(n) if n == 'answer' then return 42 end error('module ' .. n .. ' not found') end end",
    )?;
    let spec = InstanceSpec { name: "scriptA", packages: &[], hidden_data: None, loader: &loader };
    let instance = sandbox.new_instance(&runtime, &spec)?;
    let template = sandbox.load_template(&runtime, "script.lua", "print('hello') return require('answer')")?;
    let results = sandbox.run(&runtime, &template, &instance, CallContext { id: 1, category: MemoryCategory(0) })?;
    assert_eq!(results.len(), 1);
    Ok(())
}
```

## CompileOptions

Module `l3i::source`.

{{ api_signature(value="struct CompileOptions { pub optimization_level: u8, pub debug_level: u8, pub type_info_level: u8, pub coverage_level: u8, pub vector_lib: Option<CString>, pub vector_ctor: Option<CString>, pub vector_type: Option<CString>, pub mutable_globals: Vec<CString>, pub userdata_types: Vec<CString>, pub disabled_builtins: Vec<CString>, pub known_libraries: Vec<CString>, pub library_members: Option<Rc<dyn LibraryMembers>> }") }}

Luau compiler policy. An explicit `CompileOptions` always travels with the source, never
Luau's silent defaults. `Clone`, `Debug`, `Default`.

| Field | Default | Meaning |
|---|---|---|
| `optimization_level` | 2 | Luau's `-O` level |
| `debug_level` | 1 | Line information and function names |
| `type_info_level` | 0 | Type information for native code; runtime plans turn it on |
| `coverage_level` | 0 | Coverage instrumentation, for `DebugScope::coverage` |
| `vector_lib`, `vector_ctor` | none | An alternative global vector constructor `vector_lib.vector_ctor`, in addition to `vector.create` |
| `vector_type` | none | An alternative vector type name for type tables |
| `mutable_globals` | empty | Globals the compiler must not assume constant |
| `userdata_types` | empty | Userdata type names in type index order, the list the code generator sees too |
| `disabled_builtins` | empty | Builtins the compiler must not fast-call |
| `known_libraries` | empty | Libraries whose members the compiler may ask about through `library_members`; it folds constant members and specialises on member types |
| `library_members` | none | Answers the compiler's member queries for the known libraries |

{{ api_signature(value="trait LibraryMembers") }}

Compile-time knowledge about a library's members. Both methods default to `None`.

| Method | Meaning |
|---|---|
| `fn member_type(&self, library: &str, member: &str) -> Option<u8>` | The bytecode type of `library.member` as a `LuauBytecodeType` value (`native_code::ir::bytecode_type` constants) |
| `fn member_constant(&self, library: &str, member: &str) -> Option<CompileConstant>` | The constant value of `library.member`, if it is one |

{{ api_signature(value="enum CompileConstant { Nil, Boolean(bool), Number(f64), Integer(i64), Vector(f32, f32, f32), String(String) }") }}

A constant the compiler may fold in place of a library member access. `Clone`, `Debug`,
`PartialEq`.

{{ api_signature(value="fn compile(source: &str, options: &CompileOptions) -> Result<Vec<u8>>") }}

Compiles Luau source to bytecode. A compile error is `Error::Runtime` carrying Luau's message
(without a chunk name) rather than error bytecode. Freezes the flag policy first.

## Flags

Module `l3i::flags`: the process-global Luau feature-flag policy. Flags change the bytecode the
compiler emits, so `Runtime` creation and `source::compile` both freeze the same policy before
anything else.

{{ api_signature(value="const LUAU_FLAGS: &[&str]") }}

{{ api_signature(value="const LUAU_CODEGEN_FLAGS: &[&str]") }}

The flags OpenMW enables in the Ast, Bytecode, Compiler and VM components, plus
`LuauExperimentalIfLocalSyntax` (on here, off in OpenMW) and the two that gate APIs the binder
exposes (`LuauGcTraceUdata`, `LuauBufferCage`); and the CodeGen flags, applied only with the
`jit` feature. The X64 and A64 flags exist only on their own architecture.

{{ api_signature(value="fn initialize() -> Result<()>") }}

Freezes the policy. Idempotent and thread-safe. A flag name the linked Luau lacks is
`Error::Logic` (`Luau 0.740 has no fast flag named ...`) rather than a silent no-op.

## Error

Module `l3i::error`, re-exported from the root.

{{ api_signature(value="enum Error { Logic(String), Runtime(String), LuaErrorOnStack, Permission(String) }") }}

| Variant | Meaning |
|---|---|
| `Logic(String)` | A programming mistake found at registration or through API misuse (the C++ binder's `std::logic_error`) |
| `Runtime(String)` | An ordinary failure; it becomes the Lua error message when it reaches a native entry point |
| `LuaErrorOnStack` | The Lua error object is already on top of the stack; the native entry point re-raises it unchanged |
| `Permission(String)` | A capability the runtime policy did not grant, or a host facility a script may not use |

`Display` prints the message, or `Lua error` for `LuaErrorOnStack`. `Clone`, `Debug`, `Eq`,
`std::error::Error`.

| Constructor | |
|---|---|
| `fn logic(message: impl Into<String>) -> Self` | |
| `fn runtime(message: impl Into<String>) -> Self` | |
| `fn permission(message: impl Into<String>) -> Self` | |

{{ api_signature(value="type Result<T> = std::result::Result<T, Error>") }}

## Diagnostics

Module `l3i::diagnostics`: error text matching Luau's own `lauxlib` wording, so scripts see the
messages the C++ binder produced through `luaL_typeerror` and `luaL_argerror`.

{{ api_signature(value="fn type_error(value: ValueView<'_>, expected: &str) -> Error") }}

{{ api_signature(value="fn type_error_at(value: ValueView<'_>, position: c_int, expected: &str) -> Error") }}

The message `luaL_typeerror(L, narg, expected)` would raise for `value`, with the caller
location prefix `luaL_error` adds: `invalid argument #2 to 'name' (number expected, got
string)`, or `missing argument #2 to 'name' (number expected)` for a nonexistent slot. The first
numbers the argument by its raw stack position; the second takes the number explicitly.

{{ api_signature(value="fn object_type_name(value: ValueView<'_>) -> String") }}

`__type` from the metatable when present, else the basic type name (`luaT_objtypename`).

{{ api_signature(value="fn describe_value(value: ValueView<'_>, max_length: usize) -> String") }}

A non-executing description for error messages: booleans, numbers and (truncated, escaped)
strings by content, everything else as `<type>`.

{{ api_signature(value="fn truncate_diagnostic(text: &str, max_length: usize) -> String") }}

{{ api_signature(value="fn escape_diagnostic(text: &str) -> String") }}

UTF-8-safe bounded truncation with a trailing `...`; escaping of quotes, backslashes and
control bytes with multibyte UTF-8 passed through.

## Debug names

Module `l3i::debug_name`. A debug name is a dot-separated path of identifiers rooted at one of
the host's configured roots (`openmw.util.Vector3.__mul`, `dreamweave.archive.open`). Luau
borrows the name pointer for a closure's whole life, so names are interned in a VM-private
registry table for the VM's life.

{{ api_signature(value="fn is_valid_debug_name<R: AsRef<str>>(name: &str, roots: &[R]) -> bool") }}

{{ api_signature(value="fn require_valid_debug_name<R: AsRef<str>>(name: &str, roots: &[R]) -> Result<()>") }}

True when `name` is `<root>.<ident>(.<ident>)*` for one of `roots`; or `Error::Logic` naming the
roots when it is not.
