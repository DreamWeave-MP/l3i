+++
title = "Coroutines, debugging and the rest of the VM"
description = "Host-driven threads and yields from bound functions, the debug API and runtime hooks, memory and collector controls beyond the watchdog, the standard-library utilities, require, the analysis feature, and what the raw FFI declares."
weight = 70

[extra]
kind = "guide"
+++

## Coroutines

`thread::Thread` is a pinned Lua thread driven from the host. `Runtime::new_thread()` creates
one sharing the VM's globals and registry; `start(&scope, &function, args)` runs a function on
it until it yields, returns, breaks or fails; `resume(&scope, args)` continues a yielded
coroutine with the values its `yield` receives; and `resume_with_error(&scope, message)`
continues it by raising inside it, as if the `yield` failed. Each step reports a
`thread::Resume`:

| `Resume` | Meaning |
|---|---|
| `Yielded(values)` | The coroutine yielded these pinned values and can be resumed |
| `Finished(values)` | Its function returned these values; the thread is finished |
| `Break` | It hit `lua_break`: a breakpoint, or a bound function returning `bind::Break` |

```rust
use l3i::thread::Resume;
use l3i::{Error, Runtime};

fn main() -> l3i::Result<()> {
    let runtime = Runtime::new()?;
    let generator = runtime.load_function("return function(a) local b = coroutine.yield(a + 1) return a + b end")?;
    let thread = runtime.new_thread()?;
    let Resume::Yielded(values) = thread.start(&runtime.stack(), &generator, (1,))? else {
        return Err(Error::logic("expected a yield"));
    };
    let first = values[0].with_value(&runtime.stack(), |_, view| view.read::<f64>())?;
    let Resume::Finished(values) = thread.resume(&runtime.stack(), (10,))? else {
        return Err(Error::logic("expected a return"));
    };
    let result = values[0].with_value(&runtime.stack(), |_, view| view.read::<f64>())?;
    println!("{first} {result}");
    Ok(())
}
```

It prints `2 11`. An error inside the coroutine comes back as `Err` and leaves the thread in
`ThreadStatus::Error(code)`, as `coroutine.resume` would report; `status()` is Luau's
`lua_status` (`Ok`, `Yielded`, `Break`, `Error`) and `coroutine_status(&scope)` its
`lua_costatus` as seen from another thread (`Running`, `Suspended`, `Normal`, `Finished`,
`FinishedWithError`). A thread that finished normally is idle and can be started again; a
suspended or dead one needs `reset()` first, and `is_reset()` says whether it holds a function.

`with_stack(&scope, |stack| ..)` runs a closure with the coroutine's own root stack, for
pushing arguments or reading values between resumes, never for calling into Lua on a suspended
thread. The lease is per Lua thread: a root on the main thread and a root on each coroutine
coexist, and a nested `with_stack` on one thread panics like a second `Runtime::stack()`.
`sandbox(&scope)` is `luaL_sandboxthread`, a writable globals table proxying the frozen main
globals. `data()` and the `unsafe` `set_data(pointer)` attach host data to the thread; the
binder only stores the pointer, inside its own per-thread record.

Bound functions take part from the other side. Returning `bind::Yield(results)` yields the inner
results to the coroutine resuming the function (`lua_yield`), and `bind::Break` requests a
debugger stop (`lua_break`). Both need the function to be running inside a coroutine with no
C-call boundary in between; otherwise Luau raises `attempt to yield across metamethod/C-call
boundary`, and `Call::is_yieldable()` says in advance. Bound functions called from
`coroutine.wrap` bodies work as anywhere else, raises included.

## Debugging

`debug::DebugScope` is implemented for every `Scope`, so a `Stack`, a `Frame` or a `Call`
answers debug queries about its thread's call stack:

