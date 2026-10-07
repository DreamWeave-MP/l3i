# Comprehensions: integration and instruction evidence

The comprehension baseline below was validated on the actual L3i checkout, 2026-10-04,
starting from the already-applied `88e0636` dense/non-nil prototype. Luau remains stock 0.740,
submodule commit `c0e346edd89066b44dca174c9f54ce84c746a540`. The `sum[for ...]` reducer
documented at the end has since passed every gate; the Phase One closure section records the
current numbers for every lowering family.

## Contract

```luau
[for x in xs => x * 2]
[for e in entities if e.active => e.id]
[for x in xs if p(x) if q(x) for y in children(x) => project(x, y)]
[for row in rows => [for x in row => x * 2]]
#[for x in xs if accept(x) => effect(x)]
```

Results are dense, 1-based arrays. Every accepted projection executes exactly once;
nil raises `L3i comprehension projection produced nil; filter nil explicitly`.
False is a valid projection. Filters have ordinary Luau truthiness: only nil and
false are falsey, including when a filter returns a number, string, or table.

Sources execute once at their nesting level. Filters execute left-to-right and
short-circuit before inner generators/projection. The source length is captured
on entry to each generator, like a numeric handwritten loop. This is a dense,
indexable-sequence operation, not generic iteration; arbitrary sparse table
boundaries still have ordinary Luau `#` behavior.

Direct unary `#[for ...]` eliminates the **result** table, not source or projection
allocations. It retains projection effects, failures, and nil checks. Parentheses
or comments between `#` and the comprehension can prevent this lexical optimization;
the unfused expression remains valid. No purity assumptions are made.

## Fixes and regression coverage

- The original allocation test accidentally forbade its own input table literal.
  The corrected fixture supplies the source externally and checks for no result
  constructor, stores, or wrapper closure, while retaining multiply/nil-check opcodes.
- Luau's new solver inferred `{unknown}` from `table.create(n)` with no fill value.
  The generated allocation now uses `table.create(n) :: typeof({})`: the type of a
  fresh unsealed builder lets indexed writes infer the projection type. This is
  neither `any` nor a second typechecker. `:: {}` is not equivalent: it is sealed.
  The literal inside `typeof` is not executed. The assertion produces identical
  bytecode with and without it at optimization levels 0, 1, and 2, debug level 0.
- Clause trimming could remove a line comment's terminating newline and let the
  comment consume synthetic code. Literal-aware scanning now restores that boundary.
  Runtime and standalone tests cover source/filter/projection comments, count
  fusion, and multiple generators.
- Unary-length recognition now verifies that the scanner encountered `#` as code.
  A hash at the end of a preceding line comment previously triggered false fusion
  and could swallow the entire result expression. An intervening comment after a
  real hash safely falls back to materialization.
- Hostile event logs prove dependent-source placement, filter order, rejected
  outer iterations, and exactly-once projections for materialized and fused forms.
  Paired failure tests prove execution stops at the same element on nil or errors.
- Hygiene tests cover legacy temporary-like names, generator-variable shadowing,
  recursively embedded comprehensions, and forced collisions with the actual
  generated offset and its first salted alternative.

## Compiler and executed VM instructions

The normal L3i policy enables `LuauCompileIifeInline` and optimization level 2.
Disassembly proves the simple wrapper emits neither `NEWCLOSURE` nor `DUPCLOSURE`.
The dense entry function's only calls are `table.create` and the cold `error` path.
Luau still emits an unused wrapper prototype in the listing; that is not a runtime
closure or call. Expression-result register placement adds one setup `MOVE`.

The deterministic test uses Luau's single-step callbacks on a host-driven coroutine,
with default interpreter compilation. Counts are executed VM instructions, not AUX
words, native C implementation instructions, or CPU instructions. Setup of the input
and loading of the function occur before stepping. The cold nil-error path is not
taken in these numeric fixtures.

| Input size | Dense L3i / checked loop / unchecked loop | Filtered L3i / checked loop | Fused count L3i / checked loop | Materialized count |
|---:|---:|---:|---:|---:|
| 0 | 11 / 10 / 10 | 12 / 11 | 8 / 7 | 13 |
| 1 | 16 / 15 / 14 | 16 / 15 | 12 / 11 | 17 |
| 32 | 171 / 170 / 138 | 204 / 203 | 184 / 183 | 205 |
| 4,096 | 20,491 / 20,490 / 16,394 | 24,588 / 24,587 | 22,536 / 22,535 | 24,589 |

