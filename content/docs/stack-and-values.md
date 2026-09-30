+++
title = "Stack and values"
description = "The three tiers, exclusive stacks and nested frames, borrowed views, owned registry pins, what the typed binder accepts and returns, and how errors cross the boundary."
weight = 20

[extra]
kind = "guide"
+++

## The three tiers

| Tier | Type | Cost | Use |
|---|---|---|---|
| Borrowed | `ValueView`, `TableView`, `FunctionView` | none: a stack index bound to a `Frame` | hot paths, arguments, results |
| Owned | `Value`, `Table`, `Function` | one registry pin (`lua_ref`) | values that outlive a frame |
| Typed binder | `Runtime::bind_function`, `MetatableBuilder` | conversion from stack slots, no pins | exposing Rust to scripts |

The C++ binder asserted that a view of a stack slot is valid only as long as the slot; the Rust
port proves it with lifetimes. Nothing in the borrowed tier pins a registry reference.

## Stacks

`stack::Stack` is the exclusive handle to one Lua thread's stack. It is not `Clone`, because
two handles would let one pop what the other views, and it has no `pop` at all: only frames
pop. Two kinds exist, and only one of each may be alive per thread at a time:

- the **root** stack, leased from `Runtime::stack()` at host level. A second root while one is
  alive and not suspended inside a Lua call panics with `a root stack for this Lua thread is
  already alive; open frames from it instead`;
- a **native-call** stack, created for a bound function's frame while Luau is calling into
  Rust. Those nest only through real Lua calls.

`Stack::top()` is the height, `Stack::at(index)` a view of any existing slot, `check(n)`
reserves `n` free slots, and the `push_*` methods push at the call level: results of a native
function, or host setup before any frame. Helpers that take a `&Runtime` (`exec`, `eval`,
`load_function`, `register_module`, `sandbox`) lease the root stack themselves, so a harness
must not hold `Runtime::stack()` across them. Inside a bound function the `bind::Call` scope
stands in for the root: the host's root is suspended there, and a second root on the same
thread is refused.

## Frames

A `Frame` is a temporary region of the stack. Everything pushed through it is popped when it
drops, and its drop only ever lowers the top, never raises it.

```rust
use l3i::Runtime;
use l3i::stack::Scope;
use l3i::value::Table;

fn main() -> l3i::Result<()> {
    let runtime = Runtime::new()?;
    runtime.exec("config = { name = 'vvardenfell', size = 3 }")?;
    let config = Table::from_value(runtime.global("config")?)?;
    let stack = runtime.stack();
    let size: i32 = stack.with_frame(|frame| {
        let table = config.push_to(frame)?;
        let name = table.get(frame, "name")?;
        println!("name {}", name.read::<&str>()?);
        table.get_as::<i32>(frame, "size")
    })?;
    println!("size {size}");
    Ok(())
}
```

`with_frame` takes a closure that is higher-ranked over the frame's lifetime, so its result
cannot borrow anything the frame pushed: `name` is a `ValueView` tied to `frame` and cannot
leave the closure, while the `i32` can. `Scope` is the trait both `Stack` and `Frame`
implement (and `bind::Call`), with `at`, `frame`, `top_value`, `with_frame` and `push`.

Frame topology is strictly nested. A stack or a frame has at most one open child frame; opening
a sibling panics at the opening line with `a frame is already open on this scope; open nested
frames from the innermost frame`, because one sibling's drop could pop the other's slots. Inside
a bound function, read the arguments before opening a frame, or open the frame from the call
itself.