| Method | Returns |
|---|---|
| `debug_info(level)` | The `DebugInfo` record `level` frames up (0 is the running function): `what` (`"Lua"` or `"C"`), `source`, `short_source`, `name`, `line_defined`, `current_line`, counts, `is_vararg` |
| `call_site(level)` | Only the chunk name and current line of that frame, as a `CallSite`, for attributing every native call to the script line that made it (level 1 from inside a bound function). Luau records lines, not columns: no binder can give a column |
| `function_info(view)` | The record of the function at a stack slot |
| `stack_depth()`, `debug_trace()`, `traceback(message, level)` | Frame count, Luau's own trace, and `luaL_traceback` |
| `local(level, n)`, `set_local(level, n)` | Push local `n` of a frame with its name, or pop the top value into it |
| `argument(level, n)` | Push argument `n` of a frame, vararg-aware |
| `upvalue(view, n)`, `set_upvalue(view, n)` | The same for a function's upvalues |
| `single_step(enabled)` | Every instruction on this thread then reaches `RuntimeHooks::debug_step` |
| `set_breakpoint(view, line, enabled)` | Sets or clears a breakpoint; returns the line it landed on |
| `coverage(view)` | Line hit counts of a function and its nested functions (`lua_getcoverage`); the chunk must be compiled with `coverage_level` above 0 |

Local and upvalue names need `debug_level` 2 in the `CompileOptions`; optimisation level 2 may
inline a small function away, which optimisation level 1 keeps as a frame.

`debug::RuntimeHooks` is one trait for every remaining `lua_Callbacks` slot, each method with a
no-op default, and `Runtime::set_hooks(hooks, HookSet)` installs the slots named in the set
(`HookSet::DEBUGGER`, `ALL`, `NONE`, or a struct of booleans) so the VM pays only for the ones
in use. `clear_hooks()` removes them. The hooks that receive a `Stack` (`debug_break`,
`debug_step`, `debug_interrupt`, `debug_protected_error`) may use the Lua API on that thread;
the ones that receive raw states (`user_thread`, `user_finalizer`, `pre_resume`, `post_resume`,
`on_free`, `panic`) may not, which is Luau's documented restriction. Hooks run inside Luau
callbacks and must not panic.

`debug_break` returns a `DebugAction`: `Continue`, or `Break` to stop the thread with
`LUA_BREAK`, which is only possible on a thread the host drives through `Thread`; on the main
thread Luau raises `attempt to break across metamethod/C-call boundary`. Resuming re-executes
the instruction, so the hook is called again for the same breakpoint and must answer `Continue`
to step off it, which is how Luau's own tests do it.

## Memory

