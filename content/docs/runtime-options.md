+++
title = "Runtime options"
description = "Everything Runtime::builder decides, the watchdog, the allocator-enforced memory limit and what it counts, memory categories, call scopes, the collector's controls, the profiler and sampler, and the fast-flag policy."
weight = 50

[extra]
kind = "guide"
+++

The host owns a `Runtime`, which owns one `lua_State`. `Runtime::new()` takes every default;
`Runtime::builder()` makes each step of the C++ binder's creation order a choice: flag policy,
`luaL_newstate`, the pointer-encoding seed, the buffer cage, the per-VM shared block and its
callbacks, native code generation, the atom catalogue, and last the standard libraries. Nothing
is entrenched by the constructor.

## The builder

| `RuntimeBuilder` | Default | Sets |
|---|---|---|
| `debug_roots(&["openmw", "string", "vector"])` | `dreamweave` | The root vocabulary for debug names. Every native function and userdata type registered into the VM must be named under one of them |
| `pointer_encoding(bool)` | on | Whether Luau's pointer-encoding key is seeded from OS entropy right after the state is created, before any table exists. Off leaves Luau's identity map, for deterministic test fixtures only |
| `standard_libraries(bool)` | on | Whether `build` opens the standard libraries. Hosts that need to install callbacks first turn it off and call `Runtime::open_standard_libraries()` |
| `execution_time_limit(Duration)` | zero, off | The watchdog's wall-clock budget per outermost call into Luau |
| `memory_limit(bytes)` | zero, off | The heap ceiling across every memory category, enforced by the allocator |
| `profiler(bool)` | off | Time script calls, switch memory categories per call, record allocation activity |
| `initialization_category(MemoryCategory)` | 0 | The category charged while sandboxes and templates are set up |
| `atom_catalogue(AtomCatalogue)` | none | This VM's atoms, installed before the libraries open; see [Direct access](@/docs/direct-access.md) |
| `buffer_cage(cage)` | none | Routes every `buffer` allocation through a `memory::BufferCage` |
| `native_code(options)` (`jit`) | off | Luau native code generation; see [Native code](@/docs/native-code.md) |
| `coroutine_finalizers(bool)` | off | Luau's coroutine finalizers: `coroutine.finally` for scripts, `Thread::add_finalizer` and `Thread::finalizer_function` for the host. The flag is process-wide, so the first runtime built in a process decides for every runtime after it: asking once a runtime exists without them fails `build`, and a runtime that does not ask gets them anyway once one has. `RuntimePlan` has no finalizer option, so a plan-built runtime cannot ask; build one plain runtime with them first |

```rust
use std::time::Duration;

use l3i::runtime::MemoryCategory;
use l3i::Runtime;

fn main() -> l3i::Result<()> {
    let runtime = Runtime::builder()
        .debug_roots(&["shapes", "dreamweave"])
        .execution_time_limit(Duration::from_millis(50))
        .memory_limit(64 * 1024 * 1024)
        .profiler(true)
        .initialization_category(MemoryCategory(1))
        .build()?;
    println!("heap {} bytes", runtime.total_bytes());
    Ok(())
}
```

`build` also proves the binder's mirror of Luau's 16-byte value layout against this build's
API and registers l3i's own packed kinds. `Runtime::from_plan(&plan)` is the other constructor,
for a runtime made from an extension plan; see [Extensions](@/docs/extensions.md).

## The watchdog

Both limits live in `runtime::Limits`, readable with `limits()` and changeable with
`set_limits`: a new time limit takes effect for the next call scope, a new memory limit at the
next allocation. The interrupt hook runs at Luau's safepoints and, every 64th safepoint, polls
the deadline; when it has passed it raises a Lua error, as OpenMW does:

```text
Lua execution time limit exceeded after 20 ms
```

The memory limit is the VM's allocator's. Every block Luau allocates goes through a counting
allocator, and a growth that would take the heap past the limit fails at the allocation with
Luau's out-of-memory error. A script can catch that with `pcall`; when it reaches the host it is
`Error::MemoryLimit` (`Lua memory limit exceeded`). Frees and shrinks always succeed, so the
collector, finalizers and closing the runtime work at any total, and the runtime stays usable
once the memory is released.

