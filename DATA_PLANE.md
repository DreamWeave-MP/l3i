# The pipeline IR and the generic data plane

L3i has two layers under JSL. The **pipeline plan** is how every comprehension-shaped
expression is represented and lowered. The **data plane** (`@dream/data`, feature `data`) is
the generic bulk-data substrate: native operations over typed buffer spans with no per-element
callback and no per-element host crossing. This note is the architecture record for both and
the semantic matrix for the data plane. The JSL contracts themselves live in
[COMPREHENSIONS.md](COMPREHENSIONS.md); tooling in [SURFACE_TOOLING.md](SURFACE_TOOLING.md).

```text
                 .jsl source
                      ↓
           canonical recovering frontend        csrc/surface_frontend.cpp
                      ↓
               pipeline plan                    csrc/surface_syntax.cpp (Pipeline, Stage, Consumer)
                      ↓
      ┌───────────────┴────────────────┐
      ↓                                ↓
 fused scalar Luau             recognized data pipeline
 (one emitter, every           → prologue in Luau, one call into
  source kind × consumer)        the data plane, scalar fallback
      ↓                                ↓
 stock Luau compiler           @dream/data (src/data/mod.rs)
      └───────────────┬────────────────┘
                      ↓
              stock Luau runtime
```

## 1. The pipeline plan

A comprehension is planned as ordered **stages** feeding one **consumer**:

```text
Stage      = Generator { kind: Plain | Enumerate | Range | Zip | Slice, prefix }
           | Filter
Consumer   = Materialize | Count | Sum | Min | Max | Any | All | Sink
Pipeline   = { consumer, stages[], generators }
```

One recursive emitter walks the stages: each generator evaluates its source once and opens a
loop, each filter opens an `if`, the consumer's `step` runs on every accepted projection, and
the closers unwind. Because the walk is recursive, a stage may emit the rest of the pipeline
more than once: a leading slice whose representation is only known at run time emits one
specialized loop for buffers and one for tables under a single dispatch, instead of testing the
representation per element.

The plan knows three facts that drive allocation and indexing, computed once:

- `sized`: a lone leading generator (not a numeric range) knows its element count before
  allocation, so materialization preallocates with `table.create(n)`.
- `directIndex`: a lone leading plain/enumerate/zip generator with no filters stores at its
  loop index and needs no cursor.
- `needsCursor`: every other materialization, and every counting or accumulating consumer.

Everything observable about lowering is pinned by `tests/lowering.rs`: generated text,
provenance segments, tooling sites and structural errors for every shape under the compile,
Analysis, Analysis-with-buffer-types and syntax-tooling policies. A refactor proves itself by
leaving those files unchanged; an intentional change regenerates them and the diff is reviewed.

The frontend records one consumer prefix per comprehension (`#`, a reducer name, or
`into(destination)`), and the syntax module exposes them as `ExprUnary`, `ExprReduction` and
`ExprSink` around the `ExprComprehension`. Adding a consumer is: one `Reducer`/prefix in the
frontend, one `Consumer` with a `step` and an `epilogue` in the emitter, one node in the syntax
module, tests, docs. No new recognizer, no new loop emitter.

## 2. The data plane

### 2.1 Operands

A **span** is `(buffer, kind, offset, count)`: `count` elements of `kind` starting `offset`
bytes into a Luau buffer. Kinds are the `buffer` library's own little-endian representations:
`u8 i8 u16 i16 u32 i32 f32 f64`. A span is validated once, before any element is read (offset
and count non-negative, `offset + count × size ≤ length`), and is a descriptor, never an
allocation. Contiguous only; no strides.

An **index vector** is a `u32` buffer of zero-based element positions. A **selection** is a
`dream.data.Selection` userdata: a bitset over the positions of a span of a fixed length,
reusable as the output of any producing operation (`into`).

Element values cross as Luau numbers with the `buffer` library's conversions: integer kinds
read exactly and write by truncating toward zero then wrapping to the width (NaN writes zero;
values beyond ±2^63 saturate before the wrap); `f32` rounds on write; `f64` is exact.

### 2.2 Operation matrix

