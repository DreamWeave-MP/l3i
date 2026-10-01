+++
title = "Binding and userdata"
description = "The typed function binder and its parameter and return shapes; tagged and untagged userdata, MetatableBuilder and iterators; modules, read-only tables, require and the standard libraries."
weight = 230

[extra]
kind = "api"
+++

Modules `l3i::bind`, `l3i::userdata` (with `tagged`, `untagged`, `metatable`, `iterator`,
`dispatch`), `l3i::module`, `l3i::readonly`, `l3i::require` and `l3i::libraries`.
[Userdata](@/docs/userdata.md) and [Modules and sandboxes](@/docs/modules-and-sandboxes.md) are
the guides.

## The binder

Module `l3i::bind`. A Rust closure becomes a Lua function whose parameters are materialised one
checked conversion each, positionally, with the C++ binder's rules:

- `&Call` is injected and consumes no argument;
- `ValueView` borrows one argument slot without conversion;
- `Option<T>` maps absent and nil to `None`; a middle optional stays greedy and is skipped only
  when a following required parameter can consume the slot (nil disambiguates);
- `VarArgs<T>` and `ArgView` are terminators consuming every remaining argument, and must be
  last (a compile-time assertion);
- fixed arity rejects unused arguments, counts are validated before any conversion, and the
  first failing argument is reported with its position and expected type;
- `Overload((f1, f2, ..))` tries candidates in order and the first probe match commits.

The closure is moved into a Lua-owned userdata (upvalue 1 of the C closure) and dropped by the
collector, so captures must be `'static` and must not touch the Lua API in `Drop`. Bound
closures are `Fn`, not `FnMut`: a binding can re-enter itself through Lua, so state lives in
`Cell`/`RefCell` captures. Up to eight parameters.

{{ api_signature(value="trait Binding<Marker>: 'static") }}

A Rust callable the binder can expose; `Marker` is inferred from the signature and never
named. Implemented for every `Fn(P1, .., Pn) -> R + 'static` whose parameters are `Param` and
whose result is `Return`, and for `Overload`.

| Item | Meaning |
|---|---|
| `const PARAM_KINDS: &'static [ParamKind]` | Parameter kinds in order; builders validate member signatures with them |
| `const RECEIVER_NAME: Option<&'static str>` | The expected-type name of the first parameter, the receiver type of a method |
| `fn probe_for_overload(&self, call: &Call<'_>) -> bool` | Whether the arguments fit this signature exactly enough to commit |
| `fn invoke(&self, call: &Call<'_>, debug_name: &str) -> Result<c_int>` | Materialises, runs, pushes the results |
| `fn invoke_method(&self, call: &Call<'_>, debug_name: &str) -> Result<c_int>` | Method mode: slot 1 is the receiver, validated with its own type error and excluded from argument numbering |

{{ api_signature(value="struct Overload<Candidates>(pub Candidates)") }}

An ordered overload set of one to five callables. Candidates are tried in declaration order and
the first whose probe matches commits, so its conversion errors then propagate; no match is
`<name>: no matching overload`. Nested overload sets never match, and an overload set cannot be
bound as a method.

{{ api_signature(value="fn function<F: Binding<M>, M, R: AsRef<str>>(scope: &impl Scope, roots: &[R], debug_name: &str, callable: F) -> Result<Function>") }}

Binds `callable` as a Lua function named `debug_name`, validated against `roots` and retained
for the VM's life, and returns it pinned with the stack of `scope` left as it was.
`Runtime::bind_function` is this over the runtime's own roots.

Argument errors, word for word from `bindfunction.cpp`: `<name>: bad argument #N (expected T):
<cause>`, `<name>: bad argument #N (expected T): missing argument`, `<name>: bad argument count
(expected at least N, got M)`, `<name>: bad argument count (expected at most N, got M)`, `<name>:
bad argument count (unused arguments)`. A method called without a receiver (`obj.method()`) reports `missing argument
#1 to '<name>' (<type> expected)`.

