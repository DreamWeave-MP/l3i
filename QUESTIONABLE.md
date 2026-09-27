# Questionable semantics carried over from the C++ binder

The port reproduces the C++ binder's observable behaviour. Where that behaviour looks
accidental, it is listed here rather than silently corrected. Each entry names the source and
the test that pins it.

- **Luau integers report `expected integer, got integer` when out of range.**
  `ValueView::as<Integral>()` used `LUA_TINTEGER` as the expected type for both non-integral
  numbers and out-of-range integers. Ported as-is (`convert::scalar`).

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