| Operation | Reads | Writes / returns | Empty input | NaN | Aliasing | Errors before any write |
|---|---|---|---|---|---|---|
| `sum` | span | number, `f64` accumulation in element order | `0` | propagates as Luau `+` | n/a | bounds |
| `min`, `max` | span | number or nil | nil | a leading NaN stays; later NaNs never replace (Luau `<`/`>`) | n/a | bounds |
| `argmin`, `argmax` | span | zero-based position or nil | nil | as `min`/`max` | n/a | bounds |
| `count` | span | number of elements satisfying `cmp` | `0` | every comparison false except `ne` (IEEE) | n/a | bounds, comparison name |
| `compare` | span | selection of positions satisfying `cmp`; into `into` (resized) or new | empty selection | as `count` | `into` may be any selection | bounds |
| `Selection:intersect/union/xor/difference` | two selections of one length | selection; into `into` or new | — | — | `into` may be either operand | lengths differ |
| `Selection:complement` | selection | selection | — | — | `into` may be the source | — |
| `Selection:count/len/get/any/all/clear` | selection | number / boolean / nothing | `all` of nothing is true | — | — | — |
| `Selection:indices` | selection | u32 index vector and count; into `out` from `offset` or new exact buffer | empty buffer, 0 | — | — | `out` too small |
| `gather` | source span, index vector | `destination[i] = source[idx[i]]`; count written | 0 | copied bit-exact | sequential: read then write per element, in index order | bounds, any index ≥ source count |
| `scatter` | source span, index vector | `destination[idx[i]] = source[i]`; count written | 0 | copied bit-exact | sequential; a repeated index keeps the last write | bounds, any index ≥ destination count |
| `fill` | — | every element of the span | — | writes zero for integer kinds | — | bounds |
| `add` | two spans | `out[i] = l[i] + r[i]` | — | Luau `+` | out may be either input (element-wise sequential) | bounds (all three spans) |
| `scale` | span | `out[i] = s[i] × factor` | — | Luau `*` | out may be the source | bounds |
| `clamp` | span | `out[i] = clamp(s[i], low, high)` | — | NaN passes through, as `math.clamp` | out may be the source | bounds, `low > high` (or NaN bounds) |
| `argsort` | key span | u32 permutation into `out` from `outOffset`; count | 0 | NaN keys last, in original order | keys are copied before sorting, so `out` may overlap them | bounds |
| `partition` | key span | u32 index vector: satisfying positions first, then the rest, each in original order; accepted count | 0 | as `count` | as `argsort` | bounds, comparison name |

`argsort` is stable: equal keys keep their position order. It sorts a copy of the keys paired
with positions (`Vec<(f64, u32)>`), so the buffer is read once and never touched during the
sort; a caller-supplied scratch is unnecessary for correctness and left out until a benchmark
shows the allocation matters.

### 2.3 Execution

Every module function is a bound Rust function invoked once per bulk operation: one host
crossing amortized over the whole span, element loops in Rust over raw pointers after the single
bounds check. This is the portable/bound path and the correctness oracle for everything.

The reductions also exist as methods of a typed receiver, `data.kind(name)` (`dream.data.Kind`):
`K:sum(buffer, offset, count)`, `K:min`, `K:max`, `K:countEq/Ne/Lt/Le/Gt/Ge(buffer, offset,
count, threshold)`. Bound, they are the same loops. Under the `jit` feature, a call on a receiver
the script annotated (`local F32: dream_data_Kind = data.kind("f32")`, in a `--!native` chunk)
is lowered by `src/data/lowering.rs` into Luau IR: the receiver tag, buffer tag, whole
non-negative bounds and the span's fit are checked first (anything else jumps to the binder,
whose errors are the contract), then a loop per element kind behind a dispatch on the
receiver's kind word reads elements with the same `BUFFER_READ*` the `buffer` library lowers
to and accumulates in double in element order. Loop state lives in the receiver's scratch
words, since Luau IR has no loop-carried values. This is the established L3i fast-path pattern
(bytes, intern): one semantic implementation, a bound path, an optional native lowering.

No worker threads, no SIMD framework, no second JIT.

### 2.4 Evidence

`cargo bench --features data --bench data` (interpreter) and `--features data,jit` (every
chunk compiled natively, so the loop baselines are CodeGen loops). i7-10870H, retired
instructions per call (minimum of five rounds of 64 calls), Criterion median time. `u8` sums and
`> 127` counts over `n` bytes; the recognized JSL form includes the slice prologue (type checks,
bounds normalization); the scalar fallback is the same JSL source run without the extension.

Interpreter:

| n | JSL recognized | JSL scalar fallback | explicit `data.sum` / `data.count` | handwritten `readu8` loop |
|---:|---:|---:|---:|---:|
| sum 16 | 5,365 (532 ns) | 14,741 (1.21 µs) | 2,434 (205 ns) | 14,637 (1.04 µs) |
| sum 256 | 9,685 (832 ns) | 171,221 (12.2 µs) | 6,754 (531 ns) | 213,597 (16.6 µs) |
| sum 4,096 | 78,805 (6.10 µs) | 2,674,901 (191 µs) | 75,874 (5.48 µs) | 3,396,957 (250 µs) |
| sum 65,536 | 1,184,725 (83.3 µs) | 42,733,784 (3.03 ms) | 1,181,794 (87.6 µs) | 54,330,722 (3.87 ms) |
| count 16 | 5,704 (557 ns) | 14,971 (1.27 µs) | 2,799 (251 ns) | 15,217 (1.25 µs) |
| count 256 | 11,944 (1.12 µs) | 176,945 (14.9 µs) | 9,039 (737 ns) | 223,941 (18.8 µs) |
| count 4,096 | 111,784 (9.08 µs) | 2,768,018 (229 µs) | 108,879 (8.80 µs) | 3,563,260 (285 µs) |
| count 65,536 | 1,709,224 (136 µs) | 44,226,066 (3.52 ms) | 1,706,319 (139 µs) | 56,992,809 (4.56 ms) |

There is no small-N crossover in the interpreter: at 16 elements the recognized pipeline
already executes a third of the scalar loop's instructions, and the explicit call a sixth. The
JSL form carries about 3,000 instructions of prologue over the explicit call, independent of n.
`compare` into a reused selection then `count()` costs 2 to 4% more than the direct `count`.

| n | explicit `data.sum` f32 | handwritten `readf32` loop | explicit `data.gather` f32 | handwritten indexed copy | stable `data.argsort` f32 | `table.sort` index table, comparator (unstable) |
|---:|---:|---:|---:|---:|---:|---:|
| 256 | 6,273 (648 ns) | 223,837 (15.7 µs) | 14,070 (1.20 µs) | 599,977 (49.5 µs) | 63,129 (5.15 µs) | 4,247,685 (369 µs) |
| 4,096 | | | | | 1,251,732 (106 µs) | 108,144,296 (9.22 ms) |
| 65,536 | 1,050,753 (116 µs) | 56,952,163 (4.17 ms) | 2,886,390 (238 µs) | 153,094,071 (11.8 ms) | 16,006,037 (1.88 ms) | 2,307,862,971 (199 ms) |

Native code (`jit`, every chunk compiled, globals sandboxed so native code never exits on a
global access). The recognized JSL sum now runs the receiver's unrolled IR loop
(`src/data/lowering.rs`); the handwritten and fallback loops are Luau CodeGen loops.

| n | JSL recognized (IR loop) | JSL scalar fallback (CodeGen loop) | explicit `data.sum` (bound) | handwritten `readu8` (CodeGen loop) |
|---:|---:|---:|---:|---:|
| sum 16 | 1,363 (124 ns) | 1,719 (151 ns) | 1,675 (138 ns) | 1,395 (117 ns) |
| sum 256 | 2,773 (376 ns) | 9,639 (879 ns) | 5,995 (476 ns) | 9,075 (793 ns) |
| sum 4,096 | 25,333 (4.39 µs) | 136,359 (12.7 µs) | 75,115 (6.00 µs) | 131,955 (11.8 µs) |
| sum 65,536 | 386,293 (69.1 µs) | 2,163,879 (184 µs) | 1,181,035 (94.4 µs) | 2,098,035 (194 µs) |
| count 16 | 1,563 (137 ns) | 1,749 (143 ns) | 1,960 (177 ns) | 1,393 (109 ns) |
| count 256 | 6,115 (771 ns) | 10,259 (922 ns) | 8,200 (655 ns) | 9,183 (848 ns) |
| count 4,096 | 78,919 (10.3 µs) | 146,384 (14.6 µs) | 108,040 (8.14 µs) | 133,788 (13.3 µs) |
| count 65,536 | 1,243,831 (170 µs) | 2,324,444 (234 µs) | 1,705,480 (130 µs) | 2,127,528 (204 µs) |

