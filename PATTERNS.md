# Record binding patterns

A structured value can introduce a structured set of lexical bindings. JSL has one record
pattern model, used in exactly three places: local declarations, function parameters, and
comprehension generator bindings. Luau 0.740 is unmodified; patterns lower to ordinary locals
and ordinary indexing in the canonical surface frontend, like every other JSL construct.

```luau
local {x, y, z} = position
local {health: hp, name} = entity
local {transform: {position: pos, rotation}, id}: Entity = entity

function move({x, y, z}: Vec3, dt: number)
    return x + dt, y + dt, z + dt
end

local ids = [for {id, active, position: {z}} in entities if active and z > 0 => id]
```

## Grammar

```text
BindingPattern := Identifier | RecordPattern
RecordPattern  := "{" [RecordField {"," RecordField} [","]] "}"
RecordField    := Identifier | Identifier ":" BindingPattern
```

- `{x}` binds field `x` to a local `x`; `{x: px}` renames it; `{position: {x, y}}` nests,
  recursively, to any depth below the frontend's nesting limit (128).
- `{}` is legal: it evaluates its source and reads nothing. A trailing comma is legal.
- `field: binding` renames. `=` is never a rename; it stays free for a later nil-default form.
- Keys are Luau names. A key that is not a name cannot be written.

A declaration is exactly `local RecordPattern [":" Type] "=" Expression`: one pattern and one
value. A parameter is `RecordPattern [":" Type]` in any position of any function's parameter
list (`function`, `local function`, function expressions, `function T:m(...)` with its implicit
`self`). A generator binding slot is a name or a record pattern, so `for {id} in xs`,
`for i, {v} in enumerate(xs)` and `for {a}, {b} in zipStrict(xs, ys)` all work.

