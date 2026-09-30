+++
title = "Native code and analysis"
description = "Luau's code generator with lowering hooks written in Rust, the IR builder, assembly and IR dumps and execution statistics (feature jit); the type checker, linter, autocomplete and parser over a host source provider (feature analysis)."
weight = 260

[extra]
kind = "api"
+++

Modules `l3i::native_code` (with `hooks`, `ir`, `vector_buffer`), available with the `jit`
feature, and `l3i::analysis`, available with the `analysis` feature.
[Native code](@/docs/native-code.md) is the guide.

## Native code generation

One `NativeCodeGen` per runtime owns a shared Luau code generation context with a hard size
budget, immutable compilation options (mode, counters, userdata type names, the lowering hooks),
and a registry of compiled modules keyed by a 128-bit hash of their bytecode so identical scripts
share native code and hash collisions are rejected rather than aliased. The `jit` feature builds
Luau's CodeGen library and the C++ shim `csrc/codegen.cpp` that exposes its C++-only parts.

{{ api_signature(value="enum NativeCodeMode { Off, Annotated, Eager }") }}

Which functions are compiled natively: never (`compile` reports `Skipped`); only modules marked
`--!native`; every module. `Clone`, `Copy`, `Debug`, `Eq`.

| Constant | Value |
|---|---|
| `BLOCK_SIZE: usize` | 4 MiB code blocks, as OpenMW |
| `DEFAULT_MAX_TOTAL_SIZE: usize` | 32 MiB of native code per runtime by default |

{{ api_signature(value="struct NativeCodeOptions { pub mode: NativeCodeMode, pub max_total_size: usize, pub record_counters: bool, pub nop_padding: bool, pub userdata_types: Vec<String>, pub hooks: Vec<Box<dyn NativeCodeHooks>> }") }}

Host choices for native code generation, passed to `RuntimeBuilder::native_code`. `Debug`,
`Default`.

| Field | Default | Meaning |
|---|---|---|
| `mode` | `Annotated` | |
| `max_total_size` | 32 MiB | Hard ceiling for native code in this runtime; clamped to at least one block |
| `record_counters` | `false` | Record Luau's block execution counters (a regression probe, not a game setting) |
| `nop_padding` | `false` | Insert random NOP sleds between blocks |
| `userdata_types` | empty | Userdata type names in type index order: the same list handed to `CompileOptions::userdata_types`, so annotated parameters reach the `userdata_*` hooks as `TAGGED_USERDATA_BASE + index` |
| `hooks` | `[VectorBufferWriter]` | Lowering hooks, asked in order |

Builder methods: `fn mode(self, mode: NativeCodeMode) -> Self`, `fn hooks(self, hooks: impl
NativeCodeHooks) -> Self` (adds a hook set after the defaults), `fn userdata_types(self, names:
impl IntoIterator<Item = impl Into<String>>) -> Self`.

{{ api_signature(value="enum NativeCodeStatus { Skipped, Unavailable, IdentityCollision, AllocationRetrySkipped, Success, NothingToCompile, NotNativeModule, CodeGenNotInitialized, CodeGenOverflowInstructionLimit, CodeGenOverflowBlockLimit, CodeGenOverflowBlockInstructionLimit, CodeGenAssemblerFinalizationFailure, CodeGenLoweringFailure, AllocationFailed, UnknownFailure }") }}

Outcome of one compilation. The first four are the binder's: mode is `Off`; the platform has no
code generator or it failed to initialise; another module with different bytecode already has
this id; this module ran out of code space before and is not retried. The rest are Luau's
`CodeGenCompilationResult`. `Clone`, `Copy`, `Debug`, `Eq`.

{{ api_signature(value="type ModuleId = [u8; 16]") }}

{{ api_signature(value="fn module_id(bytecode: &[u8]) -> ModuleId") }}

A module identity: MurmurHash3 x64 128 of the bytecode with OpenMW's seed.

{{ api_signature(value="struct NativeCodeStats { pub native_code_size_bytes: usize, pub functions_compiled: u32, pub functions_bound: u32 }") }}

{{ api_signature(value="struct NativeCodeResult { pub status: NativeCodeStatus, pub stats: NativeCodeStats, pub module_id: Option<ModuleId> }") }}

Both `Clone`, `Copy`, `Debug`, `Eq`; `NativeCodeStats` also `Default`.