| n | explicit `data.sum` f32 | `readf32` CodeGen loop | `data.gather` f32 | indexed copy, CodeGen | stable `data.argsort` | `table.sort` comparator, CodeGen (unstable) |
|---:|---:|---:|---:|---:|---:|---:|
| 256 | 5,514 (432 ns) | 9,843 (849 ns) | 12,913 (982 ns) | 13,184 (1.28 µs) | 63,111 (4.80 µs) | 1,138,409 (99.6 µs) |
| 4,096 | | | | | 1,275,925 (100 µs) | 28,714,511 (2.20 ms) |
| 65,536 | 1,049,994 (83.6 µs) | 2,294,643 (193 µs) | 2,885,233 (201 µs) | 3,146,624 (339 µs) | 16,327,107 (1.75 ms) | 612,840,657 (52.4 ms) |

What the machine shape says:

- Luau's own CodeGen loop costs about 32 instructions per byte (`FORNLOOP`, the `readu8`
  fastcall with its checks, the number add through a VM register). The bound Rust loop costs
  about 18 per byte including its once-per-call argument parsing, and wins in time from 256
  elements up.
- The first IR loop kept its cursor and accumulator in the receiver's scratch (Luau IR has no
  loop-carried values), so every element paid three memory round trips: fewer instructions than
  the bound call (17 per byte) but slower in time at 65,536 (161 µs versus 115 µs) because the
  loop was latency bound on store-to-load forwarding.
- Unrolling the pure-add `sum` body to eight elements per block (one cursor and one
  accumulator round trip per eight) cut it to 6 instructions per byte and 69 µs: faster than
  the bound call at every size, and at 16 elements within the noise of the handwritten native
  loop, with no call overhead left to amortize.
- `count`, `min` and `max` branch per element, which ends an IR block, so they cannot unroll
  the same way (`SELECT_NUM` is equality-only and the min/max instructions do not keep the
  loop's NaN rule). Their IR loops still execute fewer instructions than the bound call but
  lose in time above a few hundred elements. The JSL bridge therefore lowers sums to the
  receiver and counts and extrema to the bound functions; the explicit receiver remains for
  scripts that measure otherwise.
- `compare` into a reused selection then `count()` costs within 5% of the direct count at
  every size, so composing selections is not a performance trade.

Interpreter and native agree on the ordering of everything else: the bound data-plane
operations beat the best obvious Luau by 2× (gather, native) to 300× (stable argsort against a
comparator sort, native) and never lose, down to 16 elements.



### 2.5 Reaching the data plane from JSL

The extension publishes its module table under the global `__l3i_data` as well as
`@dream/data`. A recognized pipeline's lowering snapshots that global in the chunk prelude and
calls it once with the span the ordinary JSL prologue already validated and normalized; when
the global is absent (the runtime did not install the extension) the same code takes the scalar
loop. Recognition is narrow and semantic: the lowered call must compute exactly what the scalar
loop computes, in the same numeric representation and with the same error behaviour, or it is
not emitted. See COMPREHENSIONS.md, "Recognized data pipelines".

### 2.6 Semantic law

Errors and evaluation order are effects. An arbitrary JSL expression in a filter or projection
has unknown effects and is never moved into the data plane; the fused scalar loop is its
correct and final form. Only `KnownDataOp` stages (the binding itself, a comparison of the
binding against a literal) are eligible. The explicit API stays first-class: nothing requires
JSL recognition to reach the fast path.

## 3. Deferred, deliberately

These were considered and left out because nothing in this repository asks for them yet; the
brief's rule is to earn operations from evidence, not from plausible consumers.

- **Strided spans.** A field of a packed record is a stride, not a contiguous span. Adding a
  stride to the operand would touch every operation and slow the contiguous fast path unless
  specialized. Re-open when a package reads a field column in a hot loop; the change is one
  operand field plus per-operation handling, not a new API.
- **Buffer sinks.** `into(buffer)[...]` needs an element kind the JSL surface cannot yet spell,
  and a nil-projection rule for a representation that has no nil. The consumer exists in the
  plan (`Consumer::Sink`); the spelling waits for a use that fixes the semantics.
- **Filters against variables in recognized pipelines.** `#[for x in buf[a:b] if x > limit
  => x]` is not recognized because the compile path has no scope information: `limit` could be
  a global with a metamethod, so reading it once instead of per element would be a semantic
  change. Analysis knows whether it is a plain local; carrying that knowledge to the compile
  path is a new seam (the Analysis-only `bufferGenerators` list shows the shape). Worth doing
  when a real pipeline needs it; the scalar loop it gets today is already native under `jit`.
- **Caller scratch for `argsort`.** The sort copies keys into a `Vec` per call; a reusable
  scratch argument is the obvious next step once a steady-state caller shows the allocation in
  a profile.