**There is no per-element wrapper overhead.** Dense, filtered, and count lowering
each add exactly one setup instruction to the same-contract loop at every tested
size. Dense nil checking adds one executed VM instruction per accepted projection.
It is intentional semantic cost, not grounds for deleting the guard.

Reproduce the evidence:

```bash
cargo test executed_instruction_counts -- --nocapture
cargo test dense_comprehension_bytecode -- --nocapture
```

## Retired CPU instructions and secondary timings

Measured with `cargo bench --bench comprehension`, default features (interpreter),
Intel Core i7-10870H, Linux x86-64, rustc 1.99.0. Each invocation processes 4,096
integers; the even predicate accepts 2,048. CPU counts use one user-space hardware
instruction counter via the existing `perf_event_open` shim, seven rounds of 128
calls. The table reports the minimum round total divided by 128, rounded down.
The harness also prints median/max; it reports unavailable counters rather than
inventing measurements on unsupported systems or restricted hosts.

| Case | CPU instructions / call | Criterion estimate, µs (95% CI) |
|---|---:|---:|
| Dense L3i, nil-checked | 1,082,906 | 82.007 (81.711–82.315) |
| Dense handwritten, unchecked | 939,523 | 72.085 (71.271–73.195) |
| Dense handwritten, nil-checked | 1,082,856 | 88.792 (88.388–89.200) |
| Filtered L3i, nil-checked | 1,318,897 | 153.45 (152.74–154.27) |
| Filtered handwritten, unchecked | 1,246,758 | 145.61 (144.68–146.54) |
| Filtered handwritten, nil-checked | 1,318,449 | 150.95 (150.38–151.58) |
| Growing `table.insert`, unchecked | 2,561,299 | 252.18 (247.89–256.33) |
| Fused length L3i, nil-checked | 1,160,400 | 133.78 (133.11–134.55) |
| Materialized L3i, then length | 1,318,484 | 150.03 (149.19–150.94) |
| Handwritten fused count, nil-checked | 1,160,370 | 138.26 (137.64–139.02) |
| Callback L3i fusion, nil-checked | 3,120,937 | 273.41 (272.26–274.55) |
| Callback handwritten fusion, nil-checked | 3,120,873 | 260.01 (258.85–261.16) |
| Callback fused helper, nil-checked | 3,121,280 | 267.39 (266.00–269.24) |
| Two-pass filter then map, nil-checked | 3,431,527 | 305.46 (302.69–308.46) |

Same-contract CPU instruction deltas are below 0.04% in these cases. Count fusion
reduces CPU instructions by about 12% versus materialization, and emits no result
constructor or stores. Two-pass callbacks cost about 10% more instructions than
L3i fusion. An already-fused callback helper is close to L3i; not every abstraction
deserves a claimed speedup.

Timings are secondary: the earlier run measured dense L3i at 85.711 µs versus
79.201 µs handwritten nil-checked, reversing the relative result above despite
equivalent hot loops. Sequential timing confidence intervals do not cover frequency,
thermal, scheduling, or other between-case drift. These runs do **not** prove general
wall-clock superiority. Instruction counts and disassembly explain the actual work.

Each candidate gets its own VM. The harness validates every output outside timing,
loads functions before leasing the root stack, drops returned table pins inside the
measurement, and collects fully before each measurement batch/CPU round. Incremental
GC and the host invocation boundary remain included. All 14 cases kept stack height
0 and identical before/after full-GC VM byte counts across 128 calls. This checks
retention, not total allocations. The result-allocation claim comes from bytecode.
The `table.insert` case also differs in capacity growth. Two-pass callback ordering
is equivalent only for these pure benchmark callbacks, not arbitrary effects.

## Analysis and diagnostics

Both old and default new solvers are tested through autocomplete of **unannotated
user bindings**, not only assignment acceptance:

- Numeric and filtered entity-id projections: `{number}`.
- Record projection: `{{ label: string, value: number }}`.
- Nullable generator elements and record fields filtered with `~= nil`: `{number}`.
- Nullable `Foo` elements projected to `x.name` after filtering: `{string}`.
- Nested comprehensions: `{{number}}`; multiple numeric generators: `{number}`.
- Removing the nullable filters still fails arithmetic typechecking. The later
  projection guard cannot refine an earlier invalid arithmetic operand.