{{ api_signature(value="struct NativeCodeGen") }}

The runtime's generator, from `Runtime::native_code(&self) -> Option<&NativeCodeGen>`. Dropped
after `lua_close`, as Luau requires for shared contexts.

| Method | Meaning |
|---|---|
| `fn for_scope<'s>(scope: &'s impl Scope) -> Option<&'s NativeCodeGen>` | The generator of the runtime whose VM `scope` runs on, from a bound function's `Call` or any other scope; `None` for a runtime without native code |
| `fn is_available(&self) -> bool` | Whether native execution is live on this runtime |
| `fn mode(&self) -> NativeCodeMode` | |
| `fn set_native_execution_enabled(&self, scope: &impl Scope, enabled: bool)` | Turns native execution on or off for the whole VM |
| `fn is_native_execution_enabled(&self, scope: &impl Scope) -> bool` | |
| `fn disable_native_execution_for_function(&self, scope: &impl Scope, level: c_int)` | For the Lua function at call-stack `level` (0 is the innermost), for example from a bound function that detected a problem |
| `fn compile(&self, scope: &impl Scope, index: c_int, bytecode: &[u8]) -> Result<NativeCodeResult>` | Compiles the Lua closure at `index` (and every nested function) natively; `bytecode` identifies the module. A C function or a non-function slot is a logic error |
| `fn assembly(&self, scope: &impl Scope, index: c_int, options: AssemblyOptions) -> Result<String>` | The assembly and/or IR Luau generates for the closure at `index`, as text, with this generator's hooks and types in effect; nothing is installed. Empty output means nothing could be lowered |
| `fn execution_stats(&self, scope: &impl Scope) -> ExecutionStats` | Luau's block counters summed over every retained module (counters must be on) |

`Sandbox::load_template` compiles each template according to the mode and keeps the result on
the `Template`; `source::LoadScope::load_bytecode` and `load_source` compile the chunk they load
on any scope the same way.

{{ api_signature(value="enum AssemblyTarget { Host, A64, A64NoFeatures, X64Windows, X64SystemV }") }}

{{ api_signature(value="struct AssemblyOptions { pub target: AssemblyTarget, pub include_assembly: bool, pub include_ir: bool, pub include_outlined_code: bool, pub include_ir_types: bool, pub include_reg_spills: bool }") }}

What `assembly` prints; the default is host assembly only. `Clone`, `Copy`, `Debug`, `Eq`,
`Default`.

{{ api_signature(value="struct ExecutionStats { pub regular_blocks_executed: u64, pub vm_exits_taken: u64 }") }}

Block counters summed over every module compiled with counters on. `Clone`, `Copy`, `Debug`,
`Default`, `Eq`.

{{ api_signature(value="struct PerfEntry { pub address: usize, pub size: u32, pub symbol: String }") }}

{{ api_signature(value="fn set_perf_log(log: impl Fn(PerfEntry) + Send + 'static)") }}

{{ api_signature(value="fn clear_perf_log()") }}

The process-wide perf log (`Luau::CodeGen::setPerfLog`): every function compiled natively
afterwards, in any runtime, is reported with its code address and size, for profiler
symbolisation.

## Lowering hooks

Module `l3i::native_code::hooks`: Luau's `HostIrHooks` written in Rust. Luau asks two kinds of
question while compiling a function: what bytecode type an operation on a vector or annotated
userdata produces, so the rest of the function specialises on it; and whether the host wants to
lower the operation to IR itself. A `*_type` answer of `bytecode_type::ANY` and a lowering answer
of `false` mean "not mine", and Luau falls back to its generic path. Lowering must obey Luau's
contract for the hook: check tags, take VM exits to `pcpos` on failure, read operands before
writing results. Hooks run inside the code generator: no Lua API, no panics (a panic aborts).

{{ api_signature(value="trait NativeCodeHooks: 'static") }}

Every method has a "not mine" default. `userdata_type` values are `TAGGED_USERDATA_BASE + index`
into the compilation's userdata type names. Implemented for `Rc<H>` too, so one hook set can be
shared between plans.

