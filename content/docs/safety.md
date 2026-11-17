+++
title = "Safety model"
description = "What the crate guarantees about exceptions, panics, unsafe code, value lifetimes, stack aliasing and type identity, the Luau facts the ecosystem builds on, and the places where it diverges from the C++ binder on purpose."
weight = 75

[extra]
kind = "reference"
+++

## What the crate guarantees

**Luau errors unwind; Rust never catches them.** Luau is built with C++ exceptions. A Lua error
raised inside a bound function unwinds through the binding's Rust frames, running their
destructors, to Luau's `pcall`. Nothing puts `catch_unwind` on that path, because Rust aborts on
a foreign exception it catches. Hand-written dispatchers and the vector writer raise through
`luaL_error` with no Rust value that needs dropping live at the raise.

**Panics in native code abort.** A Rust panic inside a native call cannot be turned into a Lua
error safely, so the trampoline every bound function runs through aborts the process. Hooks and
GC callbacks are under the same rule. Bound code reports failure by returning `Err`.

**Host-level raising calls run under `lua_pcall`.** Outside any Lua call there is no `pcall`
above the host to catch a raise, so every API operation that can raise (`lua_setglobal` on a
read-only globals table, `lua_gettable` through a raising `__index`, `lua_concat`) runs under a
protected call at host level and comes back as `Err`. `Frame::raising` picks the path from the
frame's `is_host_level()`; inside a call the same operation runs directly and unwinds to Luau.

**Every `unsafe` block states the invariant it relies on**, and GC destructors never call the
Lua API: `Userdata::Drop`, the drop of a bound closure's captures, and cursor state all run while
Luau sweeps. That is why `Userdata` is an `unsafe trait`, and why the iterator factories keep
their `next` function as a real Lua upvalue rather than a pinned `Value` captured in Rust.

**Values cannot dangle.** A `Value` holds a weak handle to its runtime's lifetime token. A value
that outlives its `Runtime` becomes invalid (`is_valid()` is false, `type_of()` is `None`,
calls fail with a logic error) instead of touching a closed VM. `Runtime::drop` ends the token
before `lua_close`, and drops extension state before that.

**No two frames alias one stack region.** `Runtime::stack()` leases the root stack; a second
root while one is alive and not suspended in a Lua call panics. Native-call stacks arise only
from Luau calling into Rust, which implies the same exclusivity. Frames nest strictly, sibling
frames panic at the opening line, views borrow the frame that pushed them, and pops take
`&mut`, so a view of a recycled slot is unrepresentable.

**Untagged identity is exact.** An untagged type's registry key is a leaked per-type address
recorded against its exact `TypeId`, never a hash of it, so two types can never share a key and
`test::<T>()` can never accept another type's storage. Checks compare exact metatable identity
and require the metatable to be read-only.

**Tags are the host's, within Luau's limit.** `build.rs` owns `-DLUA_UTAG_LIMIT=254` and
`l3i::TAG_LIMIT` mirrors it. Valid tags are `1..TAG_LIMIT`; tag 0 is Luau's untagged default and
nothing else is reserved. A tag identifies the payload type registered for it in that VM, which
is why `memory::raw::set_userdata_tag` is `unsafe`.

**The value layout is proven, not assumed.** The binder reads argument slots straight from
Luau's 16-byte value layout. `csrc/extra.cpp` pins every offset at compile time, and each
runtime proves the mirror against the API once at creation, one value of every type, so a Luau
bump that moves a byte or renumbers a type tag (0.747 removed one) fails at once rather than
misreading a slot.

**A packed scalar's bits are never a pointer.** On the light userdata carrier the 64 bits sit in
the pointer word of a light userdata. They are written and read with `memcpy` through that
mirror, never converted to a pointer, never followed, and the kind is read from the top four bits,
not from the tag. Light userdata tags `128..160` belong to packed kinds, so a host's
`LightUserdata` push or `set_light_userdata_name` at or above 128 is refused, as is a host pointer
whose top four bits are not zero, which would read as a kind. Each runtime proves its carrier at
creation: every single bit, zero, all ones and non-canonical patterns through a push, a read,
equality, inequality under another tag and a table key, or the runtime is not made.