```rust
use l3i::Runtime;
use l3i::bind::{Call, Overload, VarArgs};

fn main() -> l3i::Result<()> {
    let runtime = Runtime::new()?;
    let sum = runtime.bind_function("dreamweave.sum", |values: VarArgs<f64>| values.values.iter().sum::<f64>())?;
    runtime.set_global("sum", &sum)?;
    let greet = runtime.bind_function("dreamweave.greet", |name: &str, times: Option<i32>| {
        format!("{name}{}", "!".repeat(times.unwrap_or(1) as usize))
    })?;
    runtime.set_global("greet", &greet)?;
    let either = runtime.bind_function(
        "dreamweave.either",
        Overload((|n: f64| n * 2.0, |s: &str, _call: &Call| s.len() as f64)),
    )?;
    runtime.set_global("either", &either)?;
    runtime.exec("assert(sum(1, 2, 3) == 6) assert(greet('hi') == 'hi!') assert(greet('hi', 3) == 'hi!!!')")?;
    runtime.exec("assert(either(4) == 8 and either('abc') == 3)")?;
    Ok(())
}
```

## Call

{{ api_signature(value="struct Call<'c>") }}

The native call's frame: the arguments at `1..=argument_count()` and the stack above them, where
results are pushed. Injected into a callable as `&Call`. Implements `Scope`.

| Method | Meaning |
|---|---|
| `fn stack(&self) -> &Stack<'c>` | The call-level stack |
| `fn argument_count(&self) -> c_int` | Arguments the caller passed |
| `fn initial_top(&self) -> c_int` | The stack height when the call began |
| `fn arg(&self, index: c_int) -> ValueView<'_>` | Argument `index` (1-based). Beyond `argument_count`, or below 1, the view reads as none, whatever temporaries the callable has pushed since |
| `fn upvalue(&self, index: c_int) -> ValueView<'_>` | Upvalue `index` of the running C closure (1-based; the binder owns upvalue 1) |
| `fn result_count(&self) -> c_int` | Values above the arguments |
| `fn forward_to(&self, function: &Function, first_argument: c_int) -> Result<c_int>` | Calls `function` with the arguments from `first_argument` to the last, forwarding every result. On failure the error object stays on top and `Error::LuaErrorOnStack` is returned, so the entry re-raises it unchanged |
| `fn is_yieldable(&self) -> bool` | True when the running function may yield |

## Parameters

{{ api_signature(value="trait Param { type Item<'c>: ParamItem<'c>; }") }}

A parameter type with the call lifetime erased; `Item<'c>` is what the callable receives for a
call living `'c`. This is what lets `|view: ValueView, s: &str|` be written without lifetimes.

{{ api_signature(value="trait ParamItem<'c>: Sized") }}

One parameter of a call living `'c`: `KIND`, `EXPECTED`, `read_slot`, `read_arg`,
`read_receiver`, `matches`, `probe` and `materialize`. Hosts rarely implement it by hand: the
`impl_param_from_view!` macro (exported at the crate root) implements `Param` and `ParamItem` for
an owned type that already implements `FromView` for every lifetime.

{{ api_signature(value="enum ParamKind { Regular, Injected, Optional, VarArgs, ArgView }") }}

How a parameter consumes Lua arguments: exactly one; none (`&Call`); zero or one; every
remaining argument converted; every remaining argument borrowed. `Clone`, `Copy`, `Debug`, `Eq`.

| Parameter type | Kind | Reads |
|---|---|---|
| `bool`, the Rust integers, `f32`, `f64`, `String`, `Vec<u8>`, `Integer`, `Exact<T>`, `Bits64`, `Vector3`, `Value`, `Table`, `Function`, `Packed<T>`, `Color16` | Regular | One `FromView` conversion; scalars read the raw slot |
| `&str`, `&[u8]`, `BufferView`, `BytesView` | Regular | Borrowed for the call |
| `ValueView` | Regular | Any present value, including nil, without conversion |
| `&T where T: Userdata` | Regular | A tagged or untagged `T`; `EXPECTED` is `T::NAME`. As a method receiver the dispatcher's verification lets it skip the type check |
| `&Call` | Injected | |
| `Option<T>` | Optional | Absent and nil are `None` |
| `VarArgs<T>` | VarArgs | |
| `ArgView` | ArgView | |
| `Cursor<C>` | Regular | An iterator cursor; see the iterators below |

{{ api_signature(value="struct VarArgs<T> { pub start_slot: c_int, pub values: Vec<T> }") }}

Every remaining argument converted as `T`; must be the final parameter. `start_slot` is the
physical stack slot of the first collected argument (it includes a method receiver; binder
diagnostics number arguments separately). `Debug`.

{{ api_signature(value="struct ArgView<'c>") }}

A borrowed view over every remaining argument, heterogeneous and lazy, valid only for the call.
`Clone`, `Copy`.

