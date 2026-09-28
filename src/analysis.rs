//! Luau's Analysis library (`analysis` feature): the type checker, linter, autocomplete, and
//! the standalone parser, over a C++ shim (`csrc/analysis.cpp`). Nothing here touches a VM: an
//! [`Analysis`] is a `Luau::Frontend` fed by a host [`SourceProvider`].

use std::ffi::{c_char, c_int, c_void};

use crate::error::{Error, Result};

#[allow(non_camel_case_types)]
mod ffi {
    use super::*;

    #[repr(C)]
    pub struct db_analysis {
        _private: [u8; 0],
    }

    pub type db_sink = unsafe extern "C" fn(*mut c_void, *const c_char, usize);
    pub type db_diagnostic_fn = unsafe extern "C" fn(
        *mut c_void,
        c_int,
        c_int,
        *const c_char,
        usize,
        *const c_char,
        usize,
        *const c_char,
        usize,
        u32,
        u32,
        u32,
        u32,
    );
    pub type db_completion_fn =
        unsafe extern "C" fn(*mut c_void, *const c_char, usize, c_int, c_int, *const c_char, usize);

    #[repr(C)]
    pub struct db_module_config {
        pub mode: c_int,
        pub enabled_lint_mask: u64,
        pub lint_errors: c_int,
        pub type_errors: c_int,
    }

    #[repr(C)]
    pub struct db_source_provider {
        pub ctx: *mut c_void,
        pub read_source:
            unsafe extern "C" fn(*mut c_void, *const c_char, usize, db_sink, *mut c_void, *mut c_int) -> c_int,
        pub resolve_module: Option<
            unsafe extern "C" fn(
                *mut c_void,
                *const c_char,
                usize,
                *const c_char,
                usize,
                db_sink,
                *mut c_void,
            ) -> c_int,
        >,
        pub module_config: Option<unsafe extern "C" fn(*mut c_void, *const c_char, usize, *mut db_module_config)>,
        pub human_name: Option<unsafe extern "C" fn(*mut c_void, *const c_char, usize, db_sink, *mut c_void) -> c_int>,
    }

    #[repr(C)]
    pub struct db_definition {
        pub name: *const c_char,
        pub name_length: usize,
        pub source: *const c_char,
        pub source_length: usize,
    }

    #[repr(C)]
    pub struct db_analysis_options {
        pub solver_mode: c_int,
        pub register_builtins: c_int,
        pub retain_full_type_graphs: c_int,
        pub definitions: *const db_definition,
        pub definitions_count: usize,
        pub diagnostic: Option<db_diagnostic_fn>,
        pub diagnostic_ctx: *mut c_void,
    }

    unsafe extern "C" {
        pub fn db_analysis_create(
            provider: *const db_source_provider,
            options: *const db_analysis_options,
        ) -> *mut db_analysis;
        pub fn db_analysis_destroy(analysis: *mut db_analysis);
        pub fn db_analysis_check(
            analysis: *mut db_analysis,
            name: *const c_char,
            name_length: usize,
            lint: c_int,
            diagnostic: db_diagnostic_fn,
            ctx: *mut c_void,
        ) -> c_int;
        pub fn db_analysis_mark_dirty(analysis: *mut db_analysis, name: *const c_char, name_length: usize);
        pub fn db_analysis_clear(analysis: *mut db_analysis);
        pub fn db_analysis_autocomplete(
            analysis: *mut db_analysis,
            name: *const c_char,
            name_length: usize,
            line: u32,
            column: u32,
            completion: db_completion_fn,
            ctx: *mut c_void,
        ) -> c_int;
        pub fn db_parse(
            source: *const c_char,
            length: usize,
            diagnostic: db_diagnostic_fn,
            ctx: *mut c_void,
            json: Option<db_sink>,
            json_ctx: *mut c_void,
        ) -> c_int;
    }
}

/// How a module's unannotated code is treated.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Mode {
    /// No inference at all.
    NoCheck,
    /// Unannotated symbols are `any`.
    #[default]
    Nonstrict,
    /// Unannotated symbols are inferred.
    Strict,
}

