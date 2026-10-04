//! Luau's type checker, linter, autocomplete, and parser through the `analysis` feature.

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

#[test]
fn comprehensions_typecheck_numeric_result_annotations() {
    let mut modules = HashMap::new();
    modules.insert(
        "comprehension",
        "--!strict\nlocal values: { number } = { 1, 2, 3 }\nlocal doubled = [for x in values if x > 1 => x * 2]\nlocal first: number = doubled[1]\nlocal rows: { { value: number? } } = { { value = 1 }, {} }\nlocal compact: { number } = [for row in rows if row.value ~= nil => row.value]\nreturn first + #compact\n",
    );
    let analysis = Analysis::new(Sources { modules }, AnalysisOptions::default()).unwrap();
    let report = analysis.check("comprehension", false);
    assert!(report.is_clean(), "{report:#?}");

    let parsed = analysis::parse("return [for x in values => x * x]", true);
    assert!(parsed.errors.is_empty(), "{parsed:#?}");
    let json = parsed.json.expect("json");
    assert!(json.contains("AstExprFunction") && json.contains("AstStatFor"), "{}", &json[..json.len().min(600)]);
}

fn comprehension_analysis(modules: HashMap<&'static str, &'static str>, solver: analysis::Solver) -> Analysis {
    // check() must retain the scopes that autocomplete subsequently reads.
    Analysis::new(
        Sources { modules },
        AnalysisOptions { solver, retain_full_type_graphs: true, ..AnalysisOptions::default() },
    )
    .unwrap()
}

fn assert_binding_type(analysis: &Analysis, module: &str, line: u32, name: &str, expected: &str) {
    // Query a user binding on a later, untouched line, not a generated local or an annotation.
    let completions = analysis.autocomplete(module, line, 7).unwrap();
    let binding = completions.entries.iter().find(|entry| entry.name == name).expect(name);
    assert_eq!(binding.kind, CompletionKind::Binding, "{binding:?}");
    assert_eq!(binding.type_text, expected, "{module}: {name}");
}

#[test]
fn comprehension_numeric_and_record_results_have_precise_inferred_types() {
    let source = concat!(
        "--!strict\n",
        "local values: { number } = { 1, 2, 3 }\n",
        "local numeric = [for x in values => x * 2]\n",
        "local filtered = [for x in values if x > 1 => x * 2]\n",
        "local records = [for x in values => { value = x * 2, label = tostring(x) }]\n",
        "return numeric, filtered, records\n",
    );
    for solver in [analysis::Solver::New, analysis::Solver::Old] {
        let analysis = comprehension_analysis(HashMap::from([("projections", source)]), solver);
        let report = analysis.check("projections", false);
        assert!(report.is_clean(), "{solver:?}: {report:#?}");
        assert_binding_type(&analysis, "projections", 5, "numeric", "{number}");
        assert_binding_type(&analysis, "projections", 5, "filtered", "{number}");
        assert_binding_type(&analysis, "projections", 5, "records", "{{ label: string, value: number }}");
    }
}