| Method | Meaning |
|---|---|
| `fn len(&self) -> usize`, `fn is_empty(&self) -> bool` | |
| `fn first_slot(&self) -> c_int` | Physical stack slot of the first viewed argument |
| `fn get(&self, index: usize) -> ValueView<'c>` | 0-based within the view |
| `fn read<T: FromView<'c>>(&self, index: usize) -> Result<T>` | |
| `fn copy_to(&self, scope: &impl Scope) -> Result<()>` | Pushes copies of every viewed argument onto `scope` (same VM, any thread) |

{{ api_signature(value="trait Params") }}

A tuple of `Param` markers with its compile-time descriptors (`KINDS`, `REQUIRED`, `MAX`,
`TERMINATOR`, `RECEIVER_NAME` and the method-mode counterparts) and the ordered materialisation
the binder runs. Implemented for tuples of zero to eight parameters.

## Returns

{{ api_signature(value="trait Return { fn push_results(self, call: &Call<'_>) -> Result<c_int>; }") }}

A callable's result, pushed as zero or more Lua values.

| Return type | Pushes |
|---|---|
| `()` | Nothing |
| `bool`, the Rust integers, `f32`, `f64`, `String`, `&'static str`, `Vec<u8>`, `Integer`, `Bits64`, `Vector3`, `Value`, `Table`, `Function`, `Packed<T>`, `Color16`, `FieldValue` | One value through `Push` |
| `Option<T: Return>` | `None` is one nil result; `Some` pushes the inner results |
| `Result<T: Return>` | `Err` is raised as the Lua error; `Ok` pushes the inner results |
| `(A, B)` .. `(A, B, C, D, E, F)` of `Push` | One value each |
| `Variadic<T>` | Every element as its own result |
| `ResultOrError<T>` | `Success(T)` one value; `Failure(String)` nil plus the message |
| `NilThen<T>` | The value on success; `(nil, value)` on failure |
| `StackResults` | Nothing: the callable pushed its results itself, and `call.result_count()` is the count |
| `Owned<T: Userdata>`, `Borrowed<T: Userdata>` | A new userdata; see below |
| `IterStep<T>` | A control value and an element; see `sequence` |
| `Yield<T: Return>` | Yields the inner results to the resuming coroutine (`lua_yield`). The function must run inside a coroutine with no C-call boundary in between, or Luau raises `attempt to yield across metamethod/C-call boundary` |
| `Break` | Requests a debugger break (`lua_break`): the thread stops with `LUA_BREAK` for the host to resume |

{{ api_signature(value="struct Variadic<T>(pub Vec<T>)") }}

{{ api_signature(value="enum ResultOrError<T> { Success(T), Failure(String) }") }}

{{ api_signature(value="struct NilThen<T> { pub success: bool, pub value: T }") }}

`NilThen::success(value)` and `NilThen::failure(value)` construct it. `Debug` on all three;
`Variadic` is also `Default`.

{{ api_signature(value="struct StackResults") }}

`Debug`, `Default`, `Clone`, `Copy`. Return borrowed text from a getter with
`call.push(&text)?` and `StackResults`: a `Return` type cannot borrow from the arguments.

{{ api_signature(value="struct Yield<T: Return>(pub T)") }}

{{ api_signature(value="struct Break") }}

## Userdata

Module `l3i::userdata`. A type implements `Userdata` for identity and script name only; how it
is exposed is decided per runtime at registration. `tagged::register` takes the tag the host
assigns in that VM (payload inline, one destructor and one metatable per tag, O(1) checks;
scarce, for hot types). `untagged::register` uses exact metatable identity and the `Storage`
wrapper, consuming no tag. The same Rust type may be tag 8 in one runtime, 17 in another, and
untagged in a third.

{{ api_signature(value="unsafe trait Userdata: Sized + 'static { const NAME: &'static str; }") }}

`NAME` is the script-visible `__type` and the root of the type's debug names, such as
`dreamweave.util.Vector3`; it must be a valid debug name under the runtime's roots and unique
within a VM. Implementors promise that `Drop` never calls into the Lua API, never panics, and
does not depend on the VM being consistent: Luau runs it while sweeping. A payload whose
alignment exceeds what Luau guarantees (8 bytes, or 16 from 16 bytes up) fails to compile.

{{ api_signature(value="type RuntimeTag = u8") }}

A Luau runtime userdata tag. Valid registrations use `1..TAG_LIMIT`; tag 0 is Luau's untagged
default.

{{ api_signature(value="const fn userdata_alignment(size: usize) -> usize") }}

