+++
title = "Stack and values"
description = "Stack, Frame, ValueView, TableView, Type and Scope; the pinned Value, Table and Function; calling Lua functions; every conversion type; strict option tables."
weight = 220

[extra]
kind = "api"
+++

Modules `l3i::stack`, `l3i::value`, `l3i::call`, `l3i::convert` and `l3i::options`.
[Stack and values](@/docs/stack-and-values.md) explains the three tiers in prose; this page
lists every item.

The borrowed tier pins nothing: a `ValueView` names one stack slot and is valid exactly as long
as the raw Lua index it names, which Rust proves through the frame model. `Stack` has no `pop`;
only frames pop. Temporaries are pushed through a `Frame`, and the view returned borrows that
frame. `with_frame` closures are higher-ranked, so a view cannot be returned out of the frame
that made it. A stack or a frame has at most one open child frame: opening a sibling panics at
the opening line, pushing into a scope while its child is open is refused, and popping or
releasing needs `&mut`, so no live view can survive a pop.

## Scope

{{ api_signature(value="trait Scope: sealed::Sealed") }}

Somewhere values can be pushed: the call-level `Stack`, a `Frame`, or a bound function's `Call`.
Helpers that create values are generic over it so a result can be pushed at the call level
while a temporary goes through a frame. Sealed; implemented by `Stack`, `Frame` and `Call`.

| Method | Meaning |
|---|---|
| `fn at(&self, index: c_int) -> ValueView<'_>` | A view of slot `index` bound to this scope |
| `fn frame(&self) -> Frame<'_>` | Opens a temporary frame on this scope |
| `fn top_value(&self) -> ValueView<'_>` | The most recently pushed value (`at(-1)`) |
| `fn with_frame<R>(&self, body: impl FnOnce(&Frame<'_>) -> Result<R>) -> Result<R>` | Runs `body` inside a temporary frame; the result cannot borrow the frame |
| `fn push<T: Push + ?Sized>(&self, value: &T) -> Result<ValueView<'_>>` | Pushes a Rust value through its `Push` conversion |

## Stack

{{ api_signature(value="struct Stack<'vm>") }}

The exclusive handle to one Lua thread's stack. `'vm` is the lifetime for which the caller
guarantees the `lua_State` stays alive: inside a native callback the callback's frame, host
side the borrow of the owning `Runtime`. Not `Clone`. Two kinds exist and only one of each may
be alive per thread: the root stack leased from `Runtime::stack` or `Thread::with_stack`, and a
native-call stack created for a bound function while Lua is calling into Rust.

| Method | Meaning |
|---|---|
| `fn is_host_level(&self) -> bool` | True outside any Lua call; operations that can raise then run under `lua_pcall` |
| `fn top(&self) -> c_int` | `lua_gettop` |
| `fn at(&self, index: c_int) -> ValueView<'_>` | A view of slot `index`. Negative indexes resolve against the current top; `0`, out-of-range and above-top indexes are views of `Type::None` |
| `fn frame(&self) -> Frame<'_>` | Opens a temporary frame. Panics if a frame opened from this stack is still alive |
| `fn with_frame<R>(&self, body: impl FnOnce(&Frame<'_>) -> Result<R>) -> Result<R>` | Runs `body` inside a temporary frame |
| `fn check(&self, extra: usize) -> Result<()>` | Ensures `extra` free slots (`lua_checkstack`); `Lua error: stack overflow` otherwise |
| `fn push_nil(&self) -> ValueView<'_>` | Call-level pushes: results of a native function, or host setup before any frame. Nothing pops these within the current scope, so their views borrow the stack |
| `fn push_boolean(&self, value: bool) -> ValueView<'_>` | |
| `fn push_number(&self, value: f64) -> ValueView<'_>` | |
| `fn push_string(&self, value: &str) -> ValueView<'_>` | |
| `fn push_value(&self, value: ValueView<'_>) -> Result<ValueView<'_>>` | Pushes a copy of `value`, which must belong to this VM (`lua_xpush` from another thread of it) |
| `unsafe fn push_c_function(&self, function: lua_CFunction, debug_name: *const c_char) -> ValueView<'_>` | Pushes a C function; `debug_name` is null or a pointer valid until the VM closes |

Pushing through a `Stack` while a frame opened from it is alive is a debug assertion.

## Frame

{{ api_signature(value="struct Frame<'p>") }}

A temporary region of the stack. Everything pushed through it is popped when it drops (only
downwards: an earlier-dropped sibling may have lowered the top). Views produced by a frame
borrow the frame; popping needs `&mut`. Nested frames are opened from a frame with `frame`,
one at a time.