/// Per-module configuration (the `.luaurc` fields the checker reads).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModuleConfig {
    pub mode: Mode,
    /// Bitmask of enabled lint warning codes (`1 << code`); `0` keeps Luau's defaults.
    pub enabled_lints: u64,
    /// Report lint warnings as errors.
    pub lint_errors: bool,
    /// Report type errors at all.
    pub type_errors: bool,
}

impl Default for ModuleConfig {
    fn default() -> Self {
        ModuleConfig { mode: Mode::Nonstrict, enabled_lints: 0, lint_errors: false, type_errors: true }
    }
}

/// A module's source and whether it is a `require`able module or a top-level script.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SourceCode {
    pub text: String,
    pub is_script: bool,
}

/// What the checker asks the host for.
pub trait SourceProvider: 'static {
    /// The source of module `name`, or `None` when it does not exist.
    fn read_source(&self, name: &str) -> Option<SourceCode>;
    /// The module a `require(<string>)` inside `requirer` refers to; `None` leaves it unresolved.
    fn resolve_module(&self, requirer: &str, required: &str) -> Option<String> {
        let _ = (requirer, required);
        None
    }
    fn module_config(&self, name: &str) -> ModuleConfig {
        let _ = name;
        ModuleConfig::default()
    }
    /// A display name for diagnostics; the module name by default.
    fn human_name(&self, name: &str) -> Option<String> {
        let _ = name;
        None
    }
}

/// Which constraint solver Luau uses.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Solver {
    Old,
    #[default]
    New,
}

/// A definitions file (`.d.luau`) loaded into the global scope: `declare extern type`,
/// `declare name: T`, `export type`. A runtime plan renders one with `RuntimePlan::type_definitions`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Definitions {
    /// The package name errors are reported under.
    pub name: String,
    pub source: String,
}

/// Options for [`Analysis::new`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AnalysisOptions {
    pub solver: Solver,
    /// Register Luau's standard library definitions (`print`, `string`, ...).
    pub builtins: bool,
    /// Keep full type information per term (needed for autocomplete-heavy use; costs memory).
    pub retain_full_type_graphs: bool,
    /// Definition files loaded after the builtins; one that fails to parse or type check fails
    /// [`Analysis::new`] with its diagnostics in the error text.
    pub definitions: Vec<Definitions>,
}

impl Default for AnalysisOptions {
    fn default() -> Self {
        AnalysisOptions { solver: Solver::New, builtins: true, retain_full_type_graphs: false, definitions: Vec::new() }
    }
}

/// A source range, 0-based lines and columns, end exclusive.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct Span {
    pub begin_line: u32,
    pub begin_column: u32,
    pub end_line: u32,
    pub end_column: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DiagnosticKind {
    TypeError,
    LintWarning,
    LintError,
    ParseError,
    /// The analysis itself failed (an exception inside Luau); `text` is its message.
    Internal,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Diagnostic {
    pub kind: DiagnosticKind,
    /// Luau's error or lint code.
    pub code: i32,
    /// The lint warning's name (`LocalUnused`, ...), empty otherwise.
    pub name: String,
    pub module: String,
    pub text: String,
    pub span: Span,
}

/// The result of checking one module.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CheckReport {
    pub diagnostics: Vec<Diagnostic>,
}

impl CheckReport {
    /// Type errors, lint errors, and parse errors (what would fail a build).
    pub fn errors(&self) -> impl Iterator<Item = &Diagnostic> {
        self.diagnostics.iter().filter(|d| {
            matches!(
                d.kind,
                DiagnosticKind::TypeError
                    | DiagnosticKind::LintError
                    | DiagnosticKind::ParseError
                    | DiagnosticKind::Internal
            )
        })
    }

    pub fn is_clean(&self) -> bool {
        self.errors().next().is_none()
    }
}

/// `Luau::AutocompleteEntryKind`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CompletionKind {
    Property,
    Binding,
    Keyword,
    String,
    Type,
    Module,
    GeneratedFunction,
    RequirePath,
    HotComment,
    Unknown,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Completion {
    pub name: String,
    pub kind: CompletionKind,
    pub deprecated: bool,
    /// The entry's type as Luau prints it, empty for keywords.
    pub type_text: String,
}