| Method | Meaning |
|---|---|
| `fn vector_access_type(&self, member: &str) -> u8` | The type of `vector.member` |
| `fn vector_namecall_type(&self, member: &str) -> u8` | The type of `vector:member(...)` |
| `fn vector_access(&self, context: &NativeContext<'_>, build: &mut IrBuilder<'_>, member: &str, site: AccessSite) -> bool` | Lowers `vector.member` |
| `fn vector_namecall(&self, context: &NativeContext<'_>, build: &mut IrBuilder<'_>, member: &str, site: NamecallSite) -> bool` | Lowers `vector:member(...)` |
| `fn userdata_access_type(&self, context: &NativeContext<'_>, userdata_type: u8, member: &str) -> u8` | |
| `fn userdata_metamethod_type(&self, context: &NativeContext<'_>, lhs_type: u8, rhs_type: u8, method: HostMetamethod) -> u8` | |
| `fn userdata_namecall_type(&self, context: &NativeContext<'_>, userdata_type: u8, member: &str) -> u8` | |
| `fn userdata_access(&self, context: &NativeContext<'_>, build: &mut IrBuilder<'_>, userdata_type: u8, member: &str, site: AccessSite) -> bool` | |
| `fn userdata_metamethod(&self, context: &NativeContext<'_>, build: &mut IrBuilder<'_>, site: MetamethodSite) -> bool` | |
| `fn userdata_namecall(&self, context: &NativeContext<'_>, build: &mut IrBuilder<'_>, userdata_type: u8, member: &str, site: NamecallSite) -> bool` | |

{{ api_signature(value="struct NativeContext<'a>") }}

What the VM being compiled for actually assigned. Lowering hooks take their tags from here
rather than from constants, so native code checks the VM's real tags.

| Method | Meaning |
|---|---|
| `fn tag_of<T: Userdata>(&self) -> Option<RuntimeTag>` | The tag this VM assigned to `T` |
| `fn atom_of(&self, name: &str) -> Option<Atom>` | The atom this VM assigned to `name` |
| `fn userdata_type_of<T: Userdata>(&self) -> Option<u8>` | The bytecode type the compiler gives `T` in this runtime, or `None` when `T` is not among the types named to the compiler |

{{ api_signature(value="struct NamecallSite { pub arg_res_reg: c_int, pub source_reg: c_int, pub params: c_int, pub results: c_int, pub pcpos: c_int }") }}

A `value:method(...)` call site: the register of the first argument and of the results, the
receiver's register, the argument count including the receiver, the result count or
`LUA_MULTRET`, and the bytecode position for VM exits.

{{ api_signature(value="struct AccessSite { pub result_reg: c_int, pub source_reg: c_int, pub pcpos: c_int }") }}

{{ api_signature(value="struct MetamethodSite { pub lhs_type: u8, pub rhs_type: u8, pub result_reg: c_int, pub lhs: IrOp, pub rhs: IrOp, pub method: HostMetamethod, pub pcpos: c_int }") }}

A field access and a metamethod operation on userdata operands. All three `Clone`, `Copy`,
`Debug`, `Eq`.

```rust
use l3i::ffi::{LUA_TNUMBER, LUA_TUSERDATA};
use l3i::native_code::hooks::{AccessSite, NativeCodeHooks, NativeContext};
use l3i::native_code::ir::{IrBuilder, IrCmd, bytecode_type};

/// Lowers `p.x` / `p.y` on a `Point` (userdata type index 0) to a tag check and an f32 load.
struct PointFields;

impl NativeCodeHooks for PointFields {
    fn userdata_access_type(&self, _: &NativeContext<'_>, userdata_type: u8, member: &str) -> u8 {
        if userdata_type == bytecode_type::TAGGED_USERDATA_BASE && matches!(member, "x" | "y") {
            bytecode_type::NUMBER
        } else {
            bytecode_type::ANY
        }
    }

    fn userdata_access(
        &self,
        context: &NativeContext<'_>,
        build: &mut IrBuilder<'_>,
        userdata_type: u8,
        member: &str,
        site: AccessSite,
    ) -> bool {
        if userdata_type != bytecode_type::TAGGED_USERDATA_BASE {
            return false;
        }
        // The tag comes from the VM being compiled for, never from a constant.
        let Some(point_tag) = context.tag_of::<Point>() else { return false };
        let offset = match member {
            "x" => 0,
            "y" => 4,
            _ => return false,
        };
        let source = build.vm_reg(site.source_reg);
        let userdata = build.inst(IrCmd::LOAD_POINTER, &[source]);
        let tag = build.const_int(i32::from(point_tag));
        let exit = build.vm_exit(site.pcpos);
        build.inst(IrCmd::CHECK_USERDATA_TAG, &[userdata, tag, exit]);
        let at = build.const_int(offset);
        let userdata_tag = build.const_tag(LUA_TUSERDATA as u8);
        let value = build.inst(IrCmd::BUFFER_READF32, &[userdata, at, userdata_tag]);
        let number = build.inst(IrCmd::FLOAT_TO_NUM, &[value]);
        let result = build.vm_reg(site.result_reg);
        build.inst(IrCmd::STORE_DOUBLE, &[result, number]);
        let number_tag = build.const_tag(LUA_TNUMBER as u8);
        build.inst(IrCmd::STORE_TAG, &[result, number_tag]);
        true
    }
}
```

