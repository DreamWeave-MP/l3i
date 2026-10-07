# Safe AST specialization: decision note

**Decision: deferred.** Not implemented in this campaign; nothing in the current evidence needs
it, and the data plane removed the case that motivated it.

## What it was for

Package-owned specialization: a package receives static metadata (a schema, a component
layout, a query shape), builds a specialized Luau function through a hygienic, validated AST
API, compiles and caches it, and so escapes generic abstraction overhead in its hot loops.

## What now covers that ground

- **Expressed intent lowers directly.** Every comprehension-shaped loop goes through one
  pipeline plan ([DATA_PLANE.md](DATA_PLANE.md) §1); dense, filtered, counting, summing,
  min/max/any/all and sink forms are within 0.1% of the best handwritten loop
  ([COMPREHENSIONS.md](COMPREHENSIONS.md)). A package has nothing to gain by generating these
  loops itself.
- **Generic bulk work is a native call, not generated code.** Reductions, comparison into
  selections, selection algebra, gather/scatter, fill, element-wise arithmetic, stable argsort
  and partition over typed spans are bound Rust (plus a CodeGen loop for the reductions). The
  interpreter numbers show 6 to 140× fewer instructions than the best obvious Luau, with no
  crossover down to 16 elements. Code generation cannot beat a native kernel for these.
- **The recognized JSL forms reach that substrate automatically**, and the explicit API is
  first-class for everything JSL does not recognize.

## What remains awkward

- Per-element logic that is neither a known data op nor expressible as a pipeline over one
  source: nested record access with branching, multi-output updates. These stay fused scalar
  Luau, which under `jit` is a native loop already; a generated AST would produce the same loop.
- Wide element records (structs in buffers) with per-field projections. The data plane's
  contiguous-kind spans do not express a strided field; a package currently reads fields with
  `buffer.read*` in a JSL loop. If this recurs, the right answer is a strided span in the data
  plane (one operand change, every operation benefits), not package code generation.
- Query shapes whose *structure* is only known at run time (a filter composed from user
  input). Those compose today from selections (`compare` per predicate, then `intersect` /
  `union`), with no code generation.

## Re-open when

A neutral fixture shows a workload that (a) is not a pipeline over one source, (b) is not a
bulk data operation or a composition of them, and (c) loses measurably to a hand-specialized
Luau function. None of the campaign's fixtures do. If one appears, implement the safe AST
substrate under the original requirements (hygienic locals, validation before Luau sees the
tree, source attribution, a compile-and-cache API, no raw compiler pointers) and prove it with a
neutral package, not a domain integration.
