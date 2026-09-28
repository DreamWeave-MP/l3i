# Questionable semantics carried over from the C++ binder

None remain. The last one, the C++ binder's `expected integer, got integer` for an out-of-range
Luau integer, was replaced on 2026-09-27 by `integer <value> is out of range for <type>` (and
`number <value> is out of range for <type>` for a rounded Lua number), since the accidental
wording told the caller nothing.

# Deliberate divergences

- **Bound callables are `Fn`, not `FnMut`.** The C++ binder accepted mutable lambdas; a binding
  can re-enter itself through Lua, which would alias a `&mut` capture. Mutable state lives in
  `Cell`/`RefCell` captures.
- **Views cannot alias a recycled slot.** `StaleViewsAreRawIndices` pinned that a C++ view of a
  popped slot silently reads whatever is pushed there next. In Rust a view cannot outlive the
  frame that pops it, pops take `&mut`, and out-of-order frames panic at their opening line.
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
- **`LuauExperimentalIfLocalSyntax` is on.** OpenMW leaves it off.