**Original-source mapping is now integrated.** The shared lowerer records exact
copied spans and deliberate synthetic anchors. Analysis diagnostics, autocomplete
requests, standalone AST JSON locations, and compiled runtime debug lines use
original coordinates. See [the mapping contract and validation](SOURCE_MAPPING.md).
With zero-based byte columns and end-exclusive positions:

```luau
--!strict
local values: { number } = { 1 }
local projected = [for x in values => x.missing]
return projected
```

The diagnostic (`Type 'number' does not have key 'missing'`) now reports line 2,
columns 37–46, rather than generated columns 323–332. Same-line trailing errors
also use original positions. Regression tests cover nested/multiline expressions,
UTF-8 byte columns, CRLF, source snapshots, and cursor requests inside comprehensions.
Known line-only references embedded in diagnostic prose are mapped when unambiguous
and explicitly qualified when lowering has erased that precision.

The old recognizer has been deleted and replaced by one stock-token surface frontend.
Compilation is strict; Analysis and `@dream/luau` use structural recovery. Ordinary
conditionals/function literals belong to Luau, and comprehensions inside interpolation
expressions are recognized. `@dream/luau` now defaults to genuine source nodes rather
than generated wrappers. See [the surface tooling contract](SURFACE_TOOLING.md).
Remaining boundaries include textual lowering hygiene and bounded/fatal parser recovery.
Generated allocation and nil-error operations are snapshotted before user declarations,
so local bindings cannot capture them; copied user expressions retain ordinary lexical lookup.

## Baseline validation and next boundary

These counts are from the original integration/benchmark pass. The mapping follow-up's
current validation and remaining tooling boundaries are recorded in [SOURCE_MAPPING.md](SOURCE_MAPPING.md).

- `cargo test`: PASS (79 unit + 140 integration tests).
- `cargo test --features analysis`: PASS (79 + 148).
- `cargo bench --bench comprehension`: PASS (14 validated cases).
- Workspace default/all-feature tests: PASS (all features: 88 + 195).
- Default and Analysis strict Clippy: PASS.
- All-feature, all-target strict pedantic Clippy and formatting: PASS.
- GCC standalone scanner tests, C++17, `-Wall -Wextra -Werror -pedantic`: PASS.
- Clang standalone scanner tests with the same warning policy and ASan/UBSan: PASS.

Keep the generic surface-syntax seam. No Luau modifications, new typechecker, or
reducer semantics were necessary. Future reducer fusion must identify canonical
operations, define empty-input behavior, and preserve evaluation. In particular,
short-circuiting `any`/`all` cannot transparently replace an eager comprehension
that would evaluate later effects or encounter nil: that requires an explicit
different contract or trustworthy effect proof.

Allocation elimination is authorized. Observable evaluation elimination is not.

## Phase Two: numeric range generators

The first Phase Two construct is a JSL-owned numeric range in comprehension generator
position:

```luau
[for i in range(first, last) => project(i)]
[for i in range(first, last, step) if accept(i) => project(i)]
```

This initial surface accepts exactly two or three arguments. Bounds are inclusive and
the omitted step is `1`, matching an ordinary Luau numeric `for`. Each argument is
evaluated exactly once, from left to right, before that generator begins. A dependent
range is therefore evaluated once per applicable outer iteration. The arguments and
loop variable retain stock Luau numeric-for typing and runtime conversion behavior.

A zero step is a JSL error rather than a non-progressing loop. It raises before entering
the loop. Direction and empty-range behavior otherwise match stock numeric `for`: a positive
step with `first > last`, or a negative step with `first < last`, executes no iterations.

`range` is intrinsic only as the direct generator-source spelling above. It does not
consult a local/global named `range`; member/method calls and calls nested inside another
expression remain ordinary Luau. The first milestone does not introduce a range object or
make `range(...)` meaningful outside a comprehension generator.

Lowering must emit a numeric `for`, with no range allocation, iterator closure, coroutine,
generic-for protocol, or callback dispatch. Materialized, length-fused, and sum-fused
consumers retain the existing dense/non-nil/eager projection contract. Source nodes,
recovery, diagnostics, autocomplete, and type analysis continue to come from the canonical
frontend and refer to the original range arguments.

## Phase Two: indexed `enumerate` and explicit `zip`

Dense array generators may expose their index without allocating pairs:

```luau
[for i, value in enumerate(values) => i + value]
```