| Method | Meaning |
|---|---|
| `fn is_host_level(&self) -> bool` | As `Stack::is_host_level` |
| `fn floor(&self) -> c_int` | The height recorded when the frame opened; slots above it belong to the frame |
| `fn top(&self) -> c_int` | `lua_gettop` |
| `fn len(&self) -> c_int` | Values the frame holds above its floor |
| `fn is_empty(&self) -> bool` | |
| `fn at(&self, index: c_int) -> ValueView<'_>` | A view of any existing slot, including ones below the floor, borrowing the frame |
| `fn top_value(&self) -> ValueView<'_>` | The most recently pushed value |
| `fn frame(&self) -> Frame<'_>` | A nested frame. Panics if one opened from this frame is still alive: `a frame is already open on this scope; open nested frames from the innermost frame (inside a bound function, read the arguments before opening a frame, or open the frame from the call itself)` |
| `fn with_frame<R>(&self, body: impl FnOnce(&Frame<'_>) -> Result<R>) -> Result<R>` | |
| `fn check(&self, extra: usize) -> Result<()>` | Reserves stack for a bulk push into this frame without a per-element frame |
| `fn pop(&mut self, count: c_int)` | Pops `count` values, never below the floor; negative counts pop nothing |
| `fn release(&mut self)` | Disarms the frame: whatever it holds stays on the stack when it drops. How a value built inside a frame is handed to the enclosing scope |
| `fn preserve_top_and_release(&mut self)` | Keeps the current top value (normally an error object), discards every other value the frame holds, and disarms the frame |
| `fn push_nil(&self) -> ValueView<'_>` | |
| `fn push_boolean(&self, value: bool) -> ValueView<'_>` | |
| `fn push_number(&self, value: f64) -> ValueView<'_>` | |
| `fn push_string(&self, value: &str) -> ValueView<'_>` | |
| `fn push_table(&self, array_capacity: usize, hash_capacity: usize) -> Result<TableView<'_>>` | `lua_createtable` |
| `fn push_value(&self, value: ValueView<'_>) -> Result<ValueView<'_>>` | A copy of `value`, from any scope of this VM |
| `unsafe fn push_c_function(&self, function: lua_CFunction, debug_name: *const c_char) -> ValueView<'_>` | As on `Stack` |
| `fn set_global(&self, name: &str) -> Result<()>` | Pops the frame's top value into the global `name`. Raises (or fails at host level) when the globals table is read-only; a logic error when the frame is empty |
| `fn concat(&self, count: c_int) -> Result<ValueView<'_>>` | Concatenates the top `count` values into one string honouring `__concat`; the result replaces them |
| `fn equal(&self, a: ValueView<'_>, b: ValueView<'_>) -> Result<bool>` | `a == b` honouring `__eq` |
| `fn less_than(&self, a: ValueView<'_>, b: ValueView<'_>) -> Result<bool>` | `a < b` honouring `__lt` |

```rust
use l3i::Runtime;
use l3i::value::Value;

fn main() -> l3i::Result<()> {
    let runtime = Runtime::new()?;
    let stack = runtime.stack();
    let table = stack.with_frame(|frame| {
        let table = frame.push_table(0, 1)?;
        frame.push_number(42.0);
        table.raw_set(frame, "answer")?;
        Value::store(table.value())
    })?;
    assert_eq!(stack.top(), 0, "the frame restored the height");
    assert!(table.is_table());
    Ok(())
}
```

## Type

{{ api_signature(value="enum Type { None, Nil, Boolean, LightUserdata, Number, Integer, Vector, String, Table, Function, Userdata, Thread, Buffer }") }}

Luau's value types, including the Luau-only ones, with the `LUA_T*` discriminants. `None` is
`LUA_TNONE`, a slot that does not exist. `Clone`, `Copy`, `Debug`, `Eq`.

{{ api_signature(value="fn name(self) -> &'static str") }}

Luau's own name, as `lua_typename` reports it: `no value`, `nil`, `boolean`, `userdata` (for
both userdata kinds), `number`, `integer`, `vector`, `string`, `table`, `function`, `thread`,
`buffer`.

## ValueView

{{ api_signature(value="struct ValueView<'v>") }}

A borrowed view of one stack slot. `'v` is the scope that keeps the slot alive: the frame that
pushed it, or the stack for slots below every frame. Copying a view copies the index, not the
value. A view left behind by an out-of-order frame drop reads as `Type::None` instead of
aliasing whatever Luau puts there next. `Clone`, `Copy`, `Debug`.