8, or 16 once the payload is at least 16 bytes.

{{ api_signature(value="fn receiver<'v, T: Userdata>(value: ValueView<'v>) -> Option<&'v T>") }}

{{ api_signature(value="fn check_receiver<'v, T: Userdata>(value: ValueView<'v>) -> Result<&'v T>") }}

The payload when `value` is a `T` of either path (one tag-to-type compare, then one metatable
identity compare), or `None`; or the Luau-style type error naming `T::NAME`.

{{ api_signature(value="struct Owned<T: Userdata>(pub T)") }}

A userdata result: returning `Owned(value)` from a bound function moves `value` into a new
Lua-owned instance of whichever path this VM registered `T` on. Pushing an `Owned<T>` by
reference (`T: Clone`) clones the payload; `SequenceItem` too.

{{ api_signature(value="fn push_owned<'s, T: Userdata>(scope: &'s impl Scope, value: T) -> Result<ValueView<'s>>") }}

The same as a free function.

{{ api_signature(value="struct StableRef<T>(NonNull<T>)") }}

A pointer to an engine-owned object that Luau may observe but never owns. The host guarantees
the pointee and its address stay valid whenever Lua can reach the userdata, and that it is
destroyed only after Lua execution has stopped; dropping the userdata never dereferences it.
Prefer `Arc<T>` or handle payloads wherever the engine object's lifetime is not already this
strong. `Clone`, `Copy`, `Debug`.

| Method | |
|---|---|
| `unsafe fn new(pointer: NonNull<T>) -> StableRef<T>` | The caller upholds the contract above |
| `fn as_ptr(&self) -> *const T` | |

{{ api_signature(value="struct Borrowed<T: Userdata>(pub StableRef<T>)") }}

A stable borrow as a userdata value, untagged types only (tagged payloads are always owned).
Pushable and returnable; pushing one for a type that is tagged in this runtime is a logic error.
`Clone`, `Copy`, `Debug`.

{{ api_signature(value="enum Storage<T> { Owned(T), Borrowed(StableRef<T>) }") }}

Untagged userdata storage: the payload is owned by Luau or a stable borrow. `repr(C)`.

| Method | |
|---|---|
| `fn get(&self) -> &T` | The payload, owned or borrowed |
| `fn owned(&self) -> Option<&T>` | Only when Luau owns it |

## tagged

Module `l3i::userdata::tagged`. Registration order is the contract: the metatable is built and
frozen first, then the destructor and metatable are published for the tag last, because Luau's
setters cannot report failure. Instances are allocated with `lua_newuserdatataggedwithmetatable`
and initialised at once.

{{ api_signature(value="fn register<T: Userdata>(runtime: &Runtime, tag_value: RuntimeTag, configure: impl FnOnce(&mut MetatableBuilder<'_>) -> Result<()>) -> Result<()>") }}

Registers `T` under `tag_value` in this VM with a metatable configured by `configure`.
Registering the same `T` twice under the same tag is a no-op. Errors (`Error::Logic`): a tag
outside `1..TAG_LIMIT`; another type on the tag; `T` already on another tag; `T::NAME` already
naming a registry metatable or not under the debug roots; a failure inside `configure`, which
unpublishes the half-built metatable.

| Function | Meaning |
|---|---|
| `fn tag_of<T: Userdata>(scope: &impl Scope) -> Option<RuntimeTag>` | The tag this VM assigned to `T` |
| `fn is_registered<T: Userdata>(scope: &impl Scope) -> bool` | Whether `register::<T>` ran here |
| `fn push<'s, T: Userdata>(scope: &'s impl Scope, value: T) -> Result<ValueView<'s>>` | A new tagged userdata: one allocation, one write. A logic error when `T` is not tagged here |
| `fn test<'v, T: Userdata>(value: ValueView<'v>) -> Option<&'v T>` | The payload when `value` is a `T`: one tag read and one `TypeId` compare, no metatable walk |
| `unsafe fn test_mut<'v, T: Userdata>(value: ValueView<'v>) -> Option<&'v mut T>` | Mutable access. No other reference to the same payload may be live: Luau lets one value appear at several slots and the binder cannot see aliasing through them |
| `fn check<'v, T: Userdata>(value: ValueView<'v>) -> Result<&'v T>` | `test` or a Luau-style type error |

