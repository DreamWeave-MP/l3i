//! Luau's type checker, linter, autocomplete, and parser through the `analysis` feature.
#![cfg(feature = "analysis")]

use std::collections::HashMap;

use l3i::analysis::{
    self, Analysis, AnalysisOptions, CompletionContext, CompletionKind, DiagnosticKind, Mode, ModuleConfig, SourceCode,
    SourceProvider,
};

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
    fn human_name(&self, name: &str) -> Option<String> {
        Some(format!("{name}.luau"))
    }
}

fn sources() -> Sources {
    let mut modules = HashMap::new();
    modules.insert("util", "--!strict\nlocal M = {}\nfunction M.double(n: number): number return n * 2 end\nreturn M");
    modules.insert(
        "main",
        "--!strict\nlocal util = require('./util')\nlocal unused = 1\nlocal text: string = util.double(21)\nprint(text)\n",
    );
    modules.insert(
        "clean",
        "--!strict\nlocal function add(a: number, b: number): number return a + b end\nreturn add(1, 2)",
    );
    Sources { modules }
}

#[test]
fn the_type_checker_reports_errors_across_required_modules_and_the_linter_warns() {
    let analysis = Analysis::new(sources(), AnalysisOptions::default()).unwrap();
    let report = analysis.check("main", true);
    assert!(!report.is_clean());
    let type_error = report.diagnostics.iter().find(|d| d.kind == DiagnosticKind::TypeError).expect("a type error");
    assert!(type_error.text.contains("number") && type_error.text.contains("string"), "{}", type_error.text);
    assert_eq!(type_error.module, "main");
    assert_eq!(type_error.span.begin_line, 3, "{type_error:?}");
    let lint = report.diagnostics.iter().find(|d| d.kind == DiagnosticKind::LintWarning).expect("a lint warning");
    assert_eq!(lint.name, "LocalUnused");
    assert!(lint.text.contains("unused"), "{}", lint.text);
    // Without lint checks only the type error remains (after forgetting the cached result).
    analysis.mark_dirty("main");
    let report = analysis.check("main", false);
    assert_eq!(report.diagnostics.len(), 1, "{report:#?}");
    // A clean module.
    assert!(analysis.check("clean", true).is_clean());
    // Unknown modules are reported, not panicked on.
    let report = analysis.check("missing", true);
    assert!(!report.is_clean(), "{report:?}");
    analysis.mark_dirty("main");
    analysis.clear();
    assert!(!analysis.check("main", false).is_clean());
}

#[test]
fn autocomplete_lists_library_members_and_locals() {
    let mut modules = HashMap::new();
    modules.insert("ac", "local greeting = 'hi'\nlocal x = string.\nlocal y = gree");
    let analysis = Analysis::new(Sources { modules }, AnalysisOptions::default()).unwrap();
    let completions = analysis.autocomplete("ac", 1, 17).unwrap();
    assert_eq!(completions.context, CompletionContext::Property);
    let format = completions.entries.iter().find(|entry| entry.name == "format").expect("string.format");
    assert_eq!(format.kind, CompletionKind::Property);
    assert!(format.type_text.contains("->") || format.type_text.contains("string"), "{}", format.type_text);
    let completions = analysis.autocomplete("ac", 2, 14).unwrap();
    assert!(
        completions.entries.iter().any(|entry| entry.name == "greeting" && entry.kind == CompletionKind::Binding),
        "{completions:?}"
    );
}

#[test]
fn the_parser_reports_syntax_errors_and_encodes_the_ast_as_json() {
    let report = analysis::parse("local x = 1\nreturn x + 1", true);
    assert!(report.errors.is_empty(), "{report:?}");
    let json = report.json.expect("json");
    assert!(json.contains("AstStatLocal") && json.contains("AstStatReturn"), "{}", &json[..json.len().min(300)]);
    let report = analysis::parse("local = 1", false);
    assert_eq!(report.errors.len(), 1);
    assert_eq!(report.errors[0].kind, DiagnosticKind::ParseError);
    assert!(report.errors[0].text.contains("Expected identifier"), "{}", report.errors[0].text);
    assert!(report.json.is_none());
}