| Method | Meaning |
|---|---|
| `fn index(&self) -> c_int` | The absolute index, a pseudo-index, or 0 for no value |
| `fn type_of(&self) -> Type` | `None` when the slot no longer exists |
| `fn is_nil(&self) -> bool`, `is_boolean`, `is_integer`, `is_string`, `is_table`, `is_function`, `is_light_userdata`, `is_userdata`, `is_thread`, `is_buffer`, `is_vector` | One type test each |
| `fn is_number(&self) -> bool` | True for both Luau numbers and Luau 64-bit integers |
| `fn as_table(&self) -> Result<TableView<'v>>` | The slot as a table, or its type error |
| `fn as_function(&self) -> Result<FunctionView<'v>>` | The slot as a function, or its type error |
| `fn read<T: FromView<'v>>(self) -> Result<T>` | One checked conversion of this slot |
| `fn is<T: FromView<'v>>(self) -> bool` | True when `read` would succeed; for overload probing only |
| `fn type_error(&self, expected: Type) -> Error` | `Lua stack index N: expected <type>, got <type>`, for extensions that convert by hand |
| `fn type_error_expecting(&self, expected: &str) -> Error` | The same with the expectation in words, for a union or a domain type (`"an entry handle or an archive path"`) |
| `fn field_type_error(&self, context: &str, expected: &str) -> Error` | `<context>: expected <expected>, got <found>`, without the slot index: `context` is a field path (`importMaps.dataDirs[2]`) or an API name (`archive:extract`) |
| `fn field_type_error_of(&self, context: &str, expected: Type) -> Error` | `field_type_error` with Luau's own name for the expected type |

## TableView

{{ api_signature(value="struct TableView<'v>") }}

A borrowed view of a table on the stack. Lookups push their result and return a view bound to
the frame you pass; the frame decides when it is popped. Stores consume the value on top of that
frame. `Clone`, `Copy`, `Debug`.

| Method | Meaning |
|---|---|
| `fn value(&self) -> ValueView<'v>` | The table's own view |
| `fn index(&self) -> c_int` | |
| `fn get<'f>(&self, frame: &'f Frame<'_>, key: &str) -> Result<ValueView<'f>>` | Honours `__index`. A raising `__index` unwinds to Luau inside a call and becomes `Err` at host level |
| `fn raw_get<'f>(&self, frame: &'f Frame<'_>, key: &str) -> Result<ValueView<'f>>` | Bypasses `__index`; never raises |
| `fn get_index<'f>(&self, frame: &'f Frame<'_>, key: i64) -> Result<ValueView<'f>>` | `t[key]` for an integer key (pushed as a number), honouring `__index` |
| `fn raw_get_index<'f>(&self, frame: &'f Frame<'_>, key: i64) -> Result<ValueView<'f>>` | `rawget(t, key)` for an integer key |
| `fn set(&self, frame: &Frame<'_>, key: &str) -> Result<()>` | Honours `__newindex`; consumes the value on top of `frame` |
| `fn raw_set(&self, frame: &Frame<'_>, key: &str) -> Result<()>` | Bypasses `__newindex`; raises (or fails at host level) when the table is read-only |
| `fn raw_set_index(&self, frame: &Frame<'_>, key: i64) -> Result<()>` | `rawset(t, key, top)` for an integer key |
| `fn raw_set_number_key(&self, frame: &Frame<'_>, key: f64) -> Result<()>` | `rawset` for a number key, any double |
| `fn set_value<T: Push + ?Sized>(&self, frame: &Frame<'_>, key: &str, value: &T) -> Result<()>` | Pushes `value` and stores it under `key`, honouring `__newindex` |
| `fn raw_set_value<T: Push + ?Sized>(&self, frame: &Frame<'_>, key: &str, value: &T) -> Result<()>` | Pushes `value` and raw-stores it |
| `fn with_field<R>(&self, frame: &Frame<'_>, key: &str, body: impl FnOnce(ValueView<'_>) -> Result<R>) -> Result<R>` | Looks `key` up in a nested frame and hands the borrowed value to `body`; the frame is restored afterwards |
| `fn get_as<T: for<'a> FromView<'a>>(&self, frame: &Frame<'_>, key: &str) -> Result<T>` | `t[key]` converted to `T` inside a nested frame |
| `fn get_optional<T: for<'a> FromView<'a>>(&self, frame: &Frame<'_>, key: &str, context: &str) -> Result<Option<T>>` | Strict optional read: nil is `None`, a wrong type is a logic error naming `context`, the key and the offending value (`<context> "key" has an invalid value "..."`) |
| `fn raw_get_optional<T: for<'a> FromView<'a>>(&self, frame: &Frame<'_>, key: &str, context: &str) -> Result<Option<T>>` | `get_optional` bypassing `__index` |
| `fn for_each(&self, frame: &Frame<'_>, visitor: impl FnMut(&Frame<'_>, ValueView<'_>, ValueView<'_>) -> Result<()>) -> Result<()>` | Visits every entry through `lua_rawiter` with one frame for the whole walk: the visitor receives the per-entry frame and the key and value views, valid only for that call, and must not add or remove entries |
| `fn for_each_array(&self, frame: &Frame<'_>, visitor: impl FnMut(&Frame<'_>, i64, ValueView<'_>) -> Result<()>) -> Result<()>` | Visits `t[1]` to `t[rawlen]` in order, one element on the stack at a time and no frame per element, so a walk over twenty thousand entries never nears Luau's stack limit |
| `fn find_key(&self, frame: &Frame<'_>, predicate: impl FnMut(ValueView<'_>) -> Result<bool>) -> Result<bool>` | True when `predicate` accepts some key; stops at the first match |
| `fn len(&self, frame: &Frame<'_>) -> Result<usize>` | Length honouring `__len` |
| `fn raw_len(&self) -> usize` | Raw border length |
| `fn is_read_only(&self) -> bool` | |
| `fn set_read_only(&self, read_only: bool) -> Result<()>` | `lua_setreadonly` |
| `fn clone_table<'f>(&self, frame: &'f Frame<'_>) -> Result<TableView<'f>>` | A shallow copy (`lua_clonetable`): same array and hash parts, same metatable, not read-only |
| `fn clear(&self, frame: &Frame<'_>) -> Result<()>` | Removes every key (`lua_cleartable`), keeping the capacity; raises for a read-only table |