/// `Luau::AutocompleteContext`: what kind of thing the cursor position expects.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CompletionContext {
    Unknown,
    Expression,
    Statement,
    Property,
    Type,
    Keyword,
    String,
    HotComment,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Completions {
    pub context: CompletionContext,
    pub entries: Vec<Completion>,
}

/// A `Luau::Frontend` over a host source provider.
pub struct Analysis {
    raw: *mut ffi::db_analysis,
    /// Boxed so the provider's address (the callbacks' `ctx`) stays valid while `Analysis` moves.
    #[allow(clippy::box_collection)]
    provider: Box<Box<dyn SourceProvider>>,
}

unsafe extern "C" fn append_sink(ctx: *mut c_void, data: *const c_char, length: usize) {
    // SAFETY: `ctx` is the String the caller passed; Luau gives `length` bytes at `data`.
    unsafe {
        let target = &mut *ctx.cast::<String>();
        target.push_str(&String::from_utf8_lossy(std::slice::from_raw_parts(data.cast::<u8>(), length)));
    }
}

unsafe fn provider<'a>(ctx: *mut c_void) -> &'a dyn SourceProvider {
    // SAFETY: `ctx` is the inner Box of an `Analysis`, alive as long as its Frontend.
    unsafe { &**ctx.cast_const().cast::<Box<dyn SourceProvider>>() }
}

unsafe fn text<'a>(data: *const c_char, length: usize) -> std::borrow::Cow<'a, str> {
    if data.is_null() {
        return std::borrow::Cow::Borrowed("");
    }
    // SAFETY: Luau passes `length` bytes at `data`.
    String::from_utf8_lossy(unsafe { std::slice::from_raw_parts(data.cast::<u8>(), length) })
}

unsafe fn write(sink: ffi::db_sink, sink_ctx: *mut c_void, value: &str) {
    unsafe { sink(sink_ctx, value.as_ptr().cast(), value.len()) }
}

unsafe extern "C" fn read_source(
    ctx: *mut c_void,
    name: *const c_char,
    name_length: usize,
    sink: ffi::db_sink,
    sink_ctx: *mut c_void,
    kind: *mut c_int,
) -> c_int {
    let _guard = crate::raw::trampoline::AbortOnPanic::new();
    unsafe {
        let Some(source) = provider(ctx).read_source(&text(name, name_length)) else { return 0 };
        write(sink, sink_ctx, &source.text);
        *kind = if source.is_script { 2 } else { 1 };
        1
    }
}

unsafe extern "C" fn resolve_module(
    ctx: *mut c_void,
    requirer: *const c_char,
    requirer_length: usize,
    required: *const c_char,
    required_length: usize,
    sink: ffi::db_sink,
    sink_ctx: *mut c_void,
) -> c_int {
    let _guard = crate::raw::trampoline::AbortOnPanic::new();
    unsafe {
        let Some(resolved) =
            provider(ctx).resolve_module(&text(requirer, requirer_length), &text(required, required_length))
        else {
            return 0;
        };
        write(sink, sink_ctx, &resolved);
        1
    }
}

unsafe extern "C" fn module_config(
    ctx: *mut c_void,
    name: *const c_char,
    name_length: usize,
    out: *mut ffi::db_module_config,
) {
    let _guard = crate::raw::trampoline::AbortOnPanic::new();
    unsafe {
        let config = provider(ctx).module_config(&text(name, name_length));
        *out = ffi::db_module_config {
            mode: match config.mode {
                Mode::NoCheck => 0,
                Mode::Nonstrict => 1,
                Mode::Strict => 2,
            },
            enabled_lint_mask: config.enabled_lints,
            lint_errors: c_int::from(config.lint_errors),
            type_errors: c_int::from(config.type_errors),
        };
    }
}

unsafe extern "C" fn human_name(
    ctx: *mut c_void,
    name: *const c_char,
    name_length: usize,
    sink: ffi::db_sink,
    sink_ctx: *mut c_void,
) -> c_int {
    let _guard = crate::raw::trampoline::AbortOnPanic::new();
    unsafe {
        let Some(human) = provider(ctx).human_name(&text(name, name_length)) else { return 0 };
        write(sink, sink_ctx, &human);
        1
    }
}