`enumerate(source)` evaluates `source` once, captures its length once, and lowers to an
indexed numeric loop from `1` through `#source`. The index and element bindings are numbers
and values respectively; no iterator, closure, tuple, or generic-for dispatch is created.

Zipping is intentionally split into two named policies:

```luau
[for a, b in zipShortest(xs, ys) => a + b]
[for a, b in zipStrict(xs, ys) => a + b]
```

Both forms evaluate every source once, left to right, and load aligned elements directly by
index. `zipShortest` traverses through the shortest captured length. `zipStrict` checks all
captured lengths before entering the loop and raises `JSL zipStrict inputs must have equal lengths`
on mismatch. Both accept two or more sources and require exactly one binding per source.
There is no bare `zip` intrinsic, so unequal-length behavior cannot be selected accidentally.

These forms remain eager and preserve the ordinary comprehension nil guard. Length and sum
consumers fuse the same traversal and do not allocate a slice, pair table, iterator, or result
table. Direct source nodes (`ExprEnumerate` and `ExprZip`) retain original argument spans for
tooling and diagnostics.

## Phase Two: inclusive dense-table slices

Dense-table slices are expressions and direct comprehension generator sources:

```luau
local part = values[first:last]
[for x in values[first:last] => project(x)]
sum[for x in values[first:last] if accept(x) => project(x)]
```

Bounds are 1-based, inclusive, finite integers. The source must be a table. The source, first
bound, and last bound are evaluated once, left to right, before validation and before measuring
the source. Invalid sources and bounds fail consistently in materialized and fused forms. Bounds
are clamped to the source's dense extent; an interval with `last < first` is empty. The initial
form is intentionally unstepped. Standalone table slices allocate an exactly sized dense result
and bulk-copy with a snapshotted `table.move`. Standalone buffer slices preserve the same 1-based
inclusive surface bounds, translate them to zero-based byte offsets, and bulk-copy with
`buffer.copy`. They do not consult user bindings named `table` or `buffer`. Direct table and buffer
comprehension generators instead traverse selected indices directly; buffers read each selected
byte with `buffer.readu8`. Analysis probes the source with stock Luau types and rechecks a
representation-specialized lowering, so table element types stay precise while buffer bindings are
numbers. Fused length and sum consumers allocate neither a slice nor a comprehension result.
Materialized comprehension consumers compact selected elements into their normal dense result,
preserving projection order and the non-nil guard.

The canonical frontend exports an `ExprSlice` node with original source and bound spans for both
standalone and generator slices. Buffer specialization in dependent/nested generator positions and
string representation-directed copies remain future work. Stock Luau indexing wins when the contents
are already a valid colon method call, so ambiguous bounds use parentheses: `xs[(first()):(last())]`.

## Post-baseline JSL reducer experiment: `sum[for ...]`

The next surface experiment builds directly on the hardened comprehension lowering without
recognizing or rebinding a global helper function:

```luau
local total = sum[for x in xs if x.active => x.mass]
```

`sum` is JSL-owned syntax when it consumes a comprehension. It lowers to a numeric accumulator
initialized to `0`, preserving generator/filter order, exactly-once projection evaluation, errors,
and the existing non-nil projection guard. No result table is created. Empty input returns `0`.
Whitespace and comments may separate `sum` from the comprehension.

Conceptually:

```luau
local total = 0
for i = 1, #xs do
    local x = xs[i]
    if x.active then
        local value = x.mass
        if value == nil then
            error("L3i comprehension projection produced nil; filter nil explicitly")
        end
        total += value
    end
end
```

This is deliberately a numeric reducer. Stock Luau typing/arithmetic remains authoritative; L3i
does not add a second type system or generic additive identity protocol.

The C++ surface pass and source map are validated locally under GCC and Clang warnings-as-errors,
Clang ASan/UBSan, and 250,000 randomized scanner inputs. Lowered output was also executed through
an embedding Luau runtime: filtered, empty, nested and comment-separated forms produced the expected
results. Bytecode disassembly of the lowered filtered sum has no closure construction or result
table and differs from the equivalent handwritten loop by the same kind of one-time setup move
already measured for comprehension expressions.

The Rust runtime, disassembly, executed-instruction and Criterion cases pass on the real checkout
(see the closure below). The invariant holds at every tested size:

```text
sum[for ...] executed VM instructions == same-contract handwritten fused sum + one setup instruction
```