#[test]
fn comprehension_filters_refine_nullable_elements_and_fields_before_projection() {
    let refined = concat!(
        "--!strict\n",
        "local nullable: { number? } = { 1, 2 }\n",
        "local rows: { { value: number? } } = { { value = 1 }, {} }\n",
        "local elements = [for x in nullable if x ~= nil => x]\n",
        "local fields = [for row in rows if row.value ~= nil => row.value]\n",
        "local arithmetic = [for x in nullable if x ~= nil => x * 2]\n",
        "local fieldArithmetic = [for row in rows if row.value ~= nil => row.value + 1]\n",
        "return elements, fields, arithmetic, fieldArithmetic\n",
    );
    let unrefined_element =
        concat!("--!strict\nlocal nullable: { number? } = { 1, 2 }\n", "return [for x in nullable => x * 2]\n",);
    let unrefined_field = concat!(
        "--!strict\nlocal rows: { { value: number? } } = { { value = 1 }, {} }\n",
        "return [for row in rows => row.value + 1]\n",
    );
    for solver in [analysis::Solver::New, analysis::Solver::Old] {
        let analysis = comprehension_analysis(
            HashMap::from([("refined", refined), ("element", unrefined_element), ("field", unrefined_field)]),
            solver,
        );
        let report = analysis.check("refined", false);
        assert!(report.is_clean(), "{solver:?}: {report:#?}");
        for name in ["elements", "fields", "arithmetic", "fieldArithmetic"] {
            assert_binding_type(&analysis, "refined", 7, name, "{number}");
        }
        // A paired negative control: without the filter, the same arithmetic is invalid.
        // The runtime nonnil projection guard is too late to refine its operands.
        for module in ["element", "field"] {
            let report = analysis.check(module, false);
            assert!(!report.is_clean(), "{solver:?}: {module}");
            assert!(report.errors().all(|error| error.kind == DiagnosticKind::TypeError), "{report:#?}");
            assert!(report.errors().any(|error| error.text.contains("number?")), "{report:#?}");
        }
    }
}

#[test]
fn comprehension_diagnostic_columns_currently_refer_to_lowered_source() {
    let projection = concat!(
        "--!strict\nlocal values: { number } = { 1 }\n",
        "local projected = [for x in values => x.missing]\nreturn projected\n",
    );
    let same_line = concat!(
        "--!strict\nlocal values: { number } = { 1 }\n",
        "local projected = [for x in values => x * 2]; local wrong: string = 42\nreturn projected\n",
    );
    let next_line = concat!(
        "--!strict\nlocal values: { number } = { 1 }\n",
        "local projected = [for x in values => x * 2]\nlocal wrong: string = 42\nreturn projected\n",
    );
    let plain = "--!strict\nlocal wrong: string = 42\nreturn wrong\n";
    let analysis = comprehension_analysis(
        HashMap::from([
            ("projection", projection),
            ("same_line", same_line),
            ("next_line", next_line),
            ("plain", plain),
        ]),
        analysis::Solver::New,
    );
    for (module, source, message) in [("projection", projection, "missing"), ("same_line", same_line, "string")] {
        let report = analysis.check(module, false);
        assert_eq!(report.diagnostics.len(), 1, "{report:#?}");
        let error = &report.diagnostics[0];
        assert_eq!(error.kind, DiagnosticKind::TypeError, "{error:?}");
        assert_eq!(error.module, module);
        assert!(error.text.contains(message), "{error:?}");
        assert_eq!((error.span.begin_line, error.span.end_line), (2, 2));
        // Characterize the missing source map without pinning generated names or their length.
        assert!(error.span.begin_column as usize > source.lines().nth(2).unwrap().len(), "{error:?}");
        assert!(error.span.end_column > error.span.begin_column, "{error:?}");
    }
    let plain_report = analysis.check("plain", false);
    let next_report = analysis.check("next_line", false);
    assert_eq!(plain_report.diagnostics.len(), 1, "{plain_report:#?}");
    assert_eq!(next_report.diagnostics.len(), 1, "{next_report:#?}");
    let plain_error = &plain_report.diagnostics[0];
    let next_error = &next_report.diagnostics[0];
    assert_eq!(plain_error.kind, DiagnosticKind::TypeError);
    assert_eq!(next_error.kind, DiagnosticKind::TypeError);
    assert_eq!(next_error.text, plain_error.text);
    assert_eq!((plain_error.span.begin_line, plain_error.span.end_line), (1, 1));
    assert_eq!((next_error.span.begin_line, next_error.span.end_line), (3, 3));
    assert_eq!(next_error.span.begin_column, plain_error.span.begin_column);
    assert_eq!(next_error.span.end_column, plain_error.span.end_column);
    let original_line = next_line.lines().nth(3).unwrap();
    assert_eq!(&original_line[next_error.span.begin_column as usize..next_error.span.end_column as usize], "42");
}