## The IR builder

Module `l3i::native_code::ir`. `IrCmd`, `IrCondition`, `IrBlockKind` and `HostMetamethod` are
enums generated by `build.rs` from the headers of the exact Luau build (`IrData.h`,
`CodeGenOptions.h`), so a Luau bump that renumbers the IR cannot silently desynchronise this
layer; their variants are Luau's own names (`IrCmd::LOAD_POINTER`, `IrCmd::CHECK_USERDATA_TAG`,
`IrCmd::STORE_DOUBLE`, `IrCondition::Equal`, `IrBlockKind::Internal`, `HostMetamethod::Add`).

{{ api_signature(value="mod bytecode_type") }}

Bytecode type tags a `*_type` hook may answer with (`LuauBytecodeType`): `NIL` 0, `BOOLEAN` 1,
`NUMBER` 2, `STRING` 3, `TABLE` 4, `FUNCTION` 5, `THREAD` 6, `USERDATA` 7, `VECTOR` 8, `BUFFER`
9, `INTEGER` 10, `ANY` 15, `TAGGED_USERDATA_BASE` 64 (userdata type index `i` is
`TAGGED_USERDATA_BASE + i`), `TAGGED_USERDATA_END` 96, `OPTIONAL_BIT` 128.

{{ api_signature(value="struct IrOp") }}

An IR operand: a register, constant, block, instruction result or VM exit. An opaque 32-bit
handle; `IrOp::NONE` is the empty operand. `Clone`, `Copy`, `Debug`, `Eq`, `Hash`.

{{ api_signature(value="struct IrBuilder<'a>") }}

Luau's IR builder for the function being compiled, valid for one hook invocation.

| Method | Meaning |
|---|---|
| `fn inst(&mut self, cmd: IrCmd, ops: &[IrOp]) -> IrOp` | Appends `cmd` with up to eight operands to the current block; more panics |
| `fn undef(&mut self) -> IrOp` | |
| `fn const_int(&mut self, value: c_int) -> IrOp`, `const_int64(i64)`, `const_uint(u32)`, `const_import(u32)`, `const_double(f64)` | Constants |
| `fn const_tag(&mut self, tag: u8) -> IrOp` | A Lua type tag constant (`LUA_TNUMBER` and friends) |
| `fn cond(&mut self, condition: IrCondition) -> IrOp` | |
| `fn block(&mut self, kind: IrBlockKind) -> IrOp` | A new block; the request may be downgraded inside outlined sequences |
| `fn block_at_inst(&mut self, index: u32) -> IrOp`, `fn fallback_block(&mut self, pcpos: u32) -> IrOp` | |
| `fn begin_block(&mut self, block: IrOp)` | |
| `fn load_and_check_tag(&mut self, location: IrOp, tag: u8, fallback: IrOp)` | Loads the tag at `location` and branches to `fallback` unless it is `tag` |
| `fn vm_reg(&mut self, index: c_int) -> IrOp` | VM register `index` (0..=255; more panics) |
| `fn vm_const(&mut self, index: u32) -> IrOp`, `fn vm_upvalue(&mut self, index: u8) -> IrOp` | |
| `fn vm_exit(&mut self, pcpos: c_int) -> IrOp` | The VM exit guards take when a check fails at bytecode position `pcpos` |
| `fn in_terminated_block(&self) -> bool` | |

## The vector buffer writer

Module `l3i::native_code::vector_buffer`.

{{ api_signature(value="struct VectorBufferWriter") }}