The benchmark also compares fused sum against materializing the comprehension and traversing the
result a second time. Treat any wall-clock claim as secondary to disassembly/instruction evidence,
just as with the original comprehension campaign.

## Reducers

Five reducers consume a comprehension. Each is JSL syntax: a Name immediately before `[for`,
with trivia allowed between, never preceded by `.` or `:`, and never resolved against a binding
of that name. Every reducer lowers to one traversal of the pipeline with no result table, no
iterator and no closure; the pipeline's generator, filter and projection semantics (order,
exactly-once projection, Luau truthiness filters, the non-nil projection guard) are unchanged.

| Reducer | Result | Empty pipeline | Per accepted projection |
|---|---|---|---|
| `sum[...]` | number | `0` | `total += value` |
| `min[...]` | first least projection | error `JSL min reducer received no elements` | `if acc == nil or value < acc then acc = value end` |
| `max[...]` | first greatest projection | error `JSL max reducer received no elements` | `if acc == nil or value > acc then acc = value end` |
| `any[...]` | boolean | `false` | `if value then return true end` |
| `all[...]` | boolean | `true` | `if not value then return false end` |

`min` and `max` order with Luau `<` and `>`: numbers, strings, and `__lt`/`__le` metamethods
work; mixed or unordered operands raise the ordinary comparison error. A NaN that arrives first
stays (nothing compares below or above it); later NaNs never replace. Ties keep the first. The
empty case is an error rather than `nil` because the language has no silent nil result; a
seeded form can be added as a distinct spelling if a consumer needs one. Analysis types the
result as the projection's element type on both solvers: the epilogue is an if-expression whose
error branch has type `never`.

`any` and `all` are **language-level short-circuit reducers**, not transparent allocation
removal. They stop at the first deciding projection, which means later generators, filters and
projections do not execute. An eager comprehension passed to a function would have evaluated
them all; code that relies on those effects must not use `any`/`all`. The early exit leaves
every nesting level at once. The projection must still not be `nil`; filter nil explicitly.

Internally all five, with `#[...]` and plain materialization, are consumers of one pipeline
plan, so every generator kind and nesting combination lowers through the same emitter.

## Sinks

```luau
into(out)[for x in xs if p(x) => f(x)]
local filled = into(scratch.rows)[for row in rows[a:b] => project(row)]
```

`into(destination)` immediately before `[for` (trivia allowed, never after `.` or `:`, never a
binding named `into`) makes the pipeline fill a caller-owned table in place instead of
allocating a result. The expression evaluates to the destination. Semantics:

- The destination is evaluated exactly once, before any source, and must be a table
  (`JSL sink destination must be a table`). Its length is captured at that point.
- Replace, not append: accepted projections are written to `1..n` in pipeline order, then
  entries `n+1..previous length` are set to `nil`, so the destination is dense afterwards.
  An empty pipeline leaves an empty table.
- The non-nil projection rule and evaluation order are those of the comprehension.
- The destination may not be any generator's source table
  (`JSL sink destination must not be a pipeline source`), checked with `rawequal` once per
  source evaluation; the pipeline would otherwise overwrite elements it has yet to read.
  Reading the destination inside filters or the projection is allowed and unguarded.
- A sink may stand as a statement; the other pipeline forms are expressions only.

Lowering stores straight into the destination with the running count as index: no result
table, no `table.move`, no closure. Analysis types the expression as the destination and
checks each projection against the destination's element type.

### Buffer sinks

```luau
local out, written = into(samples, "f32")[for x in xs if x > 0 => x * 2]
into(bodies, "f32@16", 8)[for i, v in enumerate(velocities) => v * i]   -- one field per record
```

`into(buffer, "kind"[, offset])` writes each accepted projection into the buffer with the
`buffer.write<kind>` the literal names, `offset + n × stride` bytes in, where the literal is an
element kind or a `kind@stride` layout (the data plane's spelling: `"f32@16"` fills the f32
field of 16-byte records). The kind must be a string literal so the write is a known fastcall;
the offset defaults to 0 and must be a non-negative integer. The expression evaluates to the
buffer and the count written. Semantics:

- Destination, then offset, then sources, each evaluated once; the destination must be a buffer.
- Capacity is explicit: a projection that would not fit raises `JSL sink buffer destination is
  full`; nothing is silently truncated and bytes past the written count are untouched.
- The projection must be a number: the buffer library's own conversion and error apply (an
  integer kind truncates and wraps as `buffer.write<kind>` does; a non-number is `number
  expected`).