The error unwinds to the nearest `pcall`, comes back to the host as `Err`, and leaves the VM
usable. The hook ignores the safepoints Luau reports during collection, so a GC step never
charges a script's budget.

A timeout is sticky. Once the deadline has run out, every safepoint raises again until the scope
that armed the deadline ends, so a script that catches the error with `pcall` or `xpcall`, or
runs the loop inside a coroutine, stops at the next instruction boundary instead of carrying on.
The host sees `Error::ExecutionTimeLimit` (`Error::is_limit()`), never an ordinary
`Error::Runtime`, whatever the script did with the error object on the way out.

The execution time limit is per outermost call into Luau. A call scope (below) arms it; every
host entry that runs Luau code (`exec`, `eval`, `load_function`, the `Function::invoke` family,
`Thread::start` and `resume`, and host-level table access that can run a metamethod) opens an
implicit `HostInterface` scope with context `CallContext::HOST` when no scope is active, so the
runtime's limits hold for each of them. An `Initialization` scope is never time-limited: a host
that genuinely needs an unlimited call opens one around it.

{% callout(kind="warning", title="What the memory limit counts") %}
`memory_limit` counts every allocation in the VM, host-level setup and the standard libraries
included, so leave room for them. It does not count buffers routed through a
`memory::BufferCage`, or memory a native extension allocates from Rust (`@dream/bytes`,
`@dream/data`, `@dream/fs` readers): bound those in the extension.
{% end %}

## Memory categories

`runtime::MemoryCategory(u8)` is a Luau memory category, `0..256`. The ids and their meanings
are host data; OpenMW uses 0 for shared, 1 global, 2 menu, 3 player, and so on.
`Runtime::total_bytes_in(category)` reads what one category holds, `total_bytes()` the whole
heap, and `set_memory_category` switches the main thread's active category by hand. With the
profiler on, a call scope switches it for the call's duration.

## Call scopes

`Runtime::call_scope(context, kind)` wraps one script call from the host, as
`Detail::ScriptCallScope` does. It pushes an active-call context so the interrupt and allocation
callbacks know a script is running, switches the memory category when the profiler is on, arms
the watchdog deadline (the outermost or earliest one wins), and on drop records self time
(elapsed minus nested), allocation activity, and restores the previous category and deadline.
Scopes nest strictly, and a scope costs nothing when neither the profiler nor a limit needs it.

```rust
use std::time::Duration;

use l3i::runtime::{CallContext, CallKind, MemoryCategory};
use l3i::Runtime;

fn main() -> l3i::Result<()> {
    let runtime = Runtime::builder().execution_time_limit(Duration::from_millis(20)).profiler(true).build()?;
    let spin = runtime.load_function("return function() while true do end end")?;
    let error = {
        let _scope = runtime.call_scope(CallContext { id: 7, category: MemoryCategory(3) }, CallKind::ScriptCall);
        spin.invoke::<(), _>(&runtime.stack(), ()).unwrap_err()
    };
    println!("{error}");
    let stats = runtime.call_stats();
    println!("{} timed calls, {:.1} ms in context 7", stats.timed_calls, stats.time_by_context_ms[&7]);
    Ok(())
}
```

`CallContext` is a host-defined context id plus the memory category allocations are attributed
to. `CallKind` says what the scope wraps:

| `CallKind` | Timed | Time-limited |
|---|---|---|
| `ScriptCall` | yes | yes |
| `Initialization` | no | no |
| `HostInterface` | no | yes |

`Runtime::initialization_context()` is the context sandboxes and templates are set up under:
`INITIALIZATION_CONTEXT` in the configured initialization category.

## The collector

Luau has no `collectgarbage`, so a host that wants collection at frame boundaries steps it
there. `Runtime::gc(control)` is `lua_gc`; `collect_garbage()` runs a full cycle, `gc_step(kb)`
one incremental step and `gc_step_timed(kb)` the same with its wall time, for GC accounting.

| `memory::GcControl` | Does |
|---|---|
| `Stop`, `Restart`, `IsRunning`, `IsPaused` | Collector state |
| `Collect` | A full cycle |
| `Count`, `CountRemainder` | Heap size in KB, and the remainder in bytes |
| `Step(kb)` | One incremental step of `kb` kilobytes (0 for a basic step); returns 1 when a cycle ended |
| `SetGoal(percent)` | Target heap growth before a new cycle |
| `SetStepMultiplier(value)`, `SetStepSize(kb)` | How fast the collector runs relative to allocation, and how often |