#[allow(clippy::too_many_arguments)]
unsafe extern "C" fn collect_diagnostic(
    ctx: *mut c_void,
    kind: c_int,
    code: c_int,
    name: *const c_char,
    name_length: usize,
    module: *const c_char,
    module_length: usize,
    message: *const c_char,
    message_length: usize,
    begin_line: u32,
    begin_column: u32,
    end_line: u32,
    end_column: u32,
) {
    let _guard = crate::raw::trampoline::AbortOnPanic::new();
    unsafe {
        let diagnostics = &mut *ctx.cast::<Vec<Diagnostic>>();
        diagnostics.push(Diagnostic {
            kind: match kind {
                0 => DiagnosticKind::TypeError,
                1 => DiagnosticKind::LintWarning,
                2 => DiagnosticKind::LintError,
                3 => DiagnosticKind::ParseError,
                _ => DiagnosticKind::Internal,
            },
            code,
            name: text(name, name_length).into_owned(),
            module: text(module, module_length).into_owned(),
            text: text(message, message_length).into_owned(),
            span: Span { begin_line, begin_column, end_line, end_column },
        });
    }
}

unsafe extern "C" fn collect_completion(
    ctx: *mut c_void,
    name: *const c_char,
    name_length: usize,
    kind: c_int,
    deprecated: c_int,
    type_text: *const c_char,
    type_length: usize,
) {
    let _guard = crate::raw::trampoline::AbortOnPanic::new();
    unsafe {
        let entries = &mut *ctx.cast::<Vec<Completion>>();
        entries.push(Completion {
            name: text(name, name_length).into_owned(),
            kind: match kind {
                0 => CompletionKind::Property,
                1 => CompletionKind::Binding,
                2 => CompletionKind::Keyword,
                3 => CompletionKind::String,
                4 => CompletionKind::Type,
                5 => CompletionKind::Module,
                6 => CompletionKind::GeneratedFunction,
                7 => CompletionKind::RequirePath,
                8 => CompletionKind::HotComment,
                _ => CompletionKind::Unknown,
            },
            deprecated: deprecated != 0,
            type_text: text(type_text, type_length).into_owned(),
        });
    }
}

impl Analysis {
    /// A frontend over `provider`.
    pub fn new(provider: impl SourceProvider, options: AnalysisOptions) -> Result<Analysis> {
        // The frontend parses Luau's builtin definitions under the process-wide fast flags; freeze
        // the policy here too, or a runtime created on another thread meanwhile flips flags under
        // that parse (Luau then fails its `loadResult.success` assertion).
        crate::flags::initialize()?;
        let provider: Box<Box<dyn SourceProvider>> = Box::new(Box::new(provider));
        let raw_provider = ffi::db_source_provider {
            ctx: (&*provider as *const Box<dyn SourceProvider>).cast_mut().cast(),
            read_source,
            resolve_module: Some(resolve_module),
            module_config: Some(module_config),
            human_name: Some(human_name),
        };
        let definitions: Vec<ffi::db_definition> = options
            .definitions
            .iter()
            .map(|d| ffi::db_definition {
                name: d.name.as_ptr().cast(),
                name_length: d.name.len(),
                source: d.source.as_ptr().cast(),
                source_length: d.source.len(),
            })
            .collect();
        let mut diagnostics: Vec<Diagnostic> = Vec::new();
        let raw_options = ffi::db_analysis_options {
            solver_mode: match options.solver {
                Solver::Old => 0,
                Solver::New => 1,
            },
            register_builtins: c_int::from(options.builtins),
            retain_full_type_graphs: c_int::from(options.retain_full_type_graphs),
            definitions: definitions.as_ptr(),
            definitions_count: definitions.len(),
            diagnostic: Some(collect_diagnostic),
            diagnostic_ctx: (&raw mut diagnostics).cast(),
        };
        // SAFETY: the shim copies the provider table and reads the definitions during the call;
        // the provider Box outlives the frontend and the diagnostics Vec outlives the call.
        let raw = unsafe { ffi::db_analysis_create(&raw_provider, &raw_options) };
        if raw.is_null() {
            if diagnostics.is_empty() {
                return Err(Error::runtime("Unable to create the Luau analysis frontend"));
            }
            let text: Vec<String> = diagnostics
                .iter()
                .map(|d| format!("{}:{}:{}: {}", d.module, d.span.begin_line + 1, d.span.begin_column + 1, d.text))
                .collect();
            return Err(Error::runtime(format!("Definitions were rejected by the Luau frontend:\n{}", text.join("\n"))));
        }
        Ok(Analysis { raw, provider })
    }