```rust
use std::cell::Cell;
use l3i::Runtime;
use l3i::userdata::{Owned, Userdata, tagged};

struct Bar { value: Cell<i32> }

unsafe impl Userdata for Bar {
    const NAME: &'static str = "dreamweave.tests.Bar";
}

fn main() -> l3i::Result<()> {
    let runtime = Runtime::new()?;
    tagged::register::<Bar>(&runtime, 20, |ty| {
        ty.method("double", |bar: &Bar| bar.value.get() * 2)?;
        ty.property_rw("value", |bar: &Bar| bar.value.get(), |bar: &Bar, value: i32| bar.value.set(value))?;
        ty.property("readonly", |bar: &Bar| bar.value.get() + 100)?;
        ty.method("add", |bar: &Bar, other: &Bar| bar.value.get() + other.value.get())
    })?;
    let make = runtime.bind_function("dreamweave.tests.bar", |value: i32| Owned(Bar { value: Cell::new(value) }))?;
    runtime.set_global("bar", &make)?;
    runtime.exec("local b = bar(13) assert(b:double() == 26) b.value = 4 assert(b.readonly == 104 and b:add(bar(1)) == 5)")?;
    Ok(())
}
```

## untagged

Module `l3i::userdata::untagged`. Each type has exactly one metatable, published under its
canonical name in the registry, in a private catalogue table, and in a per-type slot keyed by
the Rust type, so duplicates fail in any of them. Checks compare exact metatable identity from a
per-VM cache and require the metatable to be read-only.

{{ api_signature(value="fn register<T: Userdata>(runtime: &Runtime, configure: impl FnOnce(&mut MetatableBuilder<'_>) -> Result<()>) -> Result<()>") }}

Registers `T`'s metatable configured by `configure`. Transactional: a duplicate in any of the
three places, a type already tagged in this runtime, an empty or invalid name, or a failing
`configure` leaves every registry entry as it was. `__metatable` protection is restored even if
`configure` removed it, and the table is frozen.

| Function | Meaning |
|---|---|
| `fn push<'s, T: Userdata>(scope: &'s impl Scope, value: T) -> Result<ValueView<'s>>` | A new Luau-owned userdata of type `T` |
| `fn push_borrowed<'s, T: Userdata>(scope: &'s impl Scope, borrowed: StableRef<T>) -> Result<ValueView<'s>>` | A userdata borrowing an engine-owned `T` |
| `fn test<'v, T: Userdata>(value: ValueView<'v>) -> Option<&'v T>` | The payload, owned or borrowed |
| `fn test_owned<'v, T: Userdata>(value: ValueView<'v>) -> Option<&'v T>` | Only when Luau owns it |
| `fn check<'v, T: Userdata>(value: ValueView<'v>) -> Result<&'v T>` | `test` or a Luau-style type error |
| `fn is_registered<T: Userdata>(frame: &Frame<'_>) -> bool` | Whether this VM registered `T` |

## MetatableBuilder

Module `l3i::userdata::metatable`. Member registration is a phase machine with explicit conflict
rules: methods only give a plain methods table as `__index` (no dispatcher); the first property
getter upgrades to a generated `__index` (methods, then getters) plus a generated `__namecall`
keyed by Luau atoms; the first setter installs a generated `__newindex`, and misses are
read-only errors. Explicit `__index`, `__newindex`, `__namecall` and `__len`, native method
tables and duplicate member names conflict with generated dispatch and fail at registration.
Metatables are protected by default (`__metatable = false`), and debug names are
`<__type>.<member>`, `<__type>.get.<member>`, `<__type>.set.<member>`. The generated dispatchers
run bound members directly on their own stack instead of `lua_call`ing the member closure, so a
member call costs one Luau call frame, not two.

{{ api_signature(value="struct MetatableBuilder<'s>") }}

Configures a metatable that lives at a fixed stack index for the builder's lifetime. Not
`Clone`. Handed to the `configure` closure of `tagged::register`, `untagged::register`,
`ModuleBuilder::userdata` and the planner.