| Method | Effect |
|---|---|
| `frame.floor()`, `top()`, `len()` | The height when the frame opened, the current height, and the values above the floor |
| `frame.at(index)`, `top_value()` | A view of any existing slot, or of the last pushed value |
| `push_nil`, `push_boolean`, `push_number`, `push_string`, `push_table(narr, nrec)`, `push_value(view)` | Push and return a view bound to the frame |
| `frame.push(&value)` | Push through the value's `convert::Push` conversion |
| `frame.check(n)` | Reserve `n` free slots for a bulk push (`n` is a `usize` count) |
| `frame.pop(count)` | Pop `count` values, never below the floor; takes `&mut`, so no view of the frame is alive across it |
| `frame.release()` | Disarm the frame: what it holds stays on the stack for the enclosing scope |
| `frame.preserve_top_and_release()` | Keep only the top value (normally an error object) and disarm |
| `frame.set_global(name)` | Pop the top value into the global `name` |
| `frame.frame()`, `with_frame(..)` | Open a nested frame |

## Views

A `ValueView` is one stack slot, `Copy`, and bound to the scope that keeps the slot alive.
`type_of()` reports a `stack::Type` (Luau's own set, including `Integer`, `Vector` and
`Buffer`), the `is_*` predicates test one type, and `read::<T>()` runs one checked conversion.
`as_table()` and `as_function()` give a `TableView` or a `call::FunctionView`.

`TableView` lookups push their result onto the frame you pass and return a view bound to it,
exactly as the C++ `TableView::get` left the value on the stack:

| Method | Effect |
|---|---|
| `get(frame, key)`, `get_index(frame, i)` | `t[key]`, honouring `__index`; a raising `__index` unwinds to Luau inside a call and becomes `Err` at host level |
| `raw_get(frame, key)`, `raw_get_index(frame, i)` | The same, bypassing `__index`; never raises |
| `set(frame, key)`, `raw_set(frame, key)`, `raw_set_index(frame, i)` | Store the value on top of the frame under the key |
| `set_value(frame, key, &v)`, `raw_set_value(frame, key, &v)` | Push `v` and store it |
| `get_as::<T>(frame, key)`, `get_optional::<T>(frame, key, context)`, `raw_get_optional` | Converted reads inside a nested frame; `get_optional` reads nil as `None` and names `context` and the key in a type error |
| `with_field(frame, key, body)` | Look a key up in a nested frame and hand the borrowed value to `body` |
| `for_each(frame, visitor)` | Visit every entry through `lua_rawiter`; the visitor must not add or remove entries |
| `for_each_array(frame, visitor)` | Visit `t[1]..t[rawlen]` with one element on the stack at a time and no frame per element, so twenty thousand entries never near Luau's stack limit |
| `len(frame)`, `raw_len()` | Length honouring `__len`, or the raw border |
| `is_read_only()`, `set_read_only(bool)` | Luau's table freeze bit |
| `clone_table(frame)`, `clear(frame)` | `lua_clonetable` and `lua_cleartable` |

`FunctionView::invoke::<R, A>(frame, args)` calls the function in a nested frame and reads
`R::COUNT` results. It is non-yielding, and its failure wording is
`Lua error at stack index N: <message>`.

Views of a reused slot are unrepresentable rather than merely detectable: a view cannot outlive
the frame that pops it, pops take `&mut`, and out-of-order frames panic at their opening line.
As belt and braces, every view access is bounded by the live top, so a view left behind by an
out-of-order drop reads as `Type::None` instead of aliasing whatever Luau puts there next.

## Owned values

A `value::Value` owns a registry pin and never the VM. `Value::store(view)` pins what a view
names, `Value::new_table(scope, narr, nrec)` a fresh table, and `Value::get_global(scope, name)`
a global. `push_to(frame)` pushes it back onto any thread of the same VM,
`push_to_scope(scope)` onto a call's result slot, and `with_value(scope, |frame, view| ..)`
pushes it in a temporary frame and hands both to a closure so nested lookups open their frames
from it. Cloning makes an independent pin; `reset()` releases it.

Every value carries a weak handle to its VM's lifetime token. A value that outlives its
`Runtime` reports `is_valid()` as false and is inert, rather than touching a closed VM.
Equality is raw equality within one VM; values from different VMs are never equal.

`Table` and `Function` are values known to be a table or a function (`from_value` checks). The
cold tier on `Table` is `get::<T>(scope, key)` and `set(scope, key, &value)`, each leaving the
scope's stack as it was. `Function` has four ways to call:

| Call | Reads |
|---|---|
| `invoke::<R, A>(scope, args)` | `R::CallResults::COUNT` results; extras dropped, missing ones nil |
| `invoke_with_values::<R>(scope, &[Value])` | The same, with a runtime-sized argument list of pins |
| `invoke_with(scope, args, \|frame, view\| ..)` | One result, borrowed while the call frame is alive |
| `invoke_multi(scope, args)` | Every result pinned, left to right, at most 256 |

Every call runs under `lua_pcall`, so a callee error never unwinds into Rust: it is `Err`
with the wording `Lua error: <message>`, and the frame restores the stack. Arguments are a tuple
of `Push` values, results any `call::CallResults` type: scalars, `String`, `Value`, `Table`,
`Function`, `Option<T>`, tuples, or `()`. Borrowing result types are excluded on purpose,
because the result slots are popped when the call frame closes.

## Conversions

`convert::FromView` and `convert::Push` are explicit supported-type dispatch: a type either
converts with one checked conversion or it does not convert. There is no string-to-number
coercion and no silent fallback.

- Luau has two numeric runtime types. A `number` (f64) converts to a Rust integer by rounding
  half away from zero and must land inside the type's range; an `integer` (i64) converts by
  range check only. Non-finite numbers never convert to integers.
- Rust integers push as `number` and must be exactly representable as an f64. `convert::Integer`
  pushes a Luau 64-bit `integer`; `convert::Bits64` carries an opaque 64-bit pattern through one.
- `convert::Exact<T>` reads an integer that never rounds: a Luau integer or an integer-valued
  number, in range. It is input only.
- `f32` rejects finite values outside its range; NaN and infinities pass through.
- `&str` borrows the slot's bytes and fails on invalid UTF-8; `String` copies.
- `convert::Vector3` is Luau's native three-component vector (a tag check and a 12-byte copy);
  `convert::BufferView` a Luau buffer read and written through bounds-checked copies;
  `convert::BytesView` a string or a buffer without normalising.

[Primitives](@/docs/primitives.md) covers the buffer, options and packed types in depth.

## The typed binder

`Runtime::bind_function(debug_name, callable)` and every `MetatableBuilder` member turn a Rust
closure into a Lua function whose parameters are materialised one checked conversion each,
positionally, with the C++ binder's rules. The debug name must sit under one of the runtime's
roots and is interned for the VM's life.

Accepted parameter types:

| Parameter | Consumes |
|---|---|
| `bool`, `i8` to `i64`, `u8` to `u64`, `isize`, `usize`, `f32`, `f64`, `String`, `Vec<u8>` | One argument, converted |
| `Integer`, `Exact<T>`, `Bits64`, `Vector3`, `Packed<T>` | One argument, converted |
| `&str`, `BufferView`, `BytesView` | One argument, borrowed for the call |
| `ValueView` | One argument slot, no conversion |
| `Value`, `Table`, `Function` | One argument, pinned |
| `&T` where `T: Userdata` | One argument, tagged or untagged, borrowed for the call |
| `&Call` | Nothing: injected |
| `Option<T>` | Zero or one argument; absent and nil are `None`. A middle optional stays greedy and is skipped only when a following required parameter can consume the slot; nil disambiguates |
| `VarArgs<T>` | Every remaining argument, each converted; must be last |
| `ArgView` | Every remaining argument, borrowed and lazy; must be last |
| `Overload((f1, f2, ..))` | Candidates tried in order; the first probe match commits and its conversion errors propagate |

Fixed arity rejects unused arguments, counts are validated before any conversion, and the
first failing argument is reported with its position and expected type:
`dreamweave.assets.open: bad argument #1 (expected string)`, or for a method's receiver
`invalid argument #1 to 'dreamweave.Vec3.length' (dreamweave.Vec3 expected, got number)`.

Return types follow `bind::Return`:

| Return | Pushes |
|---|---|
| `()` | Nothing |
| Any `Push` scalar, string, `Vector3`, `Integer`, `Value`, `Table`, `Function` | One value |
| `Option<T>` | `None` is one nil; `Some` pushes the inner results |
| Tuples | One value per element |
| `Variadic(Vec<T>)` | Every element as its own result |
| `ResultOrError<T>` | The value, or nil plus the message |
| `NilThen<T>` | The value on success, `(nil, value)` on failure |
| `Owned<T>`, `Borrowed<T>` | A new userdata; see [Userdata](@/docs/userdata.md) |
| `StackResults` | Nothing: the callable pushed its results onto the call itself |
| `Result<T>` | `Ok` pushes the inner results; `Err` is raised as the Lua error |
| `Yield<T>`, `Break` | Yields the inner results, or requests a debugger break; see [the VM page](@/docs/vm.md) |

A `Result<T>` whose `Err` came from a conversion carries the binder's own wording; an
`Error::runtime(message)` raises `message` unchanged. The `Error::LuaErrorOnStack` variant means
the error object is already on top of the stack and the native entry re-raises it as is.
Conversions by hand raise `view.type_error(Type)`, `type_error_expecting("an entry handle or
an archive path")` for a union, or `field_type_error("dirs[2]", "a string")` for a value reached
through a path, which names the field and never a stack slot.

Bound closures are `Fn`, not `FnMut`: a binding can re-enter itself through Lua, so state lives
in `Cell` or `RefCell` captures. They are moved into a Lua-owned userdata and dropped by the
collector, so captures must be `'static` and must not touch the Lua API in `Drop`.

## Inside a bound function

`&Call` is the native call's frame: the arguments at `1..=argument_count()` and the stack above
them, where results go. `call.arg(n)` reads as none beyond the argument count whatever the
callable has pushed since, `call.stack()` is the call-level `Stack`, `call.result_count()` the
values above the arguments, and `call.forward_to(&function, first_argument)` calls another
function with the remaining arguments and forwards every result.

```rust
use l3i::bind::Call;
use l3i::stack::{Scope, ValueView};

fn main() -> l3i::Result<()> {
    let runtime = l3i::Runtime::new()?;
    let describe = runtime.bind_function("dreamweave.describe", |call: &Call, table: ValueView| -> l3i::Result<String> {
        // Read the arguments first, then open the frame from the call.
        let table = table.as_table()?;
        call.with_frame(|frame| Ok(format!("{} entries", table.len(frame)?)))
    })?;
    runtime.set_global("describe", &describe)?;
    let text: String = runtime.eval("return describe({ 1, 2, 3 })")?;
    assert_eq!(text, "3 entries");
    Ok(())
}
```

## Running source

| Call | Does |
|---|---|
| `runtime.exec(source)` | Compiles and runs on the main thread, discarding results |
| `runtime.eval::<R>(source)` | The same, reading what the chunk returns: `f64`, a tuple, `()` |
| `runtime.load_function(source)` | Runs a chunk that returns one function and pins it |
| `runtime.load(frame, chunk_name, source, &options)` | Compiles and pushes the chunk function onto a frame of any thread of the VM |
| `runtime.load_with_env(frame, chunk_name, source, &options, &env)` | The same with the chunk's globals resolving through `env` |
| `scope.load_source(chunk_name, source, &options)` | `source::LoadScope`, on any scope: compiles and loads on the scope's own thread, natively under the runtime's policy, and pins the chunk as a `Function` |
| `scope.load_bytecode(chunk_name, &bytecode)` | The same from bytecode already compiled |
| `source::compile(source, &options)` | Bytecode, with a compile error as `Error::Runtime` carrying Luau's message |

`exec`, `eval` and `load_function` use `runtime.compile_options()`, which default to
optimisation level 2 and debug level 1; a plan fills in the known libraries and userdata types.
A syntax error comes back as `Err` with Luau's message: `[string "bad.lua"]:1: Expected
identifier when parsing variable name, got '='`.