A view whose slot no longer exists, or a frame on another thread, is `Error::Logic`.

## Value

Module `l3i::value`: the owned tier. A `Value` owns a registry pin (`lua_ref`) but never the VM.
Each carries a weak handle to its VM's lifetime token, so a value that outlives its `Runtime`
becomes invalid instead of touching a closed VM.

{{ api_signature(value="struct Value") }}

A registry-pinned Lua value tied to one VM. `Clone` creates an independent pin; `Default` is
`invalid()`; equality is raw equality within one VM (two invalid values are equal, values from
different VMs never are); `Debug` prints the type and reference id or `Value(invalid)`.
Dropping releases the pin, a no-op once the VM has closed.

| Method | Meaning |
|---|---|
| `const fn invalid() -> Value` | No VM, no pin |
| `fn store(view: ValueView<'_>) -> Result<Value>` | Pins the value `view` names. A nonexistent slot, the registry pseudo-index, or a VM no `Runtime` owns is a logic error |
| `fn new_table(scope: &impl Scope, array_capacity: usize, hash_capacity: usize) -> Result<Value>` | A new empty table, pinned, the stack left as it was |
| `unsafe fn new_function(scope: &impl Scope, function: lua_CFunction, debug_name: *const c_char) -> Result<Value>` | A pinned C function; `debug_name` retained for the VM's life or null |
| `fn get_global(scope: &impl Scope, name: &str) -> Result<Value>` | The global `name`, pinned, read raw (nil pins as a valid nil) |
| `fn is_valid(&self) -> bool` | True while the value holds a pin on an open VM |
| `fn reset(&mut self)` | Releases the pin, leaving the value invalid. Safe to call twice |
| `fn push_to<'f>(&self, frame: &'f Frame<'_>) -> Result<ValueView<'f>>` | Pushes onto `frame`, any thread of the same VM |
| `fn push_to_scope<'s, S: Scope>(&self, scope: &'s S) -> Result<ValueView<'s>>` | Pushes onto any scope of the same VM (a native call's result slot, or a frame) |
| `fn with_value<R>(&self, scope: &impl Scope, body: impl FnOnce(&Frame<'_>, ValueView<'_>) -> Result<R>) -> Result<R>` | Pushes in a temporary frame on `scope` and hands that frame and the view to `body` |
| `fn type_of(&self) -> Type` | One balanced push and pop on the owning main thread; `None` when invalid |
| `fn is_nil(&self) -> bool`, `is_table`, `is_function`, `is_userdata` | |

An invalid value, or a value pushed to another VM, is `Error::Logic` (`Lua reference belongs
to a different VM`).

## Table and Function

{{ api_signature(value="struct Table(Value)") }}

A pinned value known to be a table. `Clone`, `Debug`, `PartialEq`, `Default`; also `FromView`
(pins the slot; any other type is a type error), `Push`, `CallResults`, and a bound-function
parameter and return type.

| Method | Meaning |
|---|---|
| `fn new(scope: &impl Scope, array_capacity: usize, hash_capacity: usize) -> Result<Table>` | |
| `fn from_value(value: Value) -> Result<Table>` | `expected table, got <type>` otherwise |
| `fn value(&self) -> &Value`, `fn into_value(self) -> Value` | |
| `fn push_to<'f>(&self, frame: &'f Frame<'_>) -> Result<TableView<'f>>` | |
| `fn get<T: for<'a> FromView<'a>>(&self, scope: &impl Scope, key: &str) -> Result<T>` | Cold tier: `t[key]` converted, the stack left as it was |
| `fn set<T: Push + ?Sized>(&self, scope: &impl Scope, key: &str, value: &T) -> Result<()>` | Cold tier: `t[key] = value`, honouring `__newindex` |

{{ api_signature(value="struct Function(Value)") }}

A pinned value known to be a function. `Clone`, `Debug`, `PartialEq`, `Default`; also
`FromView`, `Push`, `CallResults`, and a parameter and return type.

| Method | Meaning |
|---|---|
| `fn from_value(value: Value) -> Result<Function>` | `expected function, got <type>` otherwise |
| `fn value(&self) -> &Value`, `fn into_value(self) -> Value` | |
| `fn push_to<'f>(&self, frame: &'f Frame<'_>) -> Result<ValueView<'f>>` | |
| `fn invoke<R: CallResults, A: PushArgs>(&self, scope: &impl Scope, args: A) -> Result<R>` | Pushes the function and `args` onto a nested frame of `scope`, calls it under `lua_pcall`, reads `R::COUNT` results. Extra results are dropped, missing ones read as nil |
| `fn invoke_with_values<R: CallResults>(&self, scope: &impl Scope, args: &[Value]) -> Result<R>` | `invoke` with a runtime-sized list of pinned arguments |
| `fn invoke_with<R, A: PushArgs>(&self, scope: &impl Scope, args: A, visitor: impl FnOnce(&Frame<'_>, ValueView<'_>) -> Result<R>) -> Result<R>` | One result, handed borrowed to `visitor` while the call frame is alive |
| `fn invoke_multi<A: PushArgs>(&self, scope: &impl Scope, args: A) -> Result<Vec<Value>>` | `LUA_MULTRET`, every result pinned left to right. At most 256 results (`Lua error: too many return values`) |

## Calling

Module `l3i::call`. Every call goes through `lua_pcall`, so a callee error never unwinds into
Rust: it comes back as `Error::Runtime`, with the stack restored by the frame. The invocation
API is non-yielding: a callee that yields through it is `Lua function yielded through a
non-yielding invocation`. Two wordings are kept from the C++ binder: a borrowed-view call
reports `Lua error at stack index N: <message>`, a pinned-function call `Lua error: <message>`.

{{ api_signature(value="trait PushArgs { const COUNT: c_int; fn push_all<S: Scope>(&self, scope: &S) -> Result<()>; }") }}

Arguments for a call: `()` and tuples of one to eight `Push` values.

{{ api_signature(value="trait CallResults: Sized { const COUNT: c_int; fn read(frame: &Frame<'_>, first: c_int) -> Result<Self>; }") }}

Results of a call, read from the frame slots `first..first + COUNT`; Lua pads with nil or
truncates. Implemented for `()`, `bool`, every Rust integer, `f32`, `f64`, `String`, `Vec<u8>`,
`Integer`, `Vector3`, `Value`, `Table`, `Function`, `Option<T>` where `Option<T>: FromView`, and
tuples of two to six results. Borrowing types (`&str`, views) are excluded on purpose: the
result slots are popped when the call frame closes.

{{ api_signature(value="struct FunctionView<'v>") }}

A borrowed view of a function slot, from `ValueView::as_function`. `Clone`, `Copy`, `Debug`.

| Method | Meaning |
|---|---|
| `fn value(&self) -> ValueView<'v>`, `fn index(&self) -> c_int` | |
| `fn invoke<R: CallResults, A: PushArgs>(&self, frame: &Frame<'_>, args: A) -> Result<R>` | Calls with `args` inside a nested frame on `frame`, reading `R::COUNT` results. A frame on another thread is a logic error |

```rust
use l3i::Runtime;
use l3i::value::Value;

fn main() -> l3i::Result<()> {
    let runtime = Runtime::new()?;
    let multi = runtime.load_function("return function() return 1, 'two' end")?;
    let stack = runtime.stack();
    assert_eq!(multi.invoke::<i32, _>(&stack, ())?, 1);
    assert_eq!(multi.invoke::<(i32, String, Option<i32>), _>(&stack, ())?, (1, "two".to_owned(), None));
    let results: Vec<Value> = multi.invoke_multi(&stack, ())?;
    assert_eq!(results.len(), 2);
    let boom = runtime.load_function("return function() error('boom') end")?;
    let error = boom.invoke::<(), _>(&stack, ()).unwrap_err().to_string();
    assert!(error.starts_with("Lua error: ") && error.ends_with("boom"));
    assert_eq!(stack.top(), 0);
    Ok(())
}
```

## Conversion

Module `l3i::convert`. Conversion is explicit supported-type dispatch: a type either implements
`FromView` and `Push` with one checked conversion, or it does not convert. No probing, no
string-to-number coercion, no silent fallback.

{{ api_signature(value="trait FromView<'v>: Sized { const EXPECTED: &'static str; fn from_view(view: ValueView<'v>) -> Result<Self>; fn from_raw_arg(raw: &RawValue, view: impl FnOnce() -> ValueView<'v>) -> Result<Self>; fn matches(view: ValueView<'v>) -> bool; }") }}

A Rust type readable from one stack slot. `EXPECTED` is the name diagnostics use. `from_view`
converts or returns the type or range error. `from_raw_arg` is `from_view` for an argument slot
the binder reads directly through Luau's 16-byte value layout; scalar types override it, the
default converts through the API. `matches` is used only for overload resolution and optional
argument disambiguation.

{{ api_signature(value="trait Push { fn push_into<'s, S: Scope>(&self, scope: &'s S) -> Result<ValueView<'s>>; fn push_only<S: Scope>(&self, scope: &S) -> Result<()>; }") }}

A Rust type pushable as one Lua value. `push_only` pushes without producing a view; the hot
return path uses it so a scalar result costs one `lua_push*` and nothing else. `&T` pushes as
`T`.

| Rust type | Reads from | Pushes as | Rule |
|---|---|---|---|
| `bool` | boolean | boolean | Exact |
| `i8` .. `i64`, `isize`, `u8` .. `u64`, `usize` | number or integer | number | A number rounds half away from zero and must land inside `[-2^digits, 2^digits)`; an integer is range-checked; non-finite never converts. Pushing a value an f64 cannot hold exactly is an error, never a rounded result |
| `f64` | number or integer | number | An integer converts as `i64 as f64` |
| `f32` | number or integer | number | Finite values outside the f32 range are rejected; NaN and infinities pass through |
| `Integer` | integer | integer | `EXPECTED` is `integer` |
| `Exact<T>` | integer or integral number | (input only) | Never rounds |
| `Bits64` | integer | integer | All 64 bits, no numeric meaning |
| `&str`, `String` | string | string | Borrowed or copied; invalid UTF-8 is `Lua string is not valid UTF-8` |
| `&[u8]`, `Vec<u8>`, `[u8]` | string | string | Any bytes |
| `()` | | nil | The counterpart of `None` |
| `Option<T>` | nil or `T` | nil or `T` | Nil is `None`; a nonexistent slot is not nil here (absence is the binder's concern) |
| `Vector3` | vector | vector | One tag check and a 12-byte copy |
| `BufferView` | buffer | | Borrowed |
| `BytesView` | string or buffer | | Borrowed, never normalised |
| `ValueView` | any present value | copies the slot | |
| `Value`, `Table`, `Function` | any, table, function | the pinned value | `Value` pins the slot |
| `LightUserdata` | light userdata | light userdata | See [Direct access and the VM](@/docs/api/direct.md) |
| `Packed<T>` | integer of kind `T::KIND` | integer | See [Extensions and primitives](@/docs/api/extension.md) |

Out-of-range wording: `Lua stack index N: integer <value> is out of range for i32`, `number
<value> is out of range for u8`, `number <value> is not an exact usize`.

## Integer, Exact and Bits64

{{ api_signature(value="struct Integer(pub i64)") }}

An explicit Luau 64-bit integer. Plain Rust integers push as Lua numbers; wrap them in
`Integer` to push the `integer` runtime type. Reads accept only an integer slot. `Clone`,
`Copy`, `Debug`, `Default`, `Eq`, `Hash`.

{{ api_signature(value="struct Exact<T>(pub T)") }}

An integer parameter that never rounds: a Luau integer in range, or a number with a zero
fractional part in range (`640.4` is refused where a plain `i32` would read `640`). For
indices, offsets, counts, sizes and ids. Input only; it has no push form. Implemented for every
Rust integer type. `Clone`, `Copy`, `Debug`, `Default`, `Eq`, `Hash`.

{{ api_signature(value="struct Bits64(pub u64)") }}

A 64-bit pattern carried in a Luau integer: opaque ids and hashes, all 64 bits, no numeric
meaning. Numerically it may look negative to a script. Reads accept an integer only; a number
never becomes one. `Clone`, `Copy`, `Debug`, `Default`, `Eq`, `Hash`.

## Vector3

{{ api_signature(value="struct Vector3 { pub x: f32, pub y: f32, pub z: f32 }") }}

A native Luau `vector`; the VM is built 3-wide. `repr(C)`, `Clone`, `Copy`, `Debug`, `Default`,
`PartialEq`, `From<[f32; 3]>` and into `[f32; 3]`.

| Method | |
|---|---|
| `const fn new(x: f32, y: f32, z: f32) -> Vector3` | |
| `const fn to_array(self) -> [f32; 3]` | |

## BufferView and BytesView

{{ api_signature(value="struct BufferView<'v>") }}

A borrowed view of a Luau `buffer`: raw bytes owned by Luau, mutable from scripts and from the
host. Safe access goes through bounds-checked copies, never a Rust slice: a script can pass one
buffer to two parameters, so two views of the same storage are ordinary. Bounds failures use the
buffer library's own wording, `buffer access out of bounds`. `Clone`, `Copy`, `Debug`.

| Method | Meaning |
|---|---|
| `fn len(&self) -> usize`, `fn is_empty(&self) -> bool` | |
| `fn read(&self, offset: usize, dst: &mut [u8]) -> Result<()>` | Copies `dst.len()` bytes from `offset` |
| `fn write(&self, offset: usize, src: &[u8]) -> Result<()>` | Writes `src` at `offset` |
| `fn fill(&self, offset: usize, len: usize, value: u8) -> Result<()>` | |
| `fn to_vec(&self) -> Vec<u8>` | A copy of the whole buffer |
| `fn range(&self, offset: usize, len: usize) -> Result<BufferView<'_>>` | A bounds-checked sub-range over the same storage |
| `fn read_u8`, `write_u8`, `read_i32`, `write_i32`, `read_f32`, `write_f32`, `read_f64`, `write_f64` | Host-order scalar helpers at an offset |
| `fn read_f32x3(&self, offset: usize) -> Result<Vector3>`, `fn write_f32x3(&self, offset: usize, vector: Vector3) -> Result<()>` | Three consecutive f32s, the semantics of `vector:writef32x3` |
| `fn read_packed<T: BufferPack>(&self, offset: usize) -> Result<T>`, `fn write_packed<T: BufferPack>(&self, offset: usize, value: &T) -> Result<()>` | A fixed layout in one bounds check; see `packed` |
| `unsafe fn bytes_unchecked(&self) -> &[u8]` | The bytes without copying. While the slice lives nothing may write the buffer: no call back into Lua, no write through any view of the same storage |
| `unsafe fn bytes_mut_unchecked(&mut self) -> &mut [u8]` | As above, and nothing may read the buffer through another view either |

{{ api_signature(value="enum BytesView<'v> { String(&'v [u8]), Buffer(BufferView<'v>) }") }}

Immutable bytes from either a Lua string or a Luau buffer, without normalising: an API that
accepts "some bytes" reads through this and never copies to decide. `EXPECTED` is `string or
buffer`. `Clone`, `Copy`, `Debug`.

| Method | Meaning |
|---|---|
| `fn len(&self) -> usize`, `fn is_empty(&self) -> bool` | |
| `fn read(&self, offset: usize, dst: &mut [u8]) -> Result<()>` | |
| `fn to_vec(&self) -> Vec<u8>` | |
| `unsafe fn bytes_unchecked(&self) -> &[u8]` | A string's bytes carry no condition; a buffer's follow `BufferView::bytes_unchecked` |

{{ api_signature(value="fn new_buffer<'s, S: Scope>(scope: &'s S, len: usize) -> Result<BufferView<'s>>") }}

Creates a zero-filled buffer of `len` bytes on `scope`.

```rust
use l3i::Runtime;
use l3i::convert::{BufferView, BytesView};

fn main() -> l3i::Result<()> {
    let runtime = Runtime::new()?;
    let total = runtime.bind_function("dreamweave.tests.total", |bytes: BytesView| {
        bytes.to_vec().iter().map(|&x| i64::from(x)).sum::<i64>()
    })?;
    runtime.set_global("total", &total)?;
    runtime.exec("assert(total('\\1\\2\\3') == 6) local b = buffer.create(4) buffer.writeu8(b, 3, 5) assert(total(b) == 5)")?;
    let fill = runtime.bind_function("dreamweave.tests.fill", |buffer: BufferView, value: i64| {
        buffer.fill(0, buffer.len(), value as u8)?;
        let range = buffer.range(1, 2)?;
        Ok::<i64, l3i::Error>(i64::from(range.read_u8(0)?) + i64::from(range.read_u8(1)?))
    })?;
    runtime.set_global("fill", &fill)?;
    runtime.exec("local b = buffer.create(4) assert(fill(b, 7) == 14)")?;
    Ok(())
}
```

## RawValue

{{ api_signature(value="struct RawValue") }}

One Luau stack slot as laid out by this build: an 8-byte value union, a 32-bit `extra` word and
the 32-bit type tag. The binder reads argument slots through this mirror instead of Luau's
index resolution; `csrc/extra.cpp` pins every offset at compile time and every runtime proves
the mirror against the API once at creation. `repr(C)`, 16 bytes.

| Method | Meaning |
|---|---|
| `fn tag(&self) -> i32` | The `LUA_T*` tag |
| `fn number(&self) -> f64`, `fn integer(&self) -> i64`, `fn boolean(&self) -> bool`, `fn vector(&self) -> [f32; 3]` | The payload of a slot of that tag |
| `unsafe fn userdata(&self) -> (u8, *mut c_void)` | The tag byte and payload address of a full userdata slot |

## Options

Module `l3i::options`: strict camelCase option tables. Every key must be known, required keys
are reported precisely, errors carry field paths, and nothing defaults permissively: a
misspelt option is an error, not a silently ignored default.

{{ api_signature(value="struct Options<'a, 'f>") }}

One option table being read. `Options::read` opens a frame, checks that the value is a table
with string keys, hands the reader to `body`, and then fails if any key was never asked for.

| Method | Meaning |
|---|---|
| `fn read<R>(scope: &impl Scope, view: ValueView<'_>, context: &str, body: impl FnOnce(&mut Options<'_, '_>) -> Result<R>) -> Result<R>` | Reads the table at `view` with `body`, then rejects keys `body` never consumed: `<context>: unknown option 'frce'; known options are force, limit, nested, path` |
| `fn context(&self) -> &str` | The diagnostic path of this table |
| `fn frame(&self) -> &Frame<'f>` | The frame the reader works on: open nested frames from here, never from the scope `read` was given |
| `fn has(&self, key: &str) -> bool` | Whether the table has `key` (consumes nothing) |
| `fn required<T: for<'v> FromView<'v>>(&mut self, key: &str) -> Result<T>` | `<context>: missing required option 'key'` when absent |
| `fn optional<T: for<'v> FromView<'v>>(&mut self, key: &str) -> Result<Option<T>>` | Absent or nil reads as `None` |
| `fn or<T: for<'v> FromView<'v>>(&mut self, key: &str, default: T) -> Result<T>` | `optional` with a default |
| `fn with_required<R>(&mut self, key: &str, body: impl FnOnce(ValueView<'_>) -> Result<R>) -> Result<R>` | The value's slot handed to `body`: borrowed conversions (`&str`, `&[u8]`, `BufferView`) work here. The body sees the slot and nothing else, so it cannot walk a table |
| `fn with_optional<R>(&mut self, key: &str, body: impl FnOnce(ValueView<'_>) -> Result<R>) -> Result<Option<R>>` | |
| `fn required_str<R>(&mut self, key: &str, body: impl FnOnce(&str) -> Result<R>) -> Result<R>`, `optional_str` | A string borrowed for `body`, no copy |
| `fn required_bytes<R>(&mut self, key: &str, body: impl FnOnce(&[u8]) -> Result<R>) -> Result<R>`, `optional_bytes` | Any Lua string as bytes, borrowed |
| `fn required_table<R>(&mut self, key: &str, body: impl FnOnce(&Frame<'_>, TableView<'_>) -> Result<R>) -> Result<R>` | A table option walked in place: `body` gets the reader's frame and the table's view, nothing pinned. Any other type is a field error naming `table` |
| `fn optional_table<R>(&mut self, key: &str, body: impl FnOnce(&Frame<'_>, TableView<'_>) -> Result<R>) -> Result<Option<R>>` | |
| `fn required_table_expecting<R>(&mut self, key: &str, expected: &str, body: ..) -> Result<R>`, `optional_table_expecting` | The same with the expectation in the option's own words for the non-table case (`"an array of strings"`) |
| `fn nested<R>(&mut self, key: &str, body: impl FnOnce(&mut Options<'_, '_>) -> Result<R>) -> Result<Option<R>>` | A nested option table under `key`, read with `body` under the context `<context>.<key>`; absent reads as `None` |

Inside a table body an error comes back prefixed with the reader's context and key
(`archive:extract.inputs: ...`) unless it already starts with that path; a slot conversion's
`Lua stack index N:` prefix is dropped, since a field error names the field.

{{ api_signature(value="trait FromOptions: Sized { fn from_options(options: &mut Options<'_, '_>) -> Result<Self>; fn read(scope: &impl Scope, view: ValueView<'_>, context: &str) -> Result<Self>; }") }}

A type built from an option table; `read` is provided and runs `Options::read` with
`from_options`.

```rust
use l3i::options::{FromOptions, Options};
use l3i::Result;

#[derive(Debug, PartialEq)]
struct Extract {
    path: String,
    force: bool,
    limit: Option<i64>,
    nested: Option<i64>,
}

impl FromOptions for Extract {
    fn from_options(o: &mut Options<'_, '_>) -> Result<Self> {
        Ok(Extract {
            path: o.required("path")?,
            force: o.or("force", false)?,
            limit: o.optional("limit")?,
            nested: o.nested("nested", |inner| inner.required::<i64>("depth"))?,
        })
    }
}
```

With `Extract::read(call, view, "archive:extract")` a script passing `{ path = 'a', frce =
true }` gets `archive:extract: unknown option 'frce'; known options are force, limit, nested,
path`, and `{ path = 'a', nested = { } }` gets `archive:extract.nested: missing required option
'depth'`.