Rejected with a diagnostic at the offending token, one diagnostic per mistake (recovery
resynchronizes at the record's next `,` or `}`):

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

Duplicates are checked across one binding list: one declaration's pattern, one function's whole
parameter list (plain names included), one generator's slots. Ordinary duplicate parameter names
without a pattern remain Luau's business. Assignment into existing bindings (`{x, y} = p`),
structural matching, per-field annotations, defaults, rest, computed keys and tuple
destructuring are deliberately not part of the language.

## Semantics

A pattern consumes exactly one value. `local {x, y} = f()` destructures the first value `f`
returns; multiple returns are never read as fields. It behaves exactly as:

```luau
local source = f()
local x = source.x
local y = source.y
```

- The source is evaluated once, in the enclosing scope, before any binding of its pattern exists:
  `local x = 10 local {x, y} = {x = x + 1, y = x}` gives `11, 10`.
- Fields are read in source order, depth first: `local {transform: {position: {x, y}}, id} = e`
  reads `e.transform`, `transform.position`, `position.x`, `position.y`, `e.id`.
- Reads are ordinary Luau indexing: `__index` metamethods run, vectors (`{x, y, z}` of a
  `vector`), strings (`{upper}` of a string) and userdata index as they always do. There is no
  `rawget`, no table check, no extractor protocol, no runtime validation.
- A missing field binds `nil`. Indexing a `nil` (or number) intermediate raises the ordinary
  Luau error, `attempt to index nil with 'x'`, on the line of the failing field.
- An error stops extraction: later fields are not read; earlier reads and source effects stay.
- Values are bound, not copied: tables, buffers and userdata keep their identity. No record,
  table, closure or call is created by destructuring.
- Bindings are ordinary locals: they shadow, are captured as upvalues, and can be assigned.

A parameter pattern destructures its argument on entry, before the body, parameters in order;
the function's arity and public type are unchanged: `function length({x, y}: Point): number`
has type `(Point) -> number`. A `nil` argument raises at entry, as `p.x` would.

A generator pattern binds each element right after the loop reads it and **before** the
clause's filters: in `[for {id, active} in entities if active => id]`, `id` is read for every
element, rejected ones included, because the pattern names it. Extraction is never moved past
a filter, deferred, or dropped as unused: reads can be observable. Names a pattern binds are
visible to later filters, generators (including dependent sources) and the projection; a
generator's source is outside its own pattern's scope. Every consumer (materialization, `#`,
`sum`, `min`, `max`, `any`, `all`, `into(...)`) and every source kind (plain, `range`,
`enumerate`, zips, table, byte and typed slices) composes unchanged: the element bound to a
slot is whatever the source kind yields, so a pattern over numbers raises the indexing error.
A pipeline whose generator slot is a record pattern is never a recognized data-plane operation;
it keeps the scalar loop.

## Types

Luau Analysis types everything; there is no second checker.

- `local {x, y} = point` with `point: Point` infers `x: number`, `y: number` from ordinary field
  access typing, on both solvers.
- `local {x, y}: Point = value` annotates the whole value, not each binding; field types follow.
- `function length({x, y}: Point)`: the parameter has the annotation, the function is
  `(Point) -> number`; calling it with two numbers is a type error.
- An optional field binds an optional local: `local {health} = entity` with `health: number?`
  gives `number?`, with no implicit assertion or default.
- A field the type does not have is a normal type error (`Key 'nope' not found in table
  'Point'`) at the field's key in the original source, for declarations, parameters, nested
  records and generator slots alike, on both solvers.

## Lowering

```luau
local {position: {x, y}, health: hp}: Entity = entity
-- becomes
local H: Entity = entity local H2 = H.position local x = H2.x local y = H2.y local hp = H.health
```

The pattern is replaced by its holder `H`; annotation, `=` and value are copied unchanged; the
reads are inserted right after the value. For a parameter the holder takes the pattern's place
in the parameter list (with its annotation) and the reads open the body. For a generator slot
the holder is the loop's element local and the reads follow the element read. No line breaks
are added, so line structure is unchanged.

Where a value ends, and where a body starts, is decided by Luau's own parser, not by a JSL
expression recognizer: `lower` lowers once with every pattern named by its holder, parses that
with stock Luau, finds each holder's declaration (its statement end) or parameter (its function
body's start), maps those generated offsets back to the original, and lowers again with the reads
inserted there. Compile, Analysis and `@dream/luau` all take this one path. The same parse checks
that a declaration has exactly one value.

Holders are named `__l3i_comp_<offset of the record's {>`; the stem is salted when any source
identifier equals it or extends it with `_` (the same rule now applies to every generated name,
so a source name like `__l3i_comp_7_g0_src` can no longer be shadowed by a comprehension's own
locals). Holders are not part of any surface tree, autocomplete list or diagnostic.

## Tooling

`@dream/luau` shows patterns, never their lowering:

- `StatLocalPattern {pattern: PatternRecord, annotation: Type?, value: Expr?,
  equalsSignLocation?}` replaces the holder declaration; the generated reads are removed from the
  block. A declaration still missing its `=` has no `value`.
- `PatternRecord {fields, openLocation, closeLocation?, hasClose, complete}`; in parameter
  position it also carries the parameter's `annotation`. It appears in `ExprFunction.args` in the
  parameter's position (other parameters stay `Local`s) and in `ComprehensionGenerator.bindings`
  in the slot's position; `binding` is the first slot's own table (a `Local` or the
  `PatternRecord`), shared with `bindings[1]`.
- `PatternField {key, keyLocation, colonLocation?, shorthand, target?}` where `target` is a
  `PatternIdentifier {name, local}` or a nested `PatternRecord`, absent while missing.
- `PatternIdentifier.local` is the shared `Local` of the binding: every use in the tree is the
  same table, with source function/loop depth and upvalue metadata.

Spans are original, as for every surface node. `complete` means closed, well formed, and with no
parse error inside. Ordinary Luau keeps its stock shapes; `{dialect = "luau"}` still rejects
patterns as stock Luau does.

Analysis: diagnostics map to original spans (a missing field is reported at its key; a nested
nil at the failing field's line at run time). Autocomplete in a pattern key (`local {posi|} =
entity`) or an empty field slot (`local {id, |} = entity`, `{position: {x, |}}`, `function
f({x, |}: Point)`, `[for {id, po|} in entities ...]`) moves the cursor to the field's generated
read `holder.key`, so Luau's property completion lists the destructured value's fields, typed,
nested records included. A record with no field yet (`local {|} = getTypedEntity()`, `local
{position: {|}} = entity`, `function update({|}: Entity)`, `[for {|} in entities ...]`) has
no read to borrow; the tooling lowering (Analysis and `@dream/luau`, never compilation) gives
it one probe read `holder.holder_probe`, and the cursor completes there the same way. The probe's
diagnostics are dropped, its statement is absent from the source tree, and its names are hidden.
Generated holders are hidden; pattern bindings and reads inside comprehensions count as user code
for diagnostics and lints. The generated reads share their source line, so Luau's
`SameLineStatement` lint is dropped for statements the lowering wrote; a lint about a bound name
itself (`LocalUnused` on an unused alias) is reported at its source span. Normal checks,
autocomplete, `mark_dirty` and `clear` are tested under both solvers, across source changes
between pattern declarations, pattern parameters and plain Luau.

Recovery is structural and deterministic: every prefix of representative declarations,
parameters and generators yields the same records, errors and tree on every parse, with no
generated name anywhere in the tree (`local {`, `local {x,`, `local {position: {x`, `function
f({x,`, `[for {x`, `[for {x} in`, ...). A record missing its `}` keeps its fields and bound
names. Strict compilation rejects every structural error before anything runs.

## Evidence

Validated 2026-10-08 on the pinned Luau 0.740 (`c0e346ed`), Intel Core i7-10870H, Linux x86-64,
rustc 1.99.0; rerun in the hardening pass the same day.

**Semantic oracle** (`tests/patterns.rs`): 43 table-driven cases each run a JSL body and the
handwritten Luau a careful author would write, in one VM, and compare results, location-free
errors and the full `__index` read log: source evaluated once, read order, aliases, missing
fields, nested-nil and nil-source errors, failing reads stopping extraction, identity, multiple
returns, vectors/strings/metatables, empty patterns, shadowing, closures, annotations, statement
boundaries; parameter order, arity (`debug.info(f, 'a')`), nil arguments, methods, varargs,
recursion, generics and attributes; every consumer, dependent generators, enumerate/zip slots,
scalar elements, nil projections; getters that reassign the source variable or write a later
field, getters that destructure, indexable userdata, a failure deep inside a nested pattern.
With the `jit` feature every case runs again compiled to native code and agrees. A pattern
pipeline gives the same results and errors with and without the data plane installed. Absolute
read orders, generated-name hygiene, the strict rejection of every incomplete form and the
brief's exit program are asserted separately.

**Executed VM instructions** (deterministic single-step counts, interpreter): declarations
(two/three fields, nested, from a call or from a local) and typed parameters execute exactly as
many instructions as the same-contract handwritten Luau at 0, 1, 32 and 1,024 records.
Generators execute the comprehension's existing one setup instruction more, nothing per
element. A holder initialized from a local costs nothing: Luau aliases a local initialized from
an unassigned local. Disassembly: identical counts of table, closure, call and keyed-read
opcodes; no `NEWTABLE`, `DUPTABLE`, `NEWCLOSURE`, `DUPCLOSURE` or `CALL` is introduced.

**Retired CPU instructions per call** over 4,096 records (`cargo bench --bench patterns`,
minimum of seven rounds of 128 calls; JSL / handwritten):

| Case | Interpreter | Native (`--features jit`) |
|---|---:|---:|
| Two fields | 1,422,344 / 1,422,344 | 324,478 / 324,478 |
| Three fields | 1,864,712 / 1,864,712 | 414,590 / 414,590 |
| Nested record | 1,750,024 / 1,750,024 | 406,398 / 406,398 |
| Two fields, `__index` function records | 6,493,192 / 6,493,192 | 6,157,181 / 6,157,181 |
| Typed parameter, called per record | 1,598,536 / 1,598,536 | 316,371 / 316,371 |
| `sum[for {x, y} in ...]` | 1,565,734 / 1,565,704 | 349,055 / 349,053 |
| Filtered `sum`, half accepted | 1,764,390 / 1,764,360 | 377,727 / 377,725 |
| Nested `#[for {position: {y}} ...]` | 1,696,806 / 1,696,776 | 361,343 / 361,341 |

Declarations, parameters and metatable-backed records are instruction-for-instruction the
handwritten code. Generators retire 30 (interpreter) or 2 (native) more instructions per
4,096-record call: the comprehension's setup, not per element. Criterion times agree within the
run-to-run drift of sequential measurement (for example 40.5 µs against 38.3 µs native for two
fields with identical instruction counts); they are not evidence of a difference. Existing
comprehension and data-plane snapshots are byte-identical: every pattern case was added to
`tests/lowering.rs` without changing any earlier snapshot line. Against the pre-pattern
baseline (`7ff4c6b`), the comprehension bench's JSL rows retire identical instruction counts and
all 67 native data-plane rows agree within 0.015%.

**Compile cost** (release build, 40 compiles of a 239 KB module of 2,000 small functions):

| Module | Pre-pattern baseline | Now |
|---|---:|---:|
| No JSL syntax | 19.1–21.2 ms | 20.7–21.9 ms (within run-to-run noise) |
| One comprehension | 36.2–39.9 ms | 35.3–39.3 ms |
| Three record patterns | n/a | 40.8–43.5 ms |
| A pattern parameter and a declaration in every function | n/a | 58.5–59.8 ms |
| 40 nested functions, each with a 24-deep parameter pattern | n/a | 2.0–2.2 ms |

The extra stock-Luau parse that places the reads costs about 3.6 ms on this module and is paid
only when a declaration or parameter pattern exists; it stays, because it is what makes value
ends and body starts Luau's decision. Most of the gap between a plain module and one with any
JSL construct is not patterns: the baseline already pays it for one comprehension (building the
source map and remapping every location). Peak RSS: 15.4–15.7 MB plain, 17.9 MB with three
patterns, 24.3 MB with patterns in all 2,000 functions.

## Limitations

- Holders are real locals named `__l3i_comp_<n>`. At the default debug level 1 Luau records no
  local names at all, so a debugger lists none, user or generated. At debug level 2 Luau lists
  every register-allocated local, with no way to mark one internal: holders appear under their
  generated names, holding exactly the record they destructure (a parameter's holder is the real
  argument), next to the bound names with their field values. Hiding them would mean rewriting
  serialized debug information or filtering by name in the host, which could hide a user's own
  local; neither is done. They are absent from surface trees, completions and diagnostics.
- Without a value (`local {x` at the end of a file) there is nothing to type keys against.
- A declaration with an annotation but no `=` reports the missing `=` at the next statement
  keyword or the end of the file rather than right after the annotation: the annotation's extent
  is Luau's to parse, and the frontend does not parse types.
- Generator patterns disable data-plane recognition for that pipeline (by design: the element is
  a record read, not a buffer element).
- Lowering parses a pattern-bearing chunk with stock Luau once more than before (see Compile
  cost); the cost is linear in the chunk and paid only when a declaration or parameter pattern
  exists.
- Because the generated reads share their source line, a same-line warning that Luau reports once
  per line can be spent on a generated read and then dropped: `local {a} = t print(a)` gets no
  `SameLineStatement` warning, where `local a = t.a print(a)` would.