`memory` holds the controls beyond the watchdog. `Runtime::gc(GcControl)`, `allocation_rate()`
and the defaults are on [Runtime options](@/docs/runtime-options.md#the-collector).

| Facility | Calls |
|---|---|
| Dumps | `memory_dump(path)` writes object counts and sizes per category; `gc_dump(path, category_names)` the full heap graph |
| Buffer cage | `RuntimeBuilder::buffer_cage(cage)` routes every `buffer` allocation through a `BufferCage`, installed before any buffer exists, so the host can place buffers in a guarded region or account for them exactly |
| Embedder GC | `set_embedder_gc(gc)` installs the embedder half of cross-heap marking: each cycle Luau asks the `EmbedderGc` to `reset`, marks userdata through `UserdataMark` callbacks (`memory::set_userdata_mark::<T, M>(&runtime)`, for a tagged `T`), then asks it to mark every weak reference reachable from marked native objects. `clear_embedder_gc()` removes it |
| Weak references | `weak_ref(view)` returns a `WeakRef` that does not keep its value alive; `get(scope)` pushes the value while it lives, `release(scope)` frees the slot. Unmarked weak references die at the next cycle |
| Light userdata | `LightUserdata { pointer, tag }` pushes and reads `lua_pushlightuserdatatagged`; `set_light_userdata_name(tag, name)` is what `typeof` reports for the tag |
| Coroutine finalizers | `enable_coroutine_finalizers()` turns on Luau's experimental `DebugLuauCoroutineFinally`; `Thread::add_finalizer(scope, callback)` attaches a function to a live coroutine, `has_finalizers()` asks, and `finalizer_function()` is the pinned `finalize(coroutine)` that runs them |
| Raw tags | `memory::raw::set_userdata_tag` and `new_userdata_tagged`, both `unsafe`, for hosts that manage a tag's meaning themselves |
| Fast flags | `fast_flags()`, `fast_flag(name)`, `set_fast_flag`, `fast_int`, `set_fast_int` |

`Runtime::clock()` is Luau's high-resolution clock in seconds, and `encode_pointer(pointer)`
encodes an address with the VM's pointer-encoding key, as `tostring` does.

## Libraries

`libraries` covers standard-library and VM utility entry points the C++ binder never needed:

- `Runtime::open_library(Library)` opens one standard library at a time (`Base` first);
  `Library::STANDARD` is the twelve `luaL_openlibs` opens, in its order, and `Library::Class` is
  Luau's experimental `class` library.
- `register_library(name, &[(name, lua_CFunction)])` is `luaL_register`: it creates or reuses the
  global table and fills it with C functions named `name.function`. `find_table("a.b.c")` is
  `luaL_findtable`, creating tables along the way.
- `TableView::clone_table(frame)` and `clear(frame)` are `lua_clonetable` and `lua_cleartable`;
  `Frame::concat(count)`, `equal(a, b)` and `less_than(a, b)` honour `__concat`, `__eq` and
  `__lt`.
- `libraries::StringBuilder::new(scope)` is Luau's `luaL_Strbuf`: `push_str`, `push_bytes`,
  `push_value` (`tostring`-style text of a slot), then `finish()` pushes the result. It keeps its
  spill storage on the stack below anything pushed after it, so finish it before popping through
  it.
- `set_jit_inliner(enabled)` toggles Luau's experimental inliner for the VM.
- `Runtime::load_with_env` loads a chunk whose globals resolve through a table; `CompileOptions`
  with `known_libraries` and a `LibraryMembers` implementation let the compiler fold constant
  library members at compile time.

## `require`

Luau's require-by-string runtime runs over a host `require::RequireNavigator`, with caching,
proxy requires, registered modules and cyclic-require placeholders supplied by Luau's own
implementation. [Modules and sandboxes](@/docs/modules-and-sandboxes.md#require-over-a-host-navigator)
documents the trait and the `Runtime` methods around it.

## Analysis

With the `analysis` feature, `analysis::Analysis` is a `Luau::Frontend` over a C++ shim, fed by a
host `SourceProvider`: `read_source(name)` returns a module's text and whether it is a script,
`resolve_module(requirer, path)` says what a `require` inside it refers to, `module_config`
gives the `.luaurc` fields the checker reads (`Mode::NoCheck`, `Nonstrict` or `Strict`, a lint
mask, lint errors, type errors), and `human_name` a display name. Nothing here touches a VM.

`Analysis::new(provider, AnalysisOptions)` builds the frontend. The options pick the solver
(`Solver::New` by default), whether to register Luau's builtin definitions, whether to retain
full type graphs, and a list of `Definitions` files (`.d.luau` sources in Luau's `declare
extern type` grammar) loaded after the builtins. A definitions file that fails to parse or type
check fails `new` with its diagnostics in the error text; `Analysis::new_reporting` returns them
as values instead. `RuntimePlan::type_definitions()` renders one for a plan, and
`analysis::PlanSources::new(plan, inner)` is the provider that serves each of the plan's module
paths (`require("@dream/quat")`) as a strict stub returning the module's declared type, with
everything else from an inner provider.

`check(module, lint)` type checks a module and what it requires and returns a `CheckReport` of
`Diagnostic`s, each with a kind (`TypeError`, `LintWarning`, `LintError`, `ParseError`,
`Internal`), Luau's code, the lint's name, the module, the text and a `Span` (0-based lines and
columns, end exclusive); `errors()` is what would fail a build and `is_clean()` says there is
nothing. `mark_dirty(module)` and `clear()` forget checked state, `autocomplete(module, line,
column)` returns `Completions` with their `CompletionContext` and each entry's kind, deprecation
and type text, and `analysis::parse(source, with_json)` parses one chunk standalone, optionally
encoding the AST as Luau's JSON.

## The raw FFI

`l3i::ffi` re-exports the hand-declared C API for hand-written native functions. Every function
in `lua.h`, `lualib.h`, `luacode.h`, `luacodegen.h`, `luajitinliner.h` and `Require.h` is
declared there; the only exception is the varargs `lua_pushvfstring`. Everything in it is
`unsafe`, and `native::enter(state, |stack| ..)` runs a closure as the whole implementation of a
`lua_CFunction` with a `Stack` over the calling thread, so a raw function can use the safe
layers.

```rust
use std::ffi::c_int;

use l3i::ffi;
use l3i::stack::Scope;

unsafe extern "C-unwind" fn twice(state: *mut ffi::lua_State) -> c_int {
    unsafe {
        l3i::native::enter(state, |stack| {
            let value = stack.at(1).read::<f64>()?;
            stack.push(&(value * 2.0))?;
            Ok(1)
        })
    }
}
```

`l3i::TAG_LIMIT` and `l3i::LUAU_VERSION` (`"0.740"`) describe the linked VM.
