# Original-source locations for L3i surface syntax

## Public contract

Tooling uses the source supplied by the caller, not expanded Luau text:

- Analysis diagnostics have original-source spans, including errors in required modules.
- `Analysis::autocomplete` takes original-source, zero-based **byte** coordinates.
  Translate UTF-16/editor columns before calling it. Generated completion bindings
  are hidden; legitimate user bindings with similar names remain visible.
- `analysis::parse` exports a **lowered Luau AST** whose locations are original.
  Node kinds and generated locals remain lowered; it is not a surface AST or CST.
- `source::compile`, loading source, and `source::disassemble` use the same mapped
  parser/compiler pipeline. Parse/compile failures use `:<one-based line>: message`.
  Disassembly now reports the first located parse error rather than just a count.
- Bytecode embeds original debug lines. Runtime errors, native `call_site` queries,
  tracebacks, breakpoints, and coverage read this metadata without a runtime map registry.

No Luau sources or serialized bytecode formats were modified. Recompile existing
cached bytecode to acquire corrected metadata; old bytecode cannot be repaired by
loading it through the new runtime.

Tools that previously compensated for expansion by passing lowered cursor coordinates
must remove that workaround and pass original coordinates. Ordinary Luau remains identity-mapped.

## Provenance and synthetic policy

The generic surface seam returns lowered text plus a source map. Copied ranges map
affinely in byte offsets. Synthetic ranges carry intentional original spans:

| Generated work | Original attribution |
|---|---|
| Wrapper function/call | Entire comprehension, including consumed unary `#` |
| Source aliases, length/index machinery, generator bindings | Generator clause; copied source/binding tokens remain exact |
| Filter control structure | Corresponding predicate |
| Projection temporary, nil guard, count increment, result store | Projection span |
| Synthetic loop exits and final return | Closing comprehension bracket |

Nested rewrites compose directly against the whole original input. Offsets are
never guessed from generated temporary spellings or searched projection substrings.
Adjacent compatible segments merge; copied exclusive ends do not adopt the next
synthetic segment's origin. Container locations are the envelope of their provenance,
so reordered coordinates do not produce backwards spans. CRLF and UTF-8 are not normalized.

Compilation parses lowered source, remaps AST/local/auxiliary/comment locations,
and uses stock Luau's pre-parsed compiler overload. Shared objects are remapped once,
including annotations and function metadata omitted by the default visitor.
Analysis deliberately keeps its internal AST and scopes in lowered coordinates:
diagnostics translate out, cursors translate in. This avoids changing Luau's scope
and refinement machinery merely to satisfy presentation.

Maps belong to the same module snapshot as the frontend's source AST. Existence
probes do not replace a cached map. `mark_dirty` permits a reread; `clear` clears both
the frontend and maps. Tests alternate normal checking and autocomplete under both
solvers, including transitions between lowered and plain source.

## Diagnostic prose

Primary structured spans map exactly for copied expressions. Secondary references
need a separate rule because Luau sometimes formats them into a plain message:

- Duplicate-type prior-definition locations are mapped structurally before formatting.
- Known parser closing-token references and lint reference templates are translated.
- Column-bearing references retain enough information to report original line and column.
- A generated line can contain copies from several original lines. A line-only
  reference in that case is explicitly labeled `within a lowered comprehension
  (ambiguous source reference)` rather than assigned a guessed original line.
- Arbitrary numbers, user exception messages, and unrecognized diagnostic prose
  are not globally rewritten. The template adapter must be reviewed on Luau upgrades.

For example, after a four-line comprehension followed by an unterminated function
on original line 5, both the primary EOF position and `to close 'function' at line 5`
are now correct. The old text incorrectly referred to generated line 2.

## Debugging boundaries

Runtime metadata has lines, not columns. Enable line information (`debug_level >= 1`).
Optimization level 2 can inline or remove code; mapping cannot recreate eliminated
breakpoint sites. At lower optimization levels generated wrappers and locals may
remain visible. Coverage includes synthetic instructions on their chosen anchors;
this is original-line attribution, not a new high-level comprehension coverage model.