**Bytecode from an older build is refused, not misread.** Luau 0.747 dropped the integer constant
but kept bytecode version 14, so `load_bytecode` walks a chunk's constants first and refuses one
holding an integer constant, a constant kind this build does not know, or a function cut short,
with an error naming the chunk. Bytecode is otherwise trusted as Luau trusts it: load only what a
build of l3i compiled.

**Buffers are copied, not sliced.** A script can pass one buffer to two parameters, so safe
`BufferView` access goes through bounds-checked copies. The slice forms are `unsafe fn
bytes_unchecked` and `bytes_mut_unchecked`, for trusted code that proves nothing writes the
buffer meanwhile, which are the rules `lua_tobuffer` imposes on C.

**Fast flags are frozen.** The policy is one named list, l3i's own (`flags::LUAU_FLAGS` and,
under `jit`, `flags::LUAU_CODEGEN_FLAGS`), applied before the first VM or the first compile and
never changed by the crate afterwards. A flag name the linked Luau lacks is a logic error, never
a silent no-op, so a Luau bump cannot drop a flag quietly.

## Questionable semantics carried over from the C++ binder

None remain. The last one, the C++ binder's `expected integer, got integer` for an out-of-range
Luau integer, was replaced on 2026-09-27 by a message naming the value and the type, since the
accidental wording told the caller nothing; Luau 0.747 then removed the integer type altogether.

## Luau facts the ecosystem builds on

**Luau 0.747 has no integer type.** A double holds every whole number up to 2^53 exactly, so l3i
pushes counts, sizes, offsets and small ids as numbers and refuses, rather than rounds, one past
that. An opaque 64-bit pattern (a digest, a client or protocol id) is a `Bits64`; a packed scalar
is its kind's type. Both compare and key tables by their bits, never equal a number, and have no
arithmetic or ordering.

```luau
local bits64 = require("@dream/bits64")
local id = bits64.fromNumber(42)
assert(id == bits64.fromNumber(42) and id ~= 42)
assert(typeof(id) == "Bits64")
local ok = pcall(function() return id < bits64.fromNumber(43) end)
assert(not ok)
```

## Deliberate divergences

- **Bound callables are `Fn`, not `FnMut`.** The C++ binder accepted mutable lambdas; a binding
  can re-enter itself through Lua, which would alias a `&mut` capture. Mutable state lives in
  `Cell` and `RefCell` captures.
- **Views cannot alias a recycled slot.** The C++ test `StaleViewsAreRawIndices` pinned that a
  view of a popped slot silently reads whatever is pushed there next. In Rust a view cannot
  outlive the frame that pops it, pops take `&mut`, and out-of-order frames panic at their
  opening line.
- **Receivers are `&T`, never `&mut T`.** The same userdata can appear in several argument slots
  of one call, so a `&mut` receiver could alias a `&T` argument. Mutation goes through interior
  mutability in the payload; `tagged::test_mut` exists as an `unsafe` escape hatch.
- **Method calls without a receiver say so.** The C++ `MethodSignatureBase::invoke` checked
  counts before reading the receiver, so `obj.method()` failed with `expected at least N, got
  -1`. Here the receiver is read first and a receiver-less call reports `missing argument #1 to
  '<name>' (<type> expected)`.
- **A missing required argument is one message.** The C++ binder nested `bad argument count
  (got M)` inside `bad argument #N (expected T)`. Here it is `<name>: bad argument #N (expected
  T): missing argument`.
- **`LuauExperimentalIfLocalSyntax` is on.** The C++ binder leaves it off; `if local x = f()
  then .. end` parses here.
- **`LuauExperimentalWhileLocalSyntax` is on.** `while local x = f() do .. end` parses here, and so does
  `while const`. Luau still calls the syntax experimental.