The `writef32x3` lowering, part of the default hook set: a statement call `v:writef32x3(buffer,
offset)` becomes three native f32 buffer stores. Any other shape (results wanted, wrong arity, a
non-buffer or non-number argument, an offset that is not an in-range integer) exits to the
interpreter, whose `__namecall` shim from `Runtime::install_vector_buffer_writer` applies the
exact library semantics.

## Analysis

Module `l3i::analysis`, feature `analysis`: Luau's type checker, linter, autocomplete and the
standalone parser over the C++ shim `csrc/analysis.cpp`. Nothing here touches a VM: an
`Analysis` is a `Luau::Frontend` fed by a host `SourceProvider`.

{{ api_signature(value="trait SourceProvider: 'static") }}

What the checker asks the host for.

| Method | Default | Meaning |
|---|---|---|
| `fn read_source(&self, name: &str) -> Option<SourceCode>` | required | The source of module `name`, or `None` |
| `fn resolve_module(&self, requirer: &str, required: &str) -> Option<String>` | `None` | The module a `require(<string>)` inside `requirer` refers to; `None` leaves it unresolved |
| `fn module_config(&self, name: &str) -> ModuleConfig` | default config | |
| `fn human_name(&self, name: &str) -> Option<String>` | `None` | A display name for diagnostics |

{{ api_signature(value="struct SourceCode { pub text: String, pub is_script: bool }") }}

A module's source and whether it is a `require`able module or a top-level script. `Clone`,
`Debug`, `Eq`.

{{ api_signature(value="enum Mode { NoCheck, Nonstrict, Strict }") }}

How unannotated code is treated: no inference; unannotated symbols are `any` (the default);
inferred. `Clone`, `Copy`, `Debug`, `Eq`, `Default`.

{{ api_signature(value="struct ModuleConfig { pub mode: Mode, pub enabled_lints: u64, pub lint_errors: bool, pub type_errors: bool }") }}