| Method | Meaning |
|---|---|
| `fn set_type(&mut self, name: &str) -> Result<()>` | The script-visible `__type`; registration sets it from `T::NAME` |
| `fn type_name(&self) -> Result<String>` | The `__type` string, required before members can be named |
| `fn set_field(&mut self, name: &str, value: &Value) -> Result<()>` | Raw-sets `value` under `name` |
| `fn method<F: Binding<M>, M>(&mut self, name: &str, callable: F) -> Result<()>` | A method: the first parameter is the receiver (`&T` whose `NAME` is this `__type`), excluded from argument numbering |
| `fn property<G: Binding<MG>, MG>(&mut self, name: &str, getter: G) -> Result<()>` | A read-only property; `getter` takes the receiver |
| `fn property_rw<G: Binding<MG>, MG, S: Binding<MS>, MS>(&mut self, name: &str, getter: G, setter: S) -> Result<()>` | A read/write property; the setter takes the receiver and exactly one Lua value |
| `fn unbound_method(&mut self, name: &str, function: &Value) -> Result<()>` | An already-bound function as a callable member without a native receiver type |
| `fn metamethod<F: Binding<M>, M>(&mut self, metamethod: &str, callable: F) -> Result<()>` | A bound closure as `<metamethod>`, named `<__type>.<metamethod>` |
| `fn raw_metamethod(&mut self, metamethod: &str, function: lua_CFunction) -> Result<()>` | A raw C function as `<metamethod>` |
| `fn metamethod_value(&mut self, metamethod: &str, function: &Value) -> Result<()>` | An already-built function as `<metamethod>` |
| `fn begin_native_methods(&mut self) -> Result<()>` | Starts a table of hand-written C functions; cannot be mixed with registered methods or properties |
| `fn add_native_method(&mut self, name: &str, body: lua_CFunction, discriminator: c_int) -> Result<()>` | Adds `body` under `name` with `discriminator` as upvalue 1, named `<__type>.<name>` |
| `fn install_native_method_index(&mut self) -> Result<()>` | Freezes the native methods table and installs it as `__index` |
| `fn frozen_native_methods(&mut self) -> Result<Value>` | The frozen native methods table |
| `fn native_discriminator(call: &Call<'_>) -> c_int` | For native method bodies: `Call::upvalue(1)`, the discriminator |
| `fn array_iterator<F: Binding<M>, M>(&mut self, next: F) -> Result<()>` | `__iter` returning `(next, object, 0)`: `next` receives the object and the numeric control and returns the next `(control, value)` or `None` |
| `fn keyed_iterator<F: Binding<M>, M>(&mut self, next: F) -> Result<()>` | `__iter` returning `(next, object, nil)`: `next` receives the object and the previous key |
| `fn cursor_iterator<C, Make, F, M>(&mut self, make_cursor: Make, next: F) -> Result<()>` | `__iter` creating a private cursor per loop: `make_cursor: Fn(&Call<'_>) -> Result<C>` sees the call (argument 1 is the object) and `next` receives `Cursor<C>` plus the ignored control |
| `fn direct_dispatch<T: DirectAccess>(&mut self, which: DirectMetamethods) -> Result<()>` | Direct-access wrappers; see [Direct access and the VM](@/docs/api/direct.md) |

