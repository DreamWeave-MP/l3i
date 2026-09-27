# Questionable semantics carried over from the C++ binder

The port reproduces the C++ binder's observable behaviour. Where that behaviour looks
accidental, it is listed here rather than silently corrected. Each entry names the source and
the test that pins it.

- **Method calls without a receiver report a negative argument count.**
  `MethodSignatureBase::invoke` validates counts before reading the receiver, so `obj.method()`
  with no arguments at all fails with `bad argument count (expected at least N, got -1)`.
  Ported as-is (`bind::params::Params::materialize_method`; `tests/tagged.rs`).
- **A missing required argument reports two nested count messages.**
  `materializeOne` raised `throwMissingArgument` inside the `catch` that wraps conversion
  failures, so the message reads `<name>: bad argument #N (expected T): <name>: bad argument
  count (got M)`. Reachable only after an optional consumed the last argument. Ported as-is
  (`bind::param::ParamItem::materialize`).
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
- **`LuauExperimentalIfLocalSyntax` is on.** OpenMW leaves it off.
