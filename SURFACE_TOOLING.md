# Comprehensions are source-language tooling features

There is one canonical surface frontend, built on stock Luau tokens. The old
recognizer and text-only compatibility seam were deleted. Compilation, Analysis,
and `@dream/luau` consume the same records; strict versus tolerant behavior is policy,
not a second grammar. Luau itself remains unmodified.

## `@dream/luau` defaults to surface syntax

```luau
local luau = require('@dream/luau')
local result = luau.parse('return [for e in entities if e.active => e.id]')
local comprehension = result.root.body[1].list[1]
assert(comprehension.kind == 'ExprComprehension')
```

The expression exposes ordered `clauses`, a `projection`, `openLocation`, optional
`arrowLocation`/`closeLocation`, and `hasArrow`, `hasClose`, `complete` flags.

- `ComprehensionGenerator`: `binding` (a shared `Local`, absent when missing),
  `source`, `keywordLocation`, optional `inLocation`, and `hasIn`.
- A direct `range(first, last[, step])` generator source is an `ExprRange` with
  original-source `args`; it lowers to a numeric loop and never resolves a binding
  named `range`.
- `ComprehensionFilter`: `condition` and `keywordLocation`.
- `ExprSlice`: `source`, `first`, `last`, and `elementKind` (the literal's content) for a typed
  slice `source[first:last, "f32@16+12"]`, in generator and standalone positions alike.
- `#[for ...]`: ordinary `ExprUnary` with a comprehension operand. Allocation fusion
  is a lowering decision, not a mutation of the source tree.
- Reducers: `ExprReduction {op = "sum" | "min" | "max" | "any" | "all", expr =
  ExprComprehension}`. Its span includes the reducer name; the child starts at `[`. A
  binding with the reducer's name is not consulted, and member access is not a reducer.
- Sinks: `ExprSink {destination = Expr, elementKind = string?, offset = Expr?, expr =
  ExprComprehension}` for `into(destination[, "kind"[, offset]])[for ...]`. Its span starts at
  `into`; the destination and offset keep their own source nodes and spans; `elementKind` is the
  literal's content for a buffer sink. `into` is likewise never resolved against a binding.

All spans slice the original source using the existing one-based byte-column,
inclusive-end API. A zero-width insertion point has `endColumn = column - 1` on its
line. Missing expressions are `ExprError` with `isMissing = true` and no children.
Malformed ordinary expressions retain stock recovery nodes or an error covering
the full original expression, never just a valid prefix that hides the bad suffix.
`messageIndex` is zero-based into `errors`; `-1` means there is no matching message.

`complete` means the construct has no structural holes or reported parser errors
within its source span; it does not mean it typechecks. Generated functions/locals
are absent from the surface tree. Binding identity, function/loop depth, shadowing,
and upvalue metadata describe source scopes, including real functions nested inside
projections. A generator's source is outside its own binding's scope.

Ordinary valid Luau keeps its existing tree shapes. For a deliberately stock parser:

```luau
local stock = luau.parse(source, {dialect = 'luau', tokens = true})
```

The default dialect is `l3i`; invalid dialect/options raise argument errors.
`declarations` and `tokens` retain their existing meanings. The token buffer still
uses 12-byte records and the same 14 numbered kinds. The surface dialect merges
adjacent `=` and `>` into one `symbol` token for `=>`. Tokens, comments, hot comments,
and `lineStarts` always describe original bytes, including interpolation regions.

## Editing unfinished code

These retain a comprehension record and located errors rather than requiring a
finished expression before tools can understand the construct:

```luau
[for
[for x
[for x in
[for x in entities
[for x in entities if
[for x in entities =>
[for x in entities => x.
```

Analysis uses recovery-only scaffolding to expose known bindings and expressions
to Luau's existing typechecker. It completes source globals in the enclosing scope,
suggests missing `in`/`=>`/`]` where relevant, and retains typed member completion
after a dot in sources, filters, and projections. Nullable filter refinement and
outer-generator captures survive nested partial expressions. Generated names and
errors caused solely by empty recovery holes are not presented as user code.
The lowering-owned operation prelude is likewise absent from the surface tree and
autocomplete results. Leading comments and hot comments remain before that prelude.
An unfinished copied member expression still reports its missing identifier even
when the diagnostic lands on a generated fence sharing another hole's EOF point.

`analysis::parse` continues to expose a **lowered** AST JSON representation with
original spans. Use `@dream/luau` for the genuine source tree. Both report structural
surface diagnostics. Cached Analysis source/maps/sites belong to the same snapshot;
normal checks, autocomplete, `mark_dirty`, and `clear` are tested under both solvers.

Compilation rejects structural recovery before bytecode generation. Neither a
missing projection nor a tooling placeholder can execute. Valid dense/non-nil,
truthiness, source/filter/projection ordering, count/sum fusion, and instruction
parity contracts remain unchanged.

## Synchronization and limits

Missing brackets synchronize at enclosing closers, commas, statement boundaries,
or EOF. Unclosed ordinary expression delimiters can recover at a surface arrow
with an explicit missing-delimiter diagnostic; later filters/arguments/statements
are not silently consumed. Bare postfix forms such as `values[for ...]` and
`values.sum[for ...]` are invalid, not implicit calls: use an explicitly parenthesized
collection index if intended. Tooling retains them as erroneous recovered indices
without exposing generated functions; strict compilation rejects them.

Recovery is deterministic and bounded, not a claim that every severely damaged
file retains every subtree. Limits for actual surface parsing are:

- 262,144 retained tokens starting at the first genuine comprehension opener;
- nesting below 128;
- expression-prefix validation: 4,096 calls and 8,388,608 cumulative bytes.

Comments and ordinary tokens before the first opener do not consume that token
budget. Ordinary Luau and inert string/comment sentinels do not gain an 8 MiB size
limit. Luau's own recursion limits also apply. Fatal recovery still returns the
required `StatBlock` root with an error statement, but can lose detailed subtrees.
CST/round-trip formatting and incremental parsing are not supplied by this pass.

## Regression signal

Tests cover every successive prefix of representative comprehensions, explicit
holes, deterministic original error spans, synchronization, nested/dependent
clauses, comments/interpolation/token coverage, GC survival of shared bindings,
ordinary-vs-stock metadata, and invalid postfix consumers. Analysis tests exercise
both solvers, copied errors, property completion, refinement, and changing snapshots.
Strict compilation tests prove recovery holes cannot execute prior source effects.

The existing executed-instruction tests still compare dense, filtered, count, and
sum lowering with checked handwritten loops: one setup instruction, no per-element
wrapper overhead. Nil checks and projection effects remain mandatory.

Validated on the real checkout with Luau 0.740 (rerun 2026-10-07; the lowering snapshot
suite in `tests/lowering.rs` now pins generated text, provenance and sites for every shape):

| Gate | Result |
|---|---|
| `cargo test --workspace` | PASS — 73 unit + 169 integration |
| `cargo test --features analysis` | PASS — 73 + 202 |
| `cargo test --workspace --all-features` | PASS — 82 + 269 |
| All-target/all-feature strict pedantic Clippy | PASS |
| Formatting and all-feature rustdoc | PASS |
| Standalone canonical frontend, ASan/UBSan | PASS |
| Standalone lowering/provenance tests | PASS |
| Compiler/disassembler allocation-failure probes | PASS |

The six deleted Rust unit tests exercised the removed text-only C seam. Their
semantic coverage remains in runtime/bytecode tests, with the new canonical
frontend and tooling suites covering structural recovery directly.