Conflicts are `Error::Logic`, in the C++ binder's words: `Explicit __index conflicts with
setMethod`, `setMethod conflicts with native method registration`, `receiverTypeName mismatch
for <type>`, `A property setter must accept one Lua value argument`, `<type>.<name> already
registered`, `Metatable already has an __iter metamethod`.

## Iterators

Module `l3i::userdata::iterator`. A stateless iterator reuses the iterated object as the
generic-for state; a cursor iterator gives each loop a private cursor userdata, so nested and
concurrent loops over one object never share state.

{{ api_signature(value="struct Cursor<'c, C>") }}

The private per-loop state of a cursor iterator, as the `next` function receives it. Derefs to
`C`; mutation goes through interior mutability in `C`. A `Param` whose `EXPECTED` is `iterator
cursor`.

{{ api_signature(value="type Step<K, V> = Option<(K, V)>") }}

A convenience return for iterator `next` functions: `Some((key, value))` continues, `None` ends
the loop.

## Modules

Module `l3i::module`: the component registration contract. A component crate implements
`LuauModule`; the host owns the `Runtime`, decides which modules exist, and calls
`Runtime::register_module`. The result is a frozen package table the host places wherever its
script environment wants it.

{{ api_signature(value="trait LuauModule { const NAME: &'static str; fn register(runtime: &Runtime, module: &mut ModuleBuilder<'_>) -> Result<()>; }") }}

`NAME` is a dot-separated package path rooted at one of the host's debug roots, such as
`dreamweave.assets`.

{{ api_signature(value="struct ModuleBuilder<'r>") }}

Builds one package table. Every function it binds is named `<module>.<key>`.

| Method | Meaning |
|---|---|
| `fn path(&self) -> &str` | The package path |
| `fn table(&self) -> &Table` | The package table being built |
| `fn function<F: Binding<M>, M>(&mut self, key: &str, callable: F) -> Result<Function>` | Binds `callable` as `<path>.<key>` and stores it |
| `fn set<T: Push + ?Sized>(&mut self, key: &str, value: &T) -> Result<()>` | Any pushable value under `key` |
| `fn userdata<T: Userdata>(&mut self, tag: Option<RuntimeTag>, configure: impl FnOnce(&mut MetatableBuilder<'_>) -> Result<()>) -> Result<()>` | Registers a userdata type, tagged under `tag` or untagged for `None` |
| `fn metamethod<F: Binding<M>, M>(&mut self, name: &str, callable: F) -> Result<Function>` | A metamethod on the package's own metatable, created on first use |
| `fn set_metafield(&mut self, name: &str, value: &Value) -> Result<()>` | A value on the package's metatable |
| `fn finish(self) -> Result<Table>` | Freezes the package (and its metatable) and returns it |

## Read-only tables

Module `l3i::readonly`. Two flavours: freezing a table in place (`lua_setreadonly`), optionally
with a shared strict metatable whose `__index` raises `Key not found`; and a read-only view, a
frozen proxy whose `__index` is the backing table, with shared `__pairs`/`__iter`/`__ipairs`
factories that iterate the backing table without exposing it, a `__len`, and `__metatable =
false`. A view's backing table is recorded in a VM-private weak-keyed table so a view can be
recognised later.

| Function | Meaning |
|---|---|
| `fn make_read_only(runtime: &Runtime, table: &Table) -> Result<()>` | Freezes `table` in place |
| `fn make_read_only_view(runtime: &Runtime, table: &Table) -> Result<Table>` | A frozen proxy reading through `table` |
| `fn make_strict_read_only_view(runtime: &Runtime, table: &Table) -> Result<Table>` | The same; a missing key raises `Key not found` |
| `fn make_strict_read_only(runtime: &Runtime, table: &Table) -> Result<()>` | Freezes in place with the shared strict metatable. The table must not already be frozen or have a metatable |
| `fn set_read_only_field<T: Push + ?Sized>(runtime: &Runtime, table: &Table, key: &str, value: &T) -> Result<()>` | Sets `key` on a frozen table (or a view's backing table), restoring the frozen state afterwards. For tables the host owns |
| `fn make_frozen_package(runtime: &Runtime, package: &Table, to_string: &Function) -> Result<()>` | Freezes a package table after giving it a metatable with only `__tostring` |

A table from another VM is `Error::Logic`.

## Require

Module `l3i::require`: Luau's require-by-string runtime over a host `RequireNavigator`. Luau
resolves `require("./path")` by walking the navigator: reset to the requiring module, step to
parents and children, ask whether a module is present, load it. Luau supplies caching, cyclic
placeholders, `.luaurc` alias handling and the error messages scripts see.

{{ api_signature(value="trait RequireNavigator: 'static") }}

The host's module space, with an implicit current position maintained between `reset` and
`load`.

| Method | Default | Meaning |
|---|---|---|
| `fn is_require_allowed(&self, requirer_chunkname: &str) -> bool` | `true` | Whether the chunk may call `require` at all |
| `fn reset(&self, requirer_chunkname: &str) -> Navigate` | required | Points the position at the requiring module |
| `fn jump_to_alias(&self, path: &str) -> Navigate` | `NotFound` | An aliased module from a configuration file |
| `fn to_alias_override(&self, alias: &str) -> Option<Navigate>` | `None` | Resolve `@alias` before configuration files |
| `fn to_alias_fallback(&self, alias: &str) -> Option<Navigate>` | `None` | Resolve `@alias` after they failed |
| `fn to_parent(&self) -> Navigate`, `fn to_child(&self, name: &str) -> Navigate` | required | |
| `fn is_module_present(&self) -> bool` | required | Whether the position names a loadable module |
| `fn chunkname(&self) -> Option<String>` | required | The chunk name the module runs under |
| `fn loadname(&self) -> Option<String>` | required | The name passed to `load` |
| `fn cache_key(&self) -> Option<String>` | required | The key Luau caches the result under |
| `fn config_status(&self) -> ConfigStatus` | `Absent` | |
| `fn config(&self) -> Option<String>` | `None` | The configuration file's contents at the position |
| `fn luau_config_timeout_ms(&self) -> Option<i32>` | `None` | Milliseconds for a Luau-syntax configuration file; Luau's default (2000) otherwise |
| `fn load(&self, scope: &Requirer<'_>, path: &str, chunkname: &str, loadname: &str) -> Result<Load>` | required | Runs the module on the requiring thread: push its results and say how many, or that the requirer should yield. An error raises into the requiring script |

`impl_scope::Requirer` is `bind::Call`.

{{ api_signature(value="enum Navigate { Success, Ambiguous, NotFound }") }}

{{ api_signature(value="enum ConfigStatus { Absent, Ambiguous, Json, Luau }") }}

{{ api_signature(value="enum Load { Results(c_int), Yield }") }}

`Navigate` and `ConfigStatus` are `Clone`, `Copy`, `Debug`, `Eq`.

On `Runtime`:

| Method | Meaning |
|---|---|
| `fn install_require(&self, navigator: impl RequireNavigator) -> Result<()>` | Installs `navigator` and registers Luau's `require` as a global. Install once at setup |
| `fn require_function(&self, navigator: impl RequireNavigator) -> Result<Function>` | Installs `navigator` and returns the `require` closure pinned without a global, for sandboxes |
| `fn proxy_require_function(&self) -> Result<Function>` | A `proxyrequire(path, chunkname)` closure over the installed navigator |
| `fn register_require_module(&self, path: &str, value: &Value) -> Result<()>` | `value` as the permanent result of requiring the alias `path` |
| `fn registered_require_modules(&self) -> Vec<String>` | Every path registered with `register_require_module`, in registration order: the plan's modules and the host's |
| `fn clear_require_cache_entry(&self, cache_key: &str) -> Result<()>` | |
| `fn clear_require_cache(&self) -> Result<()>` | |

{{ api_signature(value="trait RequirePlaceholders: Scope + Sized") }}

Placeholder support for cyclic requires, implemented for every `Scope`, for use from
`RequireNavigator::load`: `create_require_placeholder(&self)`, `lock_require_placeholder(&self,
index: c_int)` and `populate_require_placeholder(&self, placeholder: c_int, result: c_int)`.

## Libraries

Module `l3i::libraries`: standard-library and VM utility entry points the C++ binder never
needed but Luau offers.

{{ api_signature(value="enum Library { Base, Coroutine, Table, Os, String, Bit32, Buffer, Utf8, Math, Debug, Vector, Integer, Class }") }}

One of Luau's standard libraries; `Class` is Luau's experimental `class` library. `Clone`,
`Copy`, `Debug`, `Eq`, `Hash`.

| Item | Meaning |
|---|---|
| `const STANDARD: [Library; 12]` | Every library `luaL_openlibs` opens, in its order |
| `fn global_name(self) -> &'static str` | The global it is installed as (empty for `Base`) |

On `Runtime`:

| Method | Meaning |
|---|---|
| `fn open_library(&self, library: Library) -> Result<()>` | Opens one library selectively; `Base` must come first |
| `fn sandbox_luau(&self)` | `luaL_sandbox`: freezes every library table and the globals table, marks it safe; `Thread::sandbox` then gives threads writable global proxies |
| `fn register_library(&self, name: &str, functions: &[(&str, lua_CFunction)]) -> Result<Table>` | `luaL_register`: creates or reuses the global table `name`, fills it with C functions named `name.function`, returns it pinned |
| `fn find_table(&self, path: &str) -> Result<Table>` | `luaL_findtable`: the table at dotted `path` under the globals, created along the way; an error names the first segment that is not a table |
| `fn set_jit_inliner(&self, enabled: bool)` | Luau's experimental inliner for this VM |

{{ api_signature(value="struct StringBuilder<'s>") }}

Luau's `luaL_Strbuf`: builds a string in place, spilling to a mutable Lua string when the inline
buffer fills, and pushes the finished string onto the scope it was opened on. The builder keeps
its spill storage on the stack below anything pushed after it; finish it before popping through
it.

| Method | Meaning |
|---|---|
| `fn new(scope: &'s impl Scope) -> StringBuilder<'s>` | |
| `fn push_str(&mut self, text: &str) -> &mut Self`, `fn push_bytes(&mut self, bytes: &[u8]) -> &mut Self` | |
| `fn push_value(&mut self, view: ValueView<'_>) -> Result<&mut Self>` | `tostring`-style text of the value (`luaL_addvalueany`) |
| `fn finish(self) -> ValueView<'s>` | Pushes the built string and returns a view of it |
