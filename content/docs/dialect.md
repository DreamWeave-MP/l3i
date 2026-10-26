+++
title = "The l3i dialect"
description = "Comprehensions, reducers, sinks, slices and record patterns: source syntax that lowers to the handwritten loop, how the frontend keeps tooling in original coordinates, what it costs, and what it refuses to be."
weight = 92

[extra]
kind = "guide"
+++

```luau
local ids = [for e in entities if e.active => e.id]
local mass = sum[for b in bodies[1:n, "f32@16"] => b]
local {x, y, z} = position
```

Each of those compiles to the loop you would have written by hand, one setup instruction apart,
and to nothing else: no closure, no iterator, no result table unless you asked for one. Luau
itself is unmodified. A surface frontend built on Luau's own lexer rewrites the dialect into stock
Luau text, records where every byte came from, and hands that to the stock compiler, the stock
type checker or `@dream/luau`. There is exactly one frontend for all three consumers; strict
versus tolerant is a policy flag, not a second grammar.

Compilation is strict: a comprehension missing its projection does not run. Analysis and
`@dream/luau` are tolerant: they keep a record of the half-typed construct so completion and
diagnostics work while you are still typing it. Ordinary Luau passes through untouched, and a file
without a `[` is not even lexed by the frontend.

## Comprehensions

```luau
[for x in xs => x * 2]
[for e in entities if e.active => e.id]
[for x in xs if p(x) if q(x) for y in children(x) => project(x, y)]
[for row in rows => [for x in row => x * 2]]
```

The result is a dense, 1-based array. Sources are evaluated once at their nesting level and their
length is captured on entry, like a numeric `for`; this is a dense indexable-sequence operation,
not generic iteration, so a sparse table has whatever `#` says it has. Filters run left to right
with ordinary Luau truthiness and short-circuit before inner generators and the projection. Every
accepted projection runs exactly once.

A projection that produces `nil` raises `L3i comprehension projection produced nil; filter nil
explicitly`. A dense array with a hole in it is not a result, it is a bug waiting for the next
`#`, and the guard costs one executed VM instruction per accepted element. `false` is a valid
projection.

`#[for ...]` counts without materializing: the result table is never allocated, while the
projection, its effects, its failures and its nil check all still happen. Parentheses or a comment
between the `#` and the `[` fall back to counting a materialized table, which is still correct.
Allocation elimination is authorized. Observable evaluation elimination is not.

Comprehensions nest to 128 levels. Parentheses and brackets inside a clause do not count toward
that; Luau's own parser bounds them.

A comprehension that is the whole initializer of one annotated local, `local items: T = [...]`,
or the whole value of a `return` in a function that declares one result type, is type-checked as
the loop it stands for: each projection is checked against `T`'s element type, so
`local pairs: { [string | number]: Value } = [for i in range(1, n) => { i, "a" }]` is accepted
where Luau's invariant table indexers would otherwise refuse the `{ { number | string } }` the
projection infers on its own, and a misspelled record field is still an error. Analysis gives the
wrapper, its accumulator and each projected value that type; compiled code is unchanged. An
annotation spread over several lines or holding a comment, a local declared beside others, and a
value that continues past the comprehension (`[...] or {}`) keep the untyped result; cast the
projection there (`=> { i, "a" } :: Value`).

### Generators

A generator slot is `name in source` or `a, b in source`. The source is a dense table, a slice,
or one of four intrinsics that are spelled like calls and are not calls: they are never resolved
against a binding of that name, and `range`, `enumerate` or `zipStrict` anywhere else in the
program is whatever you defined it to be.