- The destination may not be a slice generator's buffer, as for table sinks.
- Analysis types the first result as `buffer`, the second as `number`, and checks the
  projection against `number` through the write call itself.


## Recognized data pipelines

When the `dream.data` extension is installed (feature `data`, see [DATA_PLANE.md](DATA_PLANE.md)),
compiled chunks snapshot its alias global `__l3i_data` in the prelude, and the buffer branch of
a recognized pipeline calls the data plane once instead of looping:

```luau
sum[for x in buf[a:b] => x]                 -- U8:sum(buf, a - 1, n)
#[for x in buf[a:b] => x]                   -- n
#[for x in buf[a:b] if x > 127 => x]        -- U8:countGt(buf, a - 1, n, 127)
min[for x in buf[a:b] => x]                 -- empty check, then U8:min(buf, a - 1, n)
max[for x in buf[a:b] => x]
```

`U8` is the chunk's `dream_data_Kind_u8` receiver for bytes (`__l3i_data.u8()`, annotated in the
prelude), so with the `jit` feature each recognized call compiles to the receiver's
unrolled native loop, measured faster than the bound call and than Luau's own native loop at
every size (DATA_PLANE.md §2.4); without `jit` the bound method runs once per pipeline. Every
path computes the number the scalar loop computes, bit for bit.

Recognition is deliberately narrow, because errors and evaluation order are effects: a lone
leading slice generator, a projection that is exactly the binding, and either no filter or one
`binding <op> threshold` filter on a count, where the threshold is a number literal or a name
the compiler has verified to be a local, upvalue or parameter declared outside the
comprehension (the compile path lowers once, parses, resolves the name, and lowers again). A
global threshold stays a scalar loop: reading it once rather than per element could be
observable. A verified name is still guarded at run time, `typeof(limit) == "number"`, so a
non-number threshold takes the scalar loop and raises exactly its comparison error. The source, both bounds, every type check and the
bounds normalization still run in the ordinary JSL prologue, so what raises, and when, is
identical. The byte values are summed in `f64` in element order, exactly as the loop adds
`buffer.readu8` results, so results are bit-identical; `min`/`max` keep the loop's `<`/`>`
rules and raise the same empty-pipeline error before the call. Without the extension the same
chunk takes its scalar loop. Table sources never take this path; Analysis and tooling never see
it. Anything else (an arbitrary projection, a filter against a variable, `any`/`all`, a sum
with a filter) stays a fused scalar loop.

### Sink and reducer evidence

`cargo bench --bench comprehension -- "comprehension_(sinks|reducers)"`, same harness and
machine class as the tables below (4,096 integers, the even half accepted; `any` decides at the
halfway element):

| Case | CPU instructions / call | µs |
|---|---:|---:|
| Sink L3i, nil-checked | 1,283,100 | 142.5 |
| Handwritten destination-reuse loop, nil-checked | 1,281,960 | 141.4 |
| Materialize, then `table.move` into the destination | 1,345,658 | 157.3 |
| `min` L3i, nil-checked | 1,246,463 | 154.2 |
| Handwritten `min` loop, nil-checked | 1,246,403 | 152.9 |
| Materialize, then `math.min(table.unpack(...))` | 1,476,036 | 155.0 |
| `any` L3i, nil-checked | 566,738 | 52.9 |
| Handwritten `any` loop, nil-checked | 566,708 | 52.6 |

The sink costs 1,140 instructions per call over the handwritten reuse loop: the one-time
destination type check and the per-source `rawequal` aliasing check, nothing per element. `min`
and `any` are within 60 instructions of their loops.

## Phase One closure, 2026-10-07

Every gate was rerun on the checkout at `e3cc3a4` plus the closure fixes: `cargo test` (73 unit +
169 integration), `--features analysis` (73 + 202), `--workspace --all-features` (82 + 269),
all-target all-feature pedantic Clippy, rustfmt, all-feature rustdoc, the standalone canonical
frontend and lowering suites under GCC-style warnings-as-errors and Clang ASan/UBSan, and the
compiler/disassembler allocation-failure probe. Two drifts were found and fixed: three Clippy
sites and rustfmt drift in the slice test suites, and one stale standalone assertion that
predated structural recovery of `xs[:n]` bounds.