The defaults are Luau's and are kept: a 200 percent goal (the heap may double relative to the
live heap), a step multiplier of 200 (the collector runs twice the speed of allocation), and
1 KB steps. `allocation_rate()` is Luau's estimate in bytes per second, `-1` until it has
enough history. Dumps, the buffer cage, embedder GC and weak references are on
[the VM page](@/docs/vm.md#memory).

## The profiler

With `profiler(true)`, every script call scope is timed and its allocation activity recorded.
`Runtime::call_stats()` returns `CallStats`: total script self time in milliseconds, self time
and allocation activity per context id, the number of timed calls, and the overhead samples
taken every 64th call. Allocation activity is growth charged to the innermost active call; Luau
does not report every fixed GC allocation, so it is activity, not ownership. `caller_location()`
gives `source:line` of the innermost Lua code running on the main thread, and
`runtime::caller_location(scope)` the same from inside a bound function.

`set_sampled_context(Some(id))` samples one context at every 32nd safepoint while it is the
innermost active call. `samples()` returns `Samples`: the innermost Lua frames by
`source:line`, every Lua function on the sampled stacks by `source:linedefined` (counted once
per sample however deep it recurses), and the sample count. Changing the selection clears the
samples; sampling nothing costs nothing.

`runtime::profiler` holds the pure statistics half of OpenMW's script profiler, with no VM
involved: `RollingAverage` (seeded by its first sample, weight `AVERAGE_COEFFICIENT`, about the
last 30 frames), `PhaseStats` (wall time of a phase and the script time within it, averaged
together), and `FrameStats`, whose `catch_up(frame)` folds a frame's accumulators into the
averages and a peak that decays by `PEAK_DECAY` per frame. `stats_frame()` and
`advance_stats_frame()` are the host's frame counter for it. Report text and UI stay in the host.

## Fast flags

Luau feature flags are global to the process, and several change the bytecode the compiler
emits, so `flags::initialize()` freezes one policy before the first VM is created or the first
`source::compile` runs; both call it, and it is idempotent. The policy is l3i's own named list
(`flags::LUAU_FLAGS`, plus `flags::LUAU_CODEGEN_FLAGS` under `jit`), never Luau's "enable
everything". Every flag on it is there for a stated reason, in the source next to its name:

- The compiler and runtime optimizations and fixes that Luau 0.742 still gates, including the
  two 0.742 miscompile fixes (`LuauCompileMoveElisionFix`, `LuauCompileTableFoldFix`) and the
  0.741 GC pacing fix (`LuauGcHeapShrinkFix`), which read a heap that shrank between
  measurements as an enormous allocation rate.
- `LuauExperimentalIfLocalSyntax`, so `if local x = f() then .. end` parses. The C++ binder
  leaves it off.
- `LuauGcTraceUdata` and `LuauBufferCage`, which gate the embedder GC and memory cage APIs the
  binder exposes; both are inert until a host installs the callback.
- `LuauCoroutineFinally` is the one flag not on the list: the builder's `coroutine_finalizers`
  raises it, once per process, before the first VM.

A flag name the linked Luau lacks is a logic error rather than a silent no-op, so drift against
the linked Luau release is caught at once; flags that graduate, as `LuauCompileIifeInline` did in
0.741, come off the list with the bump. `memory::fast_flags()`, `fast_flag(name)`, `fast_int(name)`,
`set_fast_flag` and `set_fast_int` read and set flags by name; setting is for Luau's `Debug*`
flags only once a runtime exists.

## Compiler options

`source::CompileOptions` always travels with source, never Luau's silent defaults. The defaults
match OpenMW: optimisation level 2, debug level 1 (line info and function names), no type
information, no coverage. `Runtime::compile_options()` and `set_compile_options` read and
replace what `exec`, `eval` and `load_function` use; a runtime made from a plan derives them
from the plan and refuses. `known_libraries` plus a `LibraryMembers` implementation let the
compiler fold constant members and specialise on member types for the named libraries.