| Source | Binds | Lowers to |
|---|---|---|
| `xs` | each element, `1..#xs` | an indexed numeric loop over the captured length |
| `range(first, last[, step])` | each number, inclusive, step 1 by default | a numeric `for`; a zero step raises before the loop, and direction and emptiness follow stock numeric `for` |
| `enumerate(xs)` | `i, value` | an indexed loop; no pair is allocated |
| `zipShortest(xs, ys, ...)` | one binding per source, aligned by index | one loop to the shortest captured length |
| `zipStrict(xs, ys, ...)` | the same | every argument's length is compared with the first's before the loop; a mismatch raises `JSL zipStrict inputs must have equal lengths` |
| `xs[a:b]`, `buf[a:b, "f32"]` | the selected elements | a loop over the selected indices; see [Slices](#slices) |

Every argument of an intrinsic is evaluated once, left to right, before that generator starts, so
a dependent `range(1, #row)` under an outer generator is evaluated once per outer iteration.
There is no bare `zip`: whether unequal lengths are an error is not something you get to choose
by accident.

### Reducers

Five names consume a comprehension when written immediately before its `[for`, with whitespace
or comments allowed between and never after a `.` or `:`. Like the intrinsics they are syntax,
not bindings. Each lowers to one traversal with no result table.

| Reducer | Result | Empty pipeline | Per accepted projection |
|---|---|---|---|
| `sum[...]` | number | `0` | `total += value` |
| `min[...]` | first least projection | error `JSL min reducer received no elements` | `if acc == nil or value < acc then acc = value end` |
| `max[...]` | first greatest projection | error `JSL max reducer received no elements` | `if acc == nil or value > acc then acc = value end` |
| `any[...]` | boolean | `false` | `if value then return true end` |
| `all[...]` | boolean | `true` | `if not value then return false end` |

`sum` is a numeric reducer and Luau's arithmetic is the authority: there is no additive identity
protocol and no second type system. `min` and `max` order with Luau's `<` and `>`, so strings
and `__lt` metamethods work, mixed operands raise the ordinary comparison error, a NaN that
arrives first stays, and ties keep the first. The empty case is an error because the language has
no silent nil result; if you want a seeded minimum, write the loop.

**`any` and `all` short-circuit.** They stop at the first deciding projection and leave every
nesting level at once, so later generators, filters and projections do not run. Code that relies
on those effects must not use them. The projection must still not be `nil`.

### Sinks

```luau
into(out)[for x in xs if p(x) => f(x)]
local filled = into(scratch.rows)[for row in rows[a:b] => project(row)]
local samples, written = into(samples, "f32")[for x in xs if x > 0 => x * 2]
into(bodies, "f32@16", 8)[for i, v in enumerate(velocities) => v * i]
```

`into(destination)` makes the pipeline fill a caller-owned table in place and evaluates to the
destination. It replaces rather than appends: accepted projections go to `1..n` in pipeline
order, then `n+1..previous length` are set to `nil`, so the destination is dense afterwards and an
empty pipeline leaves an empty table. The destination is evaluated once, before any source, and
must be a table (`JSL sink destination must be a table`). It may not be any generator's source
table (`JSL sink destination must not be a pipeline source`, checked with `rawequal` once per
source evaluation), because the pipeline would overwrite elements it has yet to read. Reading it
inside a filter or the projection is your business. A sink is the one pipeline form that may
stand as a statement.

`into(buffer, "kind"[, offset])` writes each accepted projection with the `buffer.write<kind>`
the literal names, `offset + n × stride` bytes in, where the literal is an element kind or a
`kind@stride` layout; `"f32@16"` fills the f32 field of 16-byte records. The kind must be a string
literal so the write is a known fastcall. The expression evaluates to the buffer and the count
written. Capacity is explicit: a projection that would not fit raises `JSL sink buffer
destination is full`, nothing is truncated, and bytes past the written count are untouched. The
projection must be a number, with the buffer library's own conversion and errors.

## Slices

```luau
local part = values[first:last]
local header = bytes[1:16]
local heights = terrain[1:n, "f32"]
local masses = bodies[1:n, "f32@16+12"]
[for x in values[first:last] => project(x)]
```

Bounds are 1-based, inclusive, finite integers, evaluated once each after the source and before
anything is measured. They are clamped to the source's dense extent and `last < first` is empty.
An untyped slice of a table allocates an exactly sized dense result and bulk-copies it with a
snapshotted `table.move`; an untyped slice of a buffer is a byte slice copied with
`buffer.copy`. Neither consults a user binding named `table` or `buffer`. As a generator source
a slice allocates nothing: the loop walks the selected indices and a buffer reads each byte with
`buffer.readu8`.

A slice may name the element kind its buffer is read as. The bounds are then element indices,
the source must be a buffer (`JSL typed slice source must be a buffer`), a generator binding is
the element as `buffer.read<kind>` returns it, and a standalone typed slice copies the elements
into a new contiguous buffer. `kind@stride` reads one field of fixed-size records and
`kind@stride+offset` a field that is not the first. The kind must be a string literal: typed
slices never dispatch on representation at run time, and a recognized reduction over one lowers
straight to the data plane receiver of that layout.

The source of a slice is the whole prefix chain: `t.a[1:2]`, `o:get()[1:2]`, `t[1][1:2]` and
`t[1:3][2:3]` all slice what the chain produces. A bound spelled as an integer literal (decimal,
hex, binary, with or without `_`) is not checked at run time; every other spelling is.

A slice that is the whole source of a `for ... in` header, `for _, v in values[1:n] do`, is bound
to a local just before the `for`, where the header would have evaluated it. Luau's new solver
checks a function literal called in that position against the iterator protocol rather than as
the literal's result, so the slice written in place would analyse as the wrong type; held in a
local it is the table it is. A header with other expressions, `for k, v in next, xs[1:2] do`,
keeps the slice where it is written.

**Stock indexing always wins.** A bracket whose contents already parse as stock Luau is left to
Luau. `values[obj:method()]` is the method call it has always been, and so is `t[lo:hi()]` when
`lo` is a name, a call or a parenthesized expression, because `lo:hi()` is a valid method call on
`lo`. The frontend cannot tell the two readings apart and raises no diagnostic: a numeric `lo`
fails at run time with Luau's own `attempt to index number with 'hi'`. Only a bound no stock
expression admits is a slice: `t[1:hi()]`, `t[lo:2]`, `t[:hi()]`. When the last bound is a call,
parenthesize one side, `t[(lo):hi()]` or `t[lo:(hi())]`. This is language policy, not a bug
awaiting a fix; a structural diagnostic would refuse valid Luau.

A standalone `t[2:9]` of a 16-element table costs about 215 ns in a sandboxed runtime against
150 for a handwritten `table.create` plus `table.move`. The gap is the kind dispatch and the
bound normalization, not the closure Luau inlines around it: a shared helper was measured at no
gain and worse error positions, and rejected.

## Record patterns

```luau
local {x, y, z} = position
local {health: hp, name} = entity
local {transform: {position: pos, rotation}, id}: Entity = entity

function move({x, y, z}: Vec3, dt: number)
    return x + dt, y + dt, z + dt
end

local ids = [for {id, active, position: {z}} in entities if active and z > 0 => id]
```

A record pattern introduces a set of locals from the fields of one value. It appears in exactly
three places: a `local` declaration, a function parameter, and a comprehension generator slot.

```text
BindingPattern := Identifier | RecordPattern
RecordPattern  := "{" [RecordField {"," RecordField} [","]] "}"
RecordField    := Identifier | Identifier ":" BindingPattern
```

`{x}` binds field `x` to a local `x`; `{x: px}` renames; `{position: {x, y}}` nests to any depth
under the 128 limit; `{}` evaluates its source and reads nothing. `=` is never a rename, so it
stays free for a nil-default form if one is ever added. Keys are Luau names.

### What it means

A pattern consumes exactly one value and behaves as the obvious locals:

```luau
local source = f()
local x = source.x
local y = source.y
```

- The source is evaluated once, in the enclosing scope, before any binding of its pattern exists.
  `local x = 10 local {x, y} = {x = x + 1, y = x}` gives `11, 10`.
- Fields are read in source order, depth first, with ordinary indexing: `__index` runs, vectors
  and strings index as they always do, and there is no `rawget`, no table check, no extractor
  protocol and no run-time validation. A missing field binds `nil`. Indexing through a `nil`
  raises Luau's own `attempt to index nil with 'x'` on the line of the failing field, and later
  fields are not read.
- Values are bound, not copied. No table, closure or call is created by destructuring, and the
  bindings are ordinary locals: they shadow, are captured as upvalues, and can be assigned.
- A parameter pattern destructures its argument on entry, parameters in order, and the function's
  arity and type are unchanged: `function length({x, y}: Point): number` is `(Point) -> number`.
  A `nil` argument raises at entry, as `p.x` would.
- A generator pattern binds each element right after the loop reads it and **before** the
  clause's filters, so `[for {id, active} in entities if active => id]` reads `id` for rejected
  elements too. Extraction is never deferred, reordered past a filter, or dropped as unused:
  reads can be observable. A pipeline whose generator slot is a pattern never takes the data-plane
  path.

Luau's Analysis types everything; there is no second checker. `local {x, y} = point` with
`point: Point` infers `number` for both on both solvers. `local {x, y}: Point = value` annotates
the whole value. An optional field binds an optional local, with no implicit assertion. A field
the type lacks is a normal type error, reported at the key in your source.

Rejected, one diagnostic per mistake, at the offending token:

| Source | Diagnostic |
|---|---|
| `local {x, other: x} = v`, `function f({x}, x)`, `for i, {i} in ...` | `duplicate binding 'x' in record pattern` |
| `local {id, ...rest} = e` | `rest patterns are not supported; ...` |
| `local {health = 100} = e` | `defaults are not supported in record patterns; a missing field binds nil` |
| `local {["k"]: v} = e` | `computed keys are not supported in record patterns; ...` |
| `local {a: [b]} = e` | `indexed patterns are not supported; ...` |
| `local [a, b] = v` | `indexed destructuring is not supported; ...` |
| `local {x}, y = v`, `local {x} = a, b` | `a record pattern declaration binds exactly one pattern to one value` |
| `for {id} in pairs(t) do end` | `record patterns are not supported in generic for loops; ...` |
| `local {x}` | `expected '=' after record pattern; ...` |

Assignment into existing bindings, structural matching, per-field annotations, defaults, rest,
computed keys and tuple destructuring are not part of the language. Not yet, not accidentally.

### What it lowers to

```luau
local {position: {x, y}, health: hp}: Entity = entity
-- becomes
local H: Entity = entity local H2 = H.position local x = H2.x local y = H2.y local hp = H.health
```

The pattern is replaced by a holder local; annotation, `=` and value are copied unchanged; the
reads are inserted right after the value, each ended with `;`, with no line breaks added, so line
structure is preserved. A parameter's holder takes the pattern's place in the parameter list and
the reads open the body. A generator's holder is the loop's element local.

Where a value ends is Luau's parser's decision, not a JSL expression recognizer's. The frontend
estimates each boundary with a token skipper, and the parse every consumer makes anyway confirms
it: each holder's declaration must end exactly at its reads with exactly one value, and each
pattern parameter's body must begin exactly there. When an estimate is not confirmed, the
frontend lowers once with every pattern named by its holder, parses that with stock Luau, finds
the boundaries, and lowers again. Correctness never rests on the skipper. Holders are named
`__l3i_comp_<offset>`, salted when a source identifier collides, and appear in no surface tree,
completion list or diagnostic.

**A holder is a local.** It occupies one of the function's 200 registers and keeps its record
reachable until its block ends: `local {a} = v` costs two registers to the end of the block where
`local a = v.a` costs one, plus one per level of nesting. A body with about a hundred single-field
patterns in one block runs out of registers where the direct reads would not. Group related
bindings in one pattern, or wrap sections of a long body in `do ... end`. Freeing the holder in
the lowering itself was measured and rejected: the bound names become locals assigned after their
declaration, 11 percent slower to extract and 70 percent slower when a closure captures one,
because Luau then captures by reference.

## Recognized data pipelines

When [`@dream/data`](@/docs/builtin-extensions.md#dream-data) is in the plan, a compiled chunk
snapshots its alias global `__l3i_data` in the prelude, and the buffer branch of a recognized
pipeline calls the data plane once instead of looping:

```luau
sum[for x in buf[a:b] => x]                 -- U8:sum(buf, a - 1, n)
#[for x in buf[a:b] => x]                   -- n
#[for x in buf[a:b] if x > 127 => x]        -- U8:countGt(buf, a - 1, n, 127)
sum[for x in buf[a:b] if x >= limit => x]   -- U8:sumGe(buf, a - 1, n, limit)
min[for x in buf[a:b] if x ~= 0 => x]       -- U8:minNe(buf, a - 1, n, 0); nil is the empty error
max[for x in buf[a:b] => x]                 -- U8:max(buf, a - 1, n)
any[for x in buf[a:b] => x > 200]           -- U8:anyGt(buf, a - 1, n, 200)
all[for x in buf[a:b] => x <= limit]        -- U8:allLe(buf, a - 1, n, limit)
sum[for v in bodies[1:n, "f32@16"] => v]    -- F32_16:sum(bodies, 0, n)
```

`U8` is the chunk's `dream_data_Kind_u8` receiver, annotated in the prelude, so under `jit`
each call compiles to the receiver's unrolled native loop; without `jit` the bound method runs
once per pipeline. Either way the result is bit for bit what the scalar loop computes: bytes are
summed in `f64` in element order, `min` and `max` keep the loop's `<` and `>` rules and raise the
same empty-pipeline error before the call, and `any`/`all` over a slice past the end return
`false` and `true` with or without the extension.

Recognition is narrow on purpose, because errors and evaluation order are effects. A lone leading
slice generator; for `sum`, `min`, `max` and `#` a projection that is exactly the binding with at
most one `binding <op> threshold` filter; for `any` and `all` no filter and a projection that is
exactly such a comparison. The threshold is a number literal or a name the compiler has verified
to be a local, upvalue or parameter declared outside the comprehension, still guarded at run time
with `typeof(limit) == "number"` so a non-number takes the scalar loop and raises exactly its
comparison error. A global threshold stays a scalar loop: reading it once rather than per element
could be observable. The source, both bounds, every type check and the bounds normalization still
run in the ordinary prologue, so what raises, and when, is identical. Table sources never take
this path, and Analysis and tooling never see it. Anything else stays a fused scalar loop, which
under `jit` is a native loop already.

The alias is an ordinary global, read once when the chunk starts, like the other globals the
prelude snapshots (`buffer`, `table.create`, `error`, `rawequal`, `typeof`). Whoever can assign a
chunk's globals can redirect its recognized pipelines, exactly as replacing `buffer` would.
Scripts that do not trust each other must not share a writable global environment; under
`Runtime::sandbox_globals` the globals are read-only, and a script that assigns `__l3i_data`
shadows it in its own environment only.

## How it lowers

```text
source → surface frontend (csrc/surface_frontend.cpp, Luau's lexer)
       → pipeline plan (csrc/surface_syntax.cpp: Pipeline, Stage, Consumer)
       → fused scalar Luau text + source map        or    → prologue + one data plane call
       → stock Luau compiler / Analysis / @dream/luau
```

Every comprehension is planned as ordered stages feeding one consumer:

```text
Stage    = Generator { kind: Plain | Enumerate | Range | Zip | Slice } | Filter
Consumer = Materialize | Count | Sum | Min | Max | Any | All | Sink
```

One recursive emitter walks the plan: each generator evaluates its source once and opens a loop,
each filter opens an `if`, the consumer's step runs per accepted projection, and the closers
unwind. Because the walk is recursive a stage may emit the rest of the pipeline more than once: a
leading slice whose representation is only known at run time emits one specialized loop for
buffers and one for tables under a single dispatch, instead of testing the representation per
element (which was measured at 30 percent more instructions and fixed). The plan knows three facts
once: whether a lone leading generator is `sized`, so materialization can `table.create(n)`;
whether it can store at its loop index with no cursor; and otherwise that it needs one. Adding a
consumer is one reducer name in the frontend, one `Consumer` in the emitter, one node in
`@dream/luau`, tests and docs. No new recognizer, no new loop emitter.

Three facts about the generated text that are worth knowing:

- The prelude snapshots the builtins it uses into locals before any user declaration, so a script
  cannot capture them by shadowing `error` or `buffer`, and takes three to nine of the chunk's
  200 top-level local registers when a chunk contains any dialect construct. A chunk already at
  197 top-level locals compiles as stock Luau and fails with `Out of local registers` once it
  gains a comprehension. Scripts do not have 197 top-level locals; modules that generate one
  should know.
- Luau compiles at optimization level 2 and inlines a function called where it is written, so the
  wrapper around a pipeline costs neither `NEWCLOSURE` nor `DUPCLOSURE` and the entry function's
  only calls are `table.create` and the cold `error` path. A pipeline that uses its enclosing
  function's `...` is the exception: Luau will not inline a vararg wrapper, so that one costs a
  closure and a call.
- Generated names are salted when any source identifier equals or extends them, so
  `__l3i_comp_7_g0_src` in your code cannot be shadowed by the lowering's own locals.

Everything observable about lowering is pinned by `tests/lowering.rs`: generated text,
provenance segments, tooling sites and structural errors for every shape under the compile,
Analysis, Analysis-with-buffer-types and syntax policies. A refactor proves itself by leaving
those snapshots unchanged. An intentional change regenerates them and the diff is reviewed.

## Tooling sees your source, not the lowering

Tools work in the coordinates of the source you wrote. The frontend returns lowered text plus a
source map: copied ranges map affinely in byte offsets, and every synthetic range is attributed to
a deliberate original span.

| Generated work | Attributed to |
|---|---|
| Wrapper function and call | the whole comprehension, consumed `#` included |
| Source aliases, length and index machinery, generator bindings | the generator clause; copied tokens stay exact |
| Filter control structure | its predicate |
| Projection temporary, nil guard, count increment, result store | the projection |
| Loop exits and the final return | the closing bracket |
| Record pattern holder | the pattern; field declarations to their fields |
| Field read `holder.key` | the field's key, so a missing field is diagnosed at its name |

What that buys, concretely:

- **Analysis diagnostics** carry original spans, in required modules too. `Type 'number' does
  not have key 'missing'` on `[for x in values => x.missing]` points at `x.missing`, not at
  column 323 of a line you never wrote. Secondary references inside Luau's message prose (`to
  close 'function' at line 5`) are translated when the template is known; a line-only reference
  into a generated line that holds copies of several original lines is labelled `within a lowered
  comprehension (ambiguous source reference)` rather than guessed.
- **`Analysis::autocomplete`** takes original, zero-based **byte** coordinates. Translate editor
  columns before calling it. Generated bindings are hidden; your own bindings with similar names
  are not. Completion inside a pattern key or an empty field slot moves the cursor to the field's
  generated read, so Luau's property completion lists the destructured value's fields, typed,
  nested records included.
- **`analysis::parse`** exports a lowered AST whose locations are original. It is not a surface
  tree; for one, use `@dream/luau`, which defaults to the `l3i` dialect and shows
  `ExprComprehension`, `ExprSlice`, `ExprReduction`, `ExprSink`, `StatLocalPattern` and
  `PatternRecord` nodes with original spans and no generated name anywhere. `{ dialect = "luau" }`
  is strict stock Luau. The [dream.luau](@/docs/builtin-extensions.md#dream-luau) section lists
  the nodes.
- **Bytecode** embeds original debug lines, so runtime errors, `DebugScope::call_site`,
  tracebacks, breakpoints and coverage report your lines with no run-time map. Recompile cached
  bytecode to get them; old bytecode cannot be repaired by loading it.

Analysis keeps its internal AST and scopes in lowered coordinates and translates at the edges:
diagnostics out, cursors in. Changing Luau's scope and refinement machinery to flatter a
presentation layer would have been the wrong trade. Maps belong to the same module snapshot as the
frontend's AST; `mark_dirty` permits a reread and `clear` clears both. CRLF and UTF-8 are not
normalized.

### Unfinished code

Every prefix of a construct keeps its record and located errors, so tooling understands what you
are typing before you have finished:

```luau
[for
[for x in entities if
[for x in entities => x.
local {position: {x,
function f({x,
```

Analysis uses recovery-only scaffolding to expose the known bindings to Luau's checker: globals
complete in the enclosing scope, missing `in`, `=>` or `]` are suggested, typed member completion
survives after a dot in sources, filters and projections, and nullable refinement and outer
captures survive nested partial expressions. Errors caused solely by empty recovery holes are not
shown as yours. Compilation rejects every structural hole before bytecode: neither a missing
projection nor a tooling placeholder can execute.

Missing brackets synchronize at enclosing closers, commas, statement boundaries or EOF. Bare
postfix forms such as `values[for ...]` or `values.sum[for ...]` are invalid, not implicit calls;
tooling keeps them as erroneous indices and strict compilation rejects them. Recovery is
deterministic and bounded: nesting below 128, and a prefix-validation budget of 8,388,608 bytes
plus eight times the source size and 4,096 calls plus one per source byte, which grows with the
source so a module is never refused for its size, only for candidates nested so that the same text
is validated over and over. Fatal recovery still returns the `StatBlock` root with an error
statement, with less detail. CST round-tripping, formatting and incremental parsing are not
provided.

### Debugging

Runtime metadata has lines, not columns: enable line information (`debug_level >= 1`).
Optimization level 2 inlines and removes code, and mapping cannot recreate a breakpoint site the
compiler deleted; at lower levels the generated wrappers and locals may stay visible. At debug
level 2 Luau lists every register-allocated local with no way to mark one internal, so holders
appear under their generated names next to the bound names, holding exactly the record they
destructure. Hiding them would mean rewriting serialized debug information or filtering by name in
the host, which could hide a user's own local; neither is done. Coverage attributes synthetic
instructions to their chosen anchors: original-line attribution, not a comprehension coverage
model.

Because the generated field reads share their source line, a same-line warning Luau reports once
per line can be spent on a generated read and dropped: `local {a} = t print(a)` gets no
`SameLineStatement` lint where `local a = t.a print(a)` would.

## What it costs

Executed VM instructions, deterministic single-step counts from `cargo test
executed_instruction_counts -- --nocapture`, interpreter, default compilation:

| Input size | Dense l3i / checked loop / unchecked loop | Filtered l3i / checked loop | Fused count l3i / checked loop | Fused sum l3i / checked loop |
|---:|---:|---:|---:|---:|
| 0 | 11 / 10 / 10 | 12 / 11 | 8 / 7 | 8 / 7 |
| 1 | 16 / 15 / 14 | 16 / 15 | 12 / 11 | 12 / 11 |
| 32 | 171 / 170 / 138 | 204 / 203 | 184 / 183 | 184 / 183 |
| 4,096 | 20,491 / 20,490 / 16,394 | 24,588 / 24,587 | 22,536 / 22,535 | 22,536 / 22,535 |

Every lowering family adds exactly one setup instruction to the same-contract loop at every size.
The dense form's one extra instruction per element over the unchecked loop is the nil guard,
which is the contract, not overhead. Retired CPU instructions per call from
`cargo bench -- comprehension` tell the same story: dense, filtered, count and sum fusion
within 0.03 percent of the handwritten loop; `enumerate` and the zips 25 to 12 percent over
deliberately unchecked baselines, the difference being the guard and the index binding; a
sink 1,140 instructions over a handwritten destination-reuse loop per call of 4,096 elements, all
of it the one-time destination check and the per-source aliasing check; `min` and `any` within 60
instructions of their loops. The bench sandboxes its runtime after setup so builtin fast calls
fire as they do in a host; absolute times quoted anywhere from earlier runs are pessimistic by
the builtin-call cost and the ratios hold either way.

Record patterns, retired instructions per call over 4,096 records, `cargo bench -- patterns`,
l3i against the handwritten extraction:

| Case | Interpreter | Native (`jit`) |
|---|---:|---:|
| Two fields | 1,422,344 / 1,422,344 | 324,478 / 324,478 |
| Three fields | 1,864,712 / 1,864,712 | 414,590 / 414,590 |
| Nested record | 1,750,024 / 1,750,024 | 406,398 / 406,398 |
| Two fields through `__index` functions | 6,493,192 / 6,493,192 | 6,157,181 / 6,157,181 |
| Typed parameter, called per record | 1,598,536 / 1,598,536 | 316,371 / 316,371 |
| `sum[for {x, y} in ...]` | 1,565,734 / 1,565,704 | 349,055 / 349,053 |
| Filtered `sum`, half accepted | 1,764,390 / 1,764,360 | 377,727 / 377,725 |
| Nested `#[for {position: {y}} ...]` | 1,696,806 / 1,696,776 | 361,343 / 361,341 |

Declarations, parameters and metatable-backed records are instruction for instruction the
handwritten code. Generators retire 30 (interpreter) or 2 (native) more per call of 4,096
records: the comprehension's setup. A holder initialized from a local costs nothing, since Luau
aliases a local initialized from an unassigned local. Disassembly introduces no `NEWTABLE`,
`DUPTABLE`, `NEWCLOSURE`, `DUPCLOSURE` or `CALL`. With `jit`, `tests/patterns.rs` runs its 43
semantic cases again compiled natively and they agree.

**Compile cost.** Against stock `luau_compile` at the same options, on 16,000 lines of plain
Luau that never uses the dialect, the frontend adds about 17 percent when the file indexes inside
loops and about 20 percent when it has dotted or method calls inside brackets; a file without a
`[` pays nothing, because nothing is lexed. Of the frontend's own time Luau's lexer is about two
thirds, and the remaining fix would be not lexing at all, which no exact textual test for a slice
allows. A module dense with patterns compiles in about 1.8 times the instructions of its
handwritten twin: lexing, building the lowered text and map, and remapping the many locations the
holders shift. `cargo bench -- patterns_compile` tracks it;
`L3I_PROFILE_CASE=<label>` runs one case for a profiler.

Wall-clock times are secondary everywhere on this page. Sequential Criterion runs have reversed
the order of two cases with identical hot loops. Instruction counts and disassembly explain the
work; timings confirm that nothing pathological happened.

## What is deliberately not here

- **Package-owned AST specialization**: a package handed static metadata generating and caching a
  specialized Luau function through a hygienic AST API. Deferred, because nothing in the evidence
  needs it. Every comprehension-shaped loop already lowers to within a setup instruction of the
  best handwritten loop, generic bulk work is a native call that code generation cannot beat, and
  run-time query shapes compose from selections. It is re-opened when a neutral fixture shows a
  workload that is not a pipeline over one source, not a bulk data operation, and loses measurably
  to a hand-specialized function. None of the campaign's did.
- A bare `zip`, a seeded `min`, defaults or rest in patterns, structural matching, `=` as rename.
  Each is a decision that can still be made later; none can be unmade.
- Any claim that `t[lo:hi()]` is a slice. See above, and parenthesize.
- Transparent `any`/`all`. They short-circuit, and that is in the contract, not hidden behind it.