Executed VM instructions (deterministic single-step counts) are unchanged from the table above;
the sum consumer matches the fused count consumer exactly: 8 / 12 / 184 / 22,536 at 0 / 1 / 32 /
4,096 items against 7 / 11 / 183 / 22,535 for the same-contract handwritten loop.

### Lowering is now one pipeline plan

Every comprehension is planned as ordered stages (generators over a source kind: plain,
enumerate, range, zip, slice; and filters) feeding one consumer (materialize, count, sum), and
one emitter walks that plan. The five parallel emitters for single-range, single-slice,
single-zip, single-plain and nested shapes are gone. The refactor was proven by lowering
snapshots: `tests/lowering.rs` pins generated text, provenance segments, tooling sites and
structural errors for 76 surface shapes under the compile, Analysis, Analysis-with-buffer-types
and syntax-tooling policies, and the snapshots were byte-identical before and after. The only
later change is deliberate: synthetic scaffolding is now attributed to its own clause for every
source kind (numeric ranges and slices previously inherited a neighbouring anchor), which moved
provenance segments and no generated text.

### Retired CPU instructions and timings, all families

Same harness and machine class as the table above (i7-10870H, interpreter, 4,096 integers, seven
rounds of 128 calls; the minimum round is reported). Criterion estimates are the midpoint of the
95% interval.

| Case | CPU instructions / call | µs |
|---|---:|---:|
| Dense L3i, nil-checked | 1,082,592 | 77.6 |
| Dense handwritten, unchecked | 939,531 | 65.0 |
| Dense handwritten, nil-checked | 1,082,901 | 82.6 |
| Filtered L3i, nil-checked | 1,318,120 | 144.9 |
| Filtered handwritten, unchecked | 1,246,710 | 130.7 |
| Filtered handwritten, nil-checked | 1,318,402 | 143.6 |
| Growing `table.insert`, unchecked | 2,563,589 | 229.8 |
| Fused length L3i | 1,160,402 | 126.8 |
| Materialized L3i, then length | 1,317,984 | 144.0 |
| Handwritten fused count | 1,160,372 | 130.9 |
| Fused sum L3i | 1,166,546 | 129.8 |
| Handwritten fused sum | 1,166,516 | 130.8 |
| Materialized comprehension, then sum | 1,657,394 | 170.5 |
| Callback L3i fusion | 3,120,601 | 270.9 |
| Callback handwritten fusion | 3,120,875 | 297.1 |
| Callback fused helper | 3,121,272 | 257.8 |
| Two-pass filter then map | 3,431,503 | 277.4 |
| `enumerate` L3i, nil-checked | 1,197,299 | 85.2 |
| `enumerate` handwritten, unchecked | 931,327 | 65.2 |
| `zipShortest` L3i, nil-checked | 1,287,762 | 94.7 |
| Zip handwritten, unchecked | 1,145,223 | 85.3 |
| Standalone table slice L3i (`table.move`) | 40,527 | 3.92 |
| Handwritten `table.move` | 38,582 | 3.85 |
| Handwritten scalar slice copy | 459,932 | 33.2 |
| Fused slice sum L3i | 519,730 | 47.0 |
| Handwritten fused checked slice sum | 401,202 | 32.2 |
| Materialized slice, then checked sum | 437,672 | 36.9 |

Dense, filtered, count and sum fusion remain within 0.03% of the same-contract handwritten loop.
The `enumerate` and zip baselines are deliberately unchecked loops, so their gap is the non-nil
guard plus the index binding, not wrapper overhead. The slice rows expose a real regression: the
fused slice sum executes 30% more CPU instructions than the handwritten range reduction and more
than materializing the slice first. The cause is in the compile-time lowering, which has no type
information and dispatches on the source representation inside the loop (`if kind == "buffer"
then buffer.readu8(...) else src[i]`) for every element.

Hoisting that dispatch fixed it. The compile-time lowering now tests the representation once
and emits one specialized loop per representation (the rest of the pipeline is emitted under
each; Analysis and tooling, which have types or no dispatch, are unchanged). Remeasured on the
same harness:

| Case | CPU instructions / call | µs |
|---|---:|---:|
| Fused slice sum L3i | 401,004 | 32.4 |
| Handwritten fused checked slice sum | 401,202 | 34.3 |
| Materialized slice, then checked sum | 437,693 | 38.3 |

The fused form is now at parity with the handwritten range reduction (198 fewer instructions:
the handwritten baseline recomputes `typeof` through a global lookup) and 8% below materializing
first, with the slice allocation gone.