Per-module configuration, the `.luaurc` fields the checker reads: `enabled_lints` is a bitmask
of `1 << code` (0 keeps Luau's defaults), `lint_errors` reports lint warnings as errors,
`type_errors` (default true) reports type errors at all. `Clone`, `Debug`, `Eq`, `Default`.

{{ api_signature(value="struct PlanSources<P: SourceProvider>") }}

A provider that serves a runtime plan's modules as typed strict stubs and everything else from
an inner provider, so `require("@dream/...")` in a checked script resolves to the plan's declared
types. Made by `RuntimePlan::analysis_sources`; `fn new(plan: Rc<RuntimePlan>, inner: P) -> Self`
and `fn inner(&self) -> &P`.

{{ api_signature(value="enum Solver { Old, New }") }}

Which constraint solver Luau uses; `New` is the default.

{{ api_signature(value="struct Definitions { pub name: String, pub source: String }") }}

A definitions file (`.d.luau`) loaded into the global scope: `declare extern type`, `declare
name: T`, `export type`. A runtime plan renders one with `RuntimePlan::type_definitions`. `name`
is the package errors are reported under. `Clone`, `Debug`, `Eq`.

{{ api_signature(value="struct AnalysisOptions { pub solver: Solver, pub builtins: bool, pub retain_full_type_graphs: bool, pub definitions: Vec<Definitions> }") }}

| Field | Default | Meaning |
|---|---|---|
| `solver` | `New` | |
| `builtins` | `true` | Register Luau's standard library definitions |
| `retain_full_type_graphs` | `false` | Keep full type information per term (autocomplete-heavy use; costs memory) |
| `definitions` | empty | Definition files loaded after the builtins; one that fails to parse or type check fails `Analysis::new` |

`Clone`, `Debug`, `Eq`, `Default`.

{{ api_signature(value="struct Analysis") }}

A `Luau::Frontend` over a host source provider.

| Method | Meaning |
|---|---|
| `fn new(provider: impl SourceProvider, options: AnalysisOptions) -> Result<Analysis>` | A rejected definitions file is `Error::Runtime` carrying its diagnostics as text (`Definitions were rejected by the Luau frontend:` and one `module:line:column: text` per line) |
| `fn new_reporting(provider: impl SourceProvider, options: AnalysisOptions) -> std::result::Result<Analysis, Vec<Diagnostic>>` | The same with the diagnostics as values (empty when the frontend itself could not be created). Freezes the flag policy first |
| `fn check(&self, module: &str, lint: bool) -> CheckReport` | Type checks `module` and what it requires, with lint checks when `lint` is set |
| `fn mark_dirty(&self, module: &str)` | Forgets the checked state of `module` and its dependents so the next check re-reads it |
| `fn clear(&self)` | Forgets every module |
| `fn autocomplete(&self, module: &str, line: u32, column: u32) -> Result<Completions>` | Completions at a 0-based position, sorted by name |
| `fn provider(&self) -> &dyn SourceProvider` | |

{{ api_signature(value="struct Span { pub begin_line: u32, pub begin_column: u32, pub end_line: u32, pub end_column: u32 }") }}

A source range, 0-based lines and columns, end exclusive. `Clone`, `Copy`, `Debug`, `Eq`,
`Default`.

{{ api_signature(value="enum DiagnosticKind { TypeError, LintWarning, LintError, ParseError, Internal }") }}

`Internal` is the analysis itself failing (an exception inside Luau); `text` is its message.
`Clone`, `Copy`, `Debug`, `Eq`.

{{ api_signature(value="struct Diagnostic { pub kind: DiagnosticKind, pub code: i32, pub name: String, pub module: String, pub text: String, pub span: Span }") }}

`code` is Luau's error or lint code; `name` the lint warning's name (`LocalUnused`), empty
otherwise. `Clone`, `Debug`, `Eq`.

{{ api_signature(value="struct CheckReport { pub diagnostics: Vec<Diagnostic> }") }}

The result of checking one module. `fn errors(&self) -> impl Iterator<Item = &Diagnostic>` is
the type errors, lint errors, parse errors and internal failures (what would fail a build);
`fn is_clean(&self) -> bool`. `Clone`, `Debug`, `Default`, `Eq`.

{{ api_signature(value="enum CompletionKind { Property, Binding, Keyword, String, Type, Module, GeneratedFunction, RequirePath, HotComment, Unknown }") }}

{{ api_signature(value="enum CompletionContext { Unknown, Expression, Statement, Property, Type, Keyword, String, HotComment }") }}

{{ api_signature(value="struct Completion { pub name: String, pub kind: CompletionKind, pub deprecated: bool, pub type_text: String }") }}

{{ api_signature(value="struct Completions { pub context: CompletionContext, pub entries: Vec<Completion> }") }}

Luau's `AutocompleteEntryKind` and `AutocompleteContext`; `type_text` is the entry's type as
Luau prints it, empty for keywords.

{{ api_signature(value="struct ParseReport { pub errors: Vec<Diagnostic>, pub json: Option<String> }") }}

{{ api_signature(value="fn parse(source: &str, with_json: bool) -> ParseReport") }}

Parses `source` standalone (`Luau::Parser::parse`) without a frontend, optionally encoding the
AST as Luau's JSON.

```rust
use std::collections::HashMap;
use l3i::analysis::{Analysis, AnalysisOptions, DiagnosticKind, Mode, ModuleConfig, SourceCode, SourceProvider};

struct Sources {
    modules: HashMap<&'static str, &'static str>,
}

impl SourceProvider for Sources {
    fn read_source(&self, name: &str) -> Option<SourceCode> {
        self.modules.get(name).map(|text| SourceCode { text: (*text).to_owned(), is_script: name == "main" })
    }
    fn resolve_module(&self, _requirer: &str, required: &str) -> Option<String> {
        let name = required.trim_start_matches("./");
        self.modules.contains_key(name).then(|| name.to_owned())
    }
    fn module_config(&self, _name: &str) -> ModuleConfig {
        ModuleConfig { mode: Mode::Strict, ..ModuleConfig::default() }
    }
}

fn main() -> l3i::Result<()> {
    let mut modules = HashMap::new();
    modules.insert("util", "--!strict\nlocal M = {}\nfunction M.double(n: number): number return n * 2 end\nreturn M");
    modules.insert("main", "--!strict\nlocal util = require('./util')\nlocal unused = 1\nlocal text: string = util.double(21)\nprint(text)\n");
    let analysis = Analysis::new(Sources { modules }, AnalysisOptions::default())?;
    let report = analysis.check("main", true);
    assert!(!report.is_clean());
    assert!(report.diagnostics.iter().any(|d| d.kind == DiagnosticKind::TypeError && d.span.begin_line == 3));
    assert!(report.diagnostics.iter().any(|d| d.kind == DiagnosticKind::LintWarning && d.name == "LocalUnused"));
    Ok(())
}
```
