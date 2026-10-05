# Comprehensions: integration and instruction evidence

The comprehension baseline below was validated on the actual L3i checkout, 2026-10-04,
starting from the already-applied `88e0636` dense/non-nil prototype. Luau remains stock 0.740,
submodule commit `c0e346edd89066b44dca174c9f54ce84c746a540`. A post-baseline `sum[for ...]`
JSL reducer prototype is documented at the end; its Cargo gates remain pending.

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
a Jess/Luau runtime: filtered, empty, nested and comment-separated forms produced the expected
results. Bytecode disassembly of the lowered filtered sum has no closure construction or result
table and differs from the equivalent handwritten loop by the same kind of one-time setup move
already measured for comprehension expressions.

Rust runtime, disassembly, executed-instruction and Criterion cases are included for the real L3i
checkout but still require Cargo validation. In particular, the expected invariant is:

```text
sum[for ...] executed VM instructions == same-contract handwritten fused sum + one setup instruction
```

The benchmark also compares fused sum against materializing the comprehension and traversing the
result a second time. Treat any wall-clock claim as secondary to disassembly/instruction evidence,
just as with the original comprehension campaign.