Mapping and parsing remain separate powers, but the shared surface frontend now
provides structural recovery and explicit expression/binding sites. Analysis maps
hole cursors to their correct scope, preserving typed member completion. `@dream/luau`
defaults to source-level comprehension nodes; `{dialect = "luau"}` explicitly selects
stock syntax. The earlier recognizer and its text-only C seam were deleted, not kept
as compatibility paths. See [SURFACE_TOOLING.md](SURFACE_TOOLING.md) for recovery and
source-tree contracts. Fatal/resource-limited recovery can retain less tree detail;
CST preservation is still not provided.

## Validation

Mapping-baseline results on the pinned Luau 0.740 checkout, before the canonical
frontend replacement (current source-tooling behavior is described in SURFACE_TOOLING.md):

| Gate | Result |
|---|---|
| Default workspace tests | PASS — 79 unit + 146 integration |
| Analysis-feature tests | PASS — 79 + 166 |
| All-feature workspace tests | PASS — 88 + 213 |
| All-feature/all-target strict pedantic Clippy | PASS |
| Formatting and all-feature rustdoc | PASS |
| GCC standalone mapping/scanner tests | PASS |
| Clang ASan/UBSan mapping/scanner tests | PASS |
| Allocation-failure probes | PASS |

Regression coverage includes:

- Same-line and later-line diagnostics, nested/multiline projections, CRLF, UTF-8,
  parse/lint/type errors, imported modules, EOF with and without trailing newline.
- Projection/filter/property completion after earlier same-line expansions, hidden
  synthetic names, legitimate prefix-like bindings, and cached snapshot invalidation.
- JSON spans for copied expressions, user locals, and type annotations.
- Original native call-site lines at optimization levels 0, 1, and 2.
- A breakpoint on the original projection line and original-line coverage.
- Runtime nil-guard and copied-operation errors for both materialized and fused forms.
- Existing deterministic instruction-count and allocation-fusion regressions, unchanged.

Required Rust validation:

```bash
cargo fmt --check
cargo test
cargo test --features analysis
cargo test --workspace --all-features
cargo clippy --workspace --all-targets --all-features -- -W clippy::pedantic -D warnings
```

The canonical frontend now uses Luau's lexer/parser library. To run its standalone
frontend and lowering/map tests, link `surface_frontend.cpp` and the relevant test
with the Cargo-built Ast/Common/VM archives, as in the fault-probe command below.
There is no dependency-free duplicate recognizer.

```bash
clang++ -std=c++17 -O1 -fuse-ld=lld -Wall -Wextra -Werror -pedantic \
  -isystem luau/Ast/include -isystem luau/Common/include -Icsrc \
  csrc/surface_frontend.cpp csrc/surface_syntax.cpp csrc/source_map.cpp csrc/surface_syntax_test.cpp \
  -Wl,--start-group "$LUAU_OUT/libluauast.a" "$LUAU_OUT/libluaucommon.a" \
  "$LUAU_OUT/libluauvm.a" -Wl,--end-group \
  -o /tmp/opencode/l3i-surface-test
/tmp/opencode/l3i-surface-test
```

It also passes under Clang ASan/UBSan. A separate allocation-failure probe ensures
compiler/disassembler error serialization cannot throw across the C ABI. Its outer
exception barrier returns null and output size zero without allocating. The earlier
text-only seam and its allocation probes were removed with the old recognizer.

To run the fault probe, set `LUAU_OUT` to the current Cargo build script's `OUT_DIR`
(shown by `cargo build -vv`), containing the Luau archives. Never link the probe's
global `operator new` override into the Rust host:

```bash
clang++ -std=c++17 -O1 -fuse-ld=lld -Wall -Wextra -Werror -pedantic \
  -isystem luau/Ast/include -isystem luau/Common/include \
  -isystem luau/Compiler/include -isystem luau/Bytecode/include -Icsrc \
  csrc/compiler_failure_test.cpp csrc/bytecode.cpp csrc/surface_frontend.cpp csrc/surface_syntax.cpp \
  csrc/source_map.cpp csrc/source_locations.cpp -Wl,--start-group \
  "$LUAU_OUT/libluaucompiler.a" "$LUAU_OUT/libluauast.a" \
  "$LUAU_OUT/libluaubytecode.a" "$LUAU_OUT/libluaucommon.a" \
  "$LUAU_OUT/libluauvm.a" -Wl,--end-group \
  -o /tmp/opencode/l3i-compiler-failure-test
/tmp/opencode/l3i-compiler-failure-test
```

The completed run covered every allocation budget through parse-error and compile-error
serialization for both entry points. All passed.

The generated representation may move. The user's source remains the reference.