    /// Type checks `module` (and what it requires), with lint checks when `lint` is set.
    pub fn check(&self, module: &str, lint: bool) -> CheckReport {
        let mut diagnostics: Vec<Diagnostic> = Vec::new();
        // SAFETY: the frontend is live; the callback fills our Vec.
        unsafe {
            ffi::db_analysis_check(
                self.raw,
                module.as_ptr().cast(),
                module.len(),
                c_int::from(lint),
                collect_diagnostic,
                (&mut diagnostics as *mut Vec<Diagnostic>).cast(),
            );
        }
        CheckReport { diagnostics }
    }

    /// Forgets the checked state of `module` (and its dependents) so the next check re-reads it.
    pub fn mark_dirty(&self, module: &str) {
        unsafe { ffi::db_analysis_mark_dirty(self.raw, module.as_ptr().cast(), module.len()) }
    }

    /// Forgets every module.
    pub fn clear(&self) {
        unsafe { ffi::db_analysis_clear(self.raw) }
    }

    /// Completions at `line`/`column` (0-based) of `module`.
    pub fn autocomplete(&self, module: &str, line: u32, column: u32) -> Result<Completions> {
        let mut entries: Vec<Completion> = Vec::new();
        let context = unsafe {
            ffi::db_analysis_autocomplete(
                self.raw,
                module.as_ptr().cast(),
                module.len(),
                line,
                column,
                collect_completion,
                (&mut entries as *mut Vec<Completion>).cast(),
            )
        };
        let context = match context {
            0 => CompletionContext::Unknown,
            1 => CompletionContext::Expression,
            2 => CompletionContext::Statement,
            3 => CompletionContext::Property,
            4 => CompletionContext::Type,
            5 => CompletionContext::Keyword,
            6 => CompletionContext::String,
            7 => CompletionContext::HotComment,
            _ => return Err(Error::runtime("Luau autocomplete failed")),
        };
        entries.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(Completions { context, entries })
    }

    /// The provider this analysis reads from.
    pub fn provider(&self) -> &dyn SourceProvider {
        &**self.provider
    }
}

impl Drop for Analysis {
    fn drop(&mut self) {
        // SAFETY: created by db_analysis_create; the provider Box is dropped afterwards.
        unsafe { ffi::db_analysis_destroy(self.raw) };
    }
}

/// The result of parsing one chunk without a frontend.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ParseReport {
    pub errors: Vec<Diagnostic>,
    /// The AST as Luau's JSON encoding, when requested and the parse produced a tree.
    pub json: Option<String>,
}

/// Parses `source` standalone (`Luau::Parser::parse`), optionally encoding the AST as JSON.
pub fn parse(source: &str, with_json: bool) -> ParseReport {
    let mut errors: Vec<Diagnostic> = Vec::new();
    let mut json = String::new();
    // SAFETY: callbacks fill our locals; the shim catches Luau's exceptions.
    let status = unsafe {
        ffi::db_parse(
            source.as_ptr().cast(),
            source.len(),
            collect_diagnostic,
            (&mut errors as *mut Vec<Diagnostic>).cast(),
            if with_json { Some(append_sink) } else { None },
            (&mut json as *mut String).cast(),
        )
    };
    if status < 0 && !errors.iter().any(|error| error.kind == DiagnosticKind::Internal) {
        errors.push(Diagnostic {
            kind: DiagnosticKind::Internal,
            code: 0,
            name: String::new(),
            module: String::new(),
            text: "the Luau parser failed without a message".to_owned(),
            span: Span::default(),
        });
    }
    ParseReport { errors, json: (with_json && !json.is_empty()).then_some(json) }
}
