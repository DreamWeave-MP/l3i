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
fn comprehension_sum_reducer_infers_number_and_preserves_filter_refinement() {
    let source = concat!(
        "--!strict\n",
        "local values: { number? } = { 1, 2, 3 }\n",
        "local total = sum[for x in values if x ~= nil if x > 1 => x * 2]\n",
        "local check: number = total\n",
        "return total, check\n",
    );
    for solver in [analysis::Solver::New, analysis::Solver::Old] {
        let analysis = comprehension_analysis(HashMap::from([("sum", source)]), solver);
        let report = analysis.check("sum", false);
        assert!(report.is_clean(), "{solver:?}: {report:#?}");
        assert_binding_type(&analysis, "sum", 4, "total", "number");
    }
}

#[test]
fn numeric_range_generators_infer_numbers_without_a_range_global() {
    let source = "--!strict\nlocal values = [for i in range(1, 5) => i * 2]\nlocal total = sum[for i in range(1, 5, 2) => i]\nreturn values, total\n";
    for solver in [analysis::Solver::New, analysis::Solver::Old] {
        let analysis = comprehension_analysis(HashMap::from([("range", source)]), solver);
        let report = analysis.check("range", false);
        assert!(report.is_clean(), "{solver:?}: {report:#?}");
        assert_binding_type(&analysis, "range", 3, "values", "{number}");
        assert_binding_type(&analysis, "range", 3, "total", "number");
    }
}

#[test]
fn enumerate_generators_infer_numeric_index_and_value_bindings() {
    let source = concat!(
        "--!strict\n",
        "local values: { number } = { 1, 2, 3 }\n",
        "local pairs = [for i, value in enumerate(values) => i + value]\n",
        "local total = sum[for i, value in enumerate(values) => i + value]\n",
        "return pairs, total\n",
    );
    for solver in [analysis::Solver::New, analysis::Solver::Old] {
        let analysis = comprehension_analysis(HashMap::from([("enumerate", source)]), solver);
        let report = analysis.check("enumerate", false);
        assert!(report.is_clean(), "{solver:?}: {report:#?}");
        assert_binding_type(&analysis, "enumerate", 4, "pairs", "{number}");
        assert_binding_type(&analysis, "enumerate", 5, "total", "number");
    }
}

#[test]
fn slices_preserve_source_types_and_infer_fused_results() {
    let source = concat!(
        "--!strict\n",
        "local values: { number } = { 1, 2, 3, 4 }\n",
        "local selected = [for x in values[2:3] => x * 2]\n",
        "local total = sum[for x in values[2:3] => x]\n",
        "return selected, total\n",
    );
    for solver in [analysis::Solver::New, analysis::Solver::Old] {
        let analysis = comprehension_analysis(HashMap::from([("slice", source)]), solver);
        let report = analysis.check("slice", false);
        assert!(report.is_clean(), "{solver:?}: {report:#?}");
        assert_binding_type(&analysis, "slice", 4, "selected", "{number}");
        assert_binding_type(&analysis, "slice", 5, "total", "number");
    }
}

#[test]
fn standalone_slices_infer_dense_source_element_types() {
    let source = "--!strict\nlocal values: { number } = { 1, 2, 3 }\nlocal part = values[1:2]\nreturn part\n";
    for solver in [analysis::Solver::New, analysis::Solver::Old] {
        let analysis = comprehension_analysis(HashMap::from([("standalone_slice", source)]), solver);
        let report = analysis.check("standalone_slice", false);
        assert!(report.is_clean(), "{solver:?}: {report:#?}");
        assert_binding_type(&analysis, "standalone_slice", 4, "part", "{number}");
    }
}

#[test]
fn standalone_buffer_slices_preserve_buffer_type() {
    let source = "--!strict\nlocal values: buffer = buffer.create(4)\nlocal part = values[1:2]\nreturn part\n";
    for solver in [analysis::Solver::New, analysis::Solver::Old] {
        let analysis = comprehension_analysis(HashMap::from([("buffer_slice", source)]), solver);
        let report = analysis.check("buffer_slice", false);
        assert!(report.is_clean(), "{solver:?}: {report:#?}");
        assert_binding_type(&analysis, "buffer_slice", 4, "part", "buffer");
    }
}

#[test]
fn buffer_slice_comprehensions_specialize_after_luau_infers_the_source() {
    let source = concat!(
        "--!strict\n",
        "local values: buffer = buffer.create(4)\n",
        "local bytes = [for byte in values[1:4] => byte * 2]\n",
        "local total = sum[for byte in values[1:4] => byte]\n",
        "return bytes, total\n",
    );
    for solver in [analysis::Solver::New, analysis::Solver::Old] {
        let analysis = comprehension_analysis(HashMap::from([("buffer_fusion", source)]), solver);
        let report = analysis.check("buffer_fusion", false);
        assert!(report.is_clean(), "{solver:?}: {report:#?}");
        assert_binding_type(&analysis, "buffer_fusion", 4, "bytes", "{number}");
        assert_binding_type(&analysis, "buffer_fusion", 5, "total", "number");
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
fn comprehension_entity_ids_nullable_records_and_nested_results_infer() {
    let source = concat!(
        "--!strict\n",
        "type Entity = { id: number, active: boolean }\n",
        "local entities: {Entity} = {{id = 1, active = true}}\n",
        "local ids = [for e in entities if e.active => e.id]\n",
        "type Foo = {name: string}\n",
        "local nullable: {Foo?} = {{name = 'a'}}\n",
        "local names = [for x in nullable if x ~= nil => x.name]\n",
        "local nested = [for row in {{1, 2}, {3, 4}} => [for x in row => x * 2]]\n",
        "local pairs = [for a in {1, 2} for b in {3, 4} => a + b]\n",
        "return ids, names, nested, pairs\n",
    );
    for solver in [analysis::Solver::New, analysis::Solver::Old] {
        let analysis = comprehension_analysis(HashMap::from([("entities", source)]), solver);
        let report = analysis.check("entities", false);
        assert!(report.is_clean(), "{solver:?}: {report:#?}");
        for (name, expected) in
            [("ids", "{number}"), ("names", "{string}"), ("nested", "{{number}}"), ("pairs", "{number}")]
        {
            assert_binding_type(&analysis, "entities", 9, name, expected);
        }
    }
}

#[test]
fn comprehension_diagnostics_refer_to_original_source() {
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
    for (module, source, message, marked) in
        [("projection", projection, "missing", "x.missing"), ("same_line", same_line, "string", "42")]
    {
        let report = analysis.check(module, false);
        assert_eq!(report.diagnostics.len(), 1, "{report:#?}");
        let error = &report.diagnostics[0];
        assert_eq!(error.kind, DiagnosticKind::TypeError, "{error:?}");
        assert_eq!(error.module, module);
        assert!(error.text.contains(message), "{error:?}");
        assert_eq!((error.span.begin_line, error.span.end_line), (2, 2));
        let original = source.lines().nth(2).unwrap();
        let begin = original.find(marked).unwrap();
        assert_eq!(error.span.begin_column as usize, begin, "{error:?}");
        assert_eq!(error.span.end_column as usize, begin + marked.len(), "{error:?}");
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

// Count bytes, not Unicode scalars; CRLF still advances the line only at LF.
fn source_position(source: &str, offset: usize) -> (u32, u32) {
    let prefix = &source[..offset];
    let line = prefix.bytes().filter(|byte| *byte == b'\n').count();
    let column = prefix.rfind('\n').map_or(offset, |newline| offset - newline - 1);
    (u32::try_from(line).unwrap(), u32::try_from(column).unwrap())
}

fn token_span(source: &str, token: &str) -> analysis::Span {
    let begin = source.find(token).expect(token);
    assert!(!source[begin + token.len()..].contains(token), "ambiguous token: {token}");
    let (begin_line, begin_column) = source_position(source, begin);
    let (end_line, end_column) = source_position(source, begin + token.len());
    analysis::Span { begin_line, begin_column, end_line, end_column }
}

fn assert_diagnostic_span(error: &analysis::Diagnostic, source: &str, token: &str) {
    assert_eq!(error.span, token_span(source, token), "{error:#?}");
}

#[test]
fn comprehension_multiline_nested_type_errors_use_original_byte_spans() {
    let source = concat!(
        "--!strict\r\nlocal rows: {{number}} = {{1}}\r\n",
        "return [for row in rows =>\r\n",
        "    [for x in row if x > 0 =>\r\n",
        "        ({label = 'é🦀', value = x.\r\n            missing})\r\n",
        "    ]\r\n]\r\n",
    );
    for solver in [analysis::Solver::New, analysis::Solver::Old] {
        let analysis = comprehension_analysis(HashMap::from([("nested_error", source)]), solver);
        let report = analysis.check("nested_error", false);
        assert_eq!(report.diagnostics.len(), 1, "{solver:?}: {report:#?}");
        let error = &report.diagnostics[0];
        assert_eq!(error.kind, DiagnosticKind::TypeError);
        assert_eq!(error.module, "nested_error");
        assert!(error.text.contains("missing"), "{error:?}");
        assert_diagnostic_span(error, source, "x.\r\n            missing");
    }
}

#[test]
fn comprehension_copied_parse_and_lint_errors_use_original_spans() {
    let malformed = concat!(
        "local rows = {{1}}\nreturn [for row in rows =>\n",
        "    [for x in row => { label = 'é', value = x + * 2 }]\n]\n",
    );
    let parsed = analysis::parse(malformed, false);
    assert_eq!(parsed.errors.len(), 1, "{parsed:#?}");
    let error = parsed.errors.iter().find(|error| error.text.contains("expression")).expect("expression error");
    assert_eq!(error.kind, DiagnosticKind::ParseError, "{parsed:#?}");
    assert_diagnostic_span(error, malformed, "*");
    let analysis = comprehension_analysis(HashMap::from([("parse_error", malformed)]), analysis::Solver::New);
    let checked = analysis.check("parse_error", false);
    assert_eq!(checked.diagnostics.len(), 1, "{checked:#?}");
    let error = checked.diagnostics.iter().find(|error| error.text.contains("expression")).expect("expression error");
    assert_eq!(error.kind, DiagnosticKind::ParseError, "{checked:#?}");
    assert_eq!(error.module, "parse_error");
    assert_diagnostic_span(error, malformed, "*");

    let lint_source = concat!(
        "local rows = {{1}}\r\nreturn [for row in rows =>\r\n",
        "    [for x in row => (function()\r\n",
        "        local label = 'é'; local forgotten = x\r\n",
        "        return label\r\n",
        "    end)()]\r\n]\r\n",
    );
    let analysis = comprehension_analysis(HashMap::from([("lint_error", lint_source)]), analysis::Solver::New);
    let report = analysis.check("lint_error", true);
    assert!(report.is_clean(), "{report:#?}");
    let warnings: Vec<_> = report.diagnostics.iter().filter(|error| error.name == "LocalUnused").collect();
    assert_eq!(warnings.len(), 1, "{report:#?}");
    assert_eq!(warnings[0].kind, DiagnosticKind::LintWarning);
    assert_diagnostic_span(warnings[0], lint_source, "forgotten");
}

#[test]
fn required_comprehension_errors_use_the_diagnostic_module_map() {
    let main = "--!strict\nlocal padding = [for n in {1} => n * 2]; return require('./dependency')\n";
    let dependency =
        concat!("--!strict\n\nlocal values: {number} = {1}\n", "return [for x in values =>\n    x.absent]\n",);
    let analysis =
        comprehension_analysis(HashMap::from([("main", main), ("dependency", dependency)]), analysis::Solver::New);
    let report = analysis.check("main", false);
    assert_eq!(report.diagnostics.len(), 1, "{report:#?}");
    let error = &report.diagnostics[0];
    assert_eq!(error.module, "dependency", "{error:?}");
    assert_eq!(error.kind, DiagnosticKind::TypeError);
    assert_diagnostic_span(error, dependency, "x.absent");
}

#[test]
fn comprehension_autocomplete_maps_projection_filter_and_later_same_line_cursors() {
    let source = concat!(
        "--!strict\nlocal rows: {{score: number, enabled: boolean}} = {{score = 1, enabled = true}}\n",
        "local first = [for r in {{warmup = 1}} => r.warmup]; local sameLine = [for item in {{later = 'a'}} => item.later]; local second = [for row in rows if row.enabled =>\n",
        "    row.score]\nreturn first, sameLine, second\n",
    );
    let analysis = comprehension_analysis(HashMap::from([("cursor", source)]), analysis::Solver::New);
    assert!(analysis.check("cursor", false).is_clean());
    for (token, expected) in [
        ("r.warmup", &["warmup"][..]),
        ("item.later", &["later"][..]),
        ("row.enabled", &["enabled", "score"][..]),
        ("row.score", &["enabled", "score"][..]),
    ] {
        // The original cursor is immediately after the dot, including the filter
        // that follows another expansion on the very same source line.
        let offset = source.find(token).unwrap() + token.find('.').unwrap() + 1;
        let (line, column) = source_position(source, offset);
        let completions = analysis.autocomplete("cursor", line, column).unwrap();
        assert_eq!(completions.context, CompletionContext::Property, "{token}: {completions:?}");
        let mut properties: Vec<_> = completions
            .entries
            .iter()
            .filter(|entry| entry.kind == CompletionKind::Property)
            .map(|entry| entry.name.as_str())
            .collect();
        properties.sort_unstable();
        assert_eq!(properties, expected, "{token}: {completions:?}");
    }
}

#[test]
fn comprehension_autocomplete_hides_only_synthetic_bindings() {
    let source = concat!(
        "local __l3i_comp_0_out = 17\nlocal __l3i_comp_999_value = 'user'\n",
        "local rows = {{1}}\nreturn [for row in rows => [for x in row =>\n",
        "    x + __l3i_comp_0_out]]\n",
    );
    let analysis = comprehension_analysis(HashMap::from([("bindings", source)]), analysis::Solver::New);
    assert!(analysis.check("bindings", false).is_clean());
    let (line, column) = source_position(source, source.find("x +").unwrap());
    let completions = analysis.autocomplete("bindings", line, column).unwrap();
    for name in ["x", "row", "__l3i_comp_0_out", "__l3i_comp_999_value"] {
        assert!(
            completions.entries.iter().any(|entry| entry.name == name && entry.kind == CompletionKind::Binding),
            "{completions:?}"
        );
    }
    let mut prefixed: Vec<_> = completions
        .entries
        .iter()
        .filter(|entry| entry.name.starts_with("__l3i_comp_"))
        .map(|entry| entry.name.as_str())
        .collect();
    prefixed.sort_unstable();
    assert_eq!(prefixed, ["__l3i_comp_0_out", "__l3i_comp_999_value"], "{completions:?}");
}

struct ChangingSource(std::rc::Rc<std::cell::RefCell<&'static str>>);

impl SourceProvider for ChangingSource {
    fn read_source(&self, name: &str) -> Option<SourceCode> {
        (name == "snapshot").then(|| SourceCode { text: (*self.0.borrow()).to_owned(), is_script: false })
    }
    fn module_config(&self, _name: &str) -> ModuleConfig {
        ModuleConfig { mode: Mode::Strict, ..ModuleConfig::default() }
    }
}

#[test]
fn comprehension_cached_ast_keeps_its_map_until_dirty_or_clear() {
    let old = "--!strict\nreturn [for x in {1} => x.oldField]\n";
    let new = "--!strict\n\nlocal prefix = [for x in {1} => x]; return [for y in {2} =>\n    y.newField]\n";
    let plain = "--!strict\n\n\nlocal wrong: string = 42\nreturn wrong\n";
    let source = std::rc::Rc::new(std::cell::RefCell::new(old));
    let analysis = Analysis::new(ChangingSource(source.clone()), AnalysisOptions::default()).unwrap();
    let check_snapshot = |expected: &str, token: &str| {
        let report = analysis.check("snapshot", false);
        assert_eq!(report.diagnostics.len(), 1, "{report:#?}");
        assert_eq!(report.diagnostics[0].kind, DiagnosticKind::TypeError);
        assert_diagnostic_span(&report.diagnostics[0], expected, token);
    };
    check_snapshot(old, "x.oldField");
    *source.borrow_mut() = new;
    // check() probes current existence, but must not replace the cached AST's map.
    check_snapshot(old, "x.oldField");
    analysis.mark_dirty("snapshot");
    check_snapshot(new, "y.newField");
    *source.borrow_mut() = plain;
    check_snapshot(new, "y.newField");
    analysis.clear();
    check_snapshot(plain, "42");
    *source.borrow_mut() = old;
    analysis.mark_dirty("snapshot");
    check_snapshot(old, "x.oldField");
}

fn json_location(span: analysis::Span) -> String {
    format!("\"location\":\"{},{} - {},{}\"", span.begin_line, span.begin_column, span.end_line, span.end_column)
}

#[test]
fn comprehension_json_preserves_lowered_kinds_with_original_expression_and_annotation_spans() {
    let source = concat!(
        "local rows = {{1}}\r\n",
        "local result = [for row in rows => [for x in row =>\r\n",
        "    x * 37]]; local annotated: {number} = {1}\r\nreturn result, annotated\r\n",
    );
    let parsed = analysis::parse(source, true);
    assert!(parsed.errors.is_empty(), "{parsed:#?}");
    let json = parsed.json.unwrap();
    assert!(json.contains("\"type\":\"AstExprFunction\"") && json.contains("\"type\":\"AstStatFor\""), "{json}");
    let projection = format!("\"type\":\"AstExprBinary\",{}", json_location(token_span(source, "x * 37")));
    assert!(json.contains(&projection), "missing copied projection {projection}: {json}");
    let annotation = format!("\"type\":\"AstTypeTable\",{}", json_location(token_span(source, "{number}")));
    assert!(json.contains(&annotation), "missing user annotation {annotation}: {json}");
    // AstLocal's location covers the identifier, not the colon or annotation.
    let mut identifier_span = token_span(source, "annotated:");
    identifier_span.end_column -= 1;
    let local =
        format!("\"name\":\"annotated\",\"isConst\":false,\"type\":\"AstLocal\",{}", json_location(identifier_span));
    assert!(json.contains(&local), "missing user local {local}: {json}");
}

#[test]
fn parser_eof_and_secondary_references_are_original_not_lowered() {
    let source = "local xs = [\n for x in {1}\n => x\n]\nfunction broken()\n return xs";
    for suffix in ["", "\n"] {
        let source = format!("{source}{suffix}");
        let parsed = analysis::parse(&source, false);
        assert_eq!(parsed.errors.len(), 1, "{parsed:?}");
        let error = &parsed.errors[0];
        let (line, column) = source_position(&source, source.len());
        assert_eq!(
            error.span,
            analysis::Span { begin_line: line, begin_column: column, end_line: line, end_column: column }
        );
        assert!(error.text.contains("to close 'function' at line 5"), "{error:?}");
    }
    let analysis = comprehension_analysis(HashMap::from([("eof", source)]), analysis::Solver::New);
    let checked = analysis.check("eof", false);
    assert_eq!(checked.diagnostics.len(), 1, "{checked:?}");
    assert!(checked.diagnostics[0].text.contains("to close 'function' at line 5"), "{checked:?}");
}

#[test]
fn duplicate_type_secondary_location_is_remapped_before_formatting() {
    let source = concat!(
        "--!strict\nlocal xs = [\n for x in {1}\n => x\n]\n",
        "type Repeated = number\n",
        "type Repeated = string\nreturn xs\n",
    );
    for solver in [analysis::Solver::New, analysis::Solver::Old] {
        let analysis = comprehension_analysis(HashMap::from([("duplicate", source)]), solver);
        let report = analysis.check("duplicate", false);
        assert_eq!(report.diagnostics.len(), 1, "{report:?}");
        let error = &report.diagnostics[0];
        assert_eq!(error.kind, DiagnosticKind::TypeError);
        assert_eq!(error.span.begin_line, 6, "{error:?}");
        assert!(error.text.contains("previously defined at line 6"), "{error:?}");
    }
}

#[test]
fn normal_and_autocomplete_caches_share_the_same_mapped_snapshot() {
    let old = concat!(
        "--!strict\nlocal rows: {{oldOnly: number}} = {{oldOnly = 1}}\n",
        "local result = [for row in rows => row.oldOnly]; local wrong: number = 'old error'\nreturn result\n",
    );
    let new = concat!(
        "--!strict\n\nlocal rows: {{newOnly: number}} = {{newOnly = 2}}\n",
        "local prefix = [for x in {1} => x]; local result = [for row in rows =>\n",
        "    row.newOnly]; local wrong: number = 'new error'\nreturn result\n",
    );
    let plain = "--!strict\nlocal row: {plainOnly: number} = {plainOnly = 3}\nlocal wrong: number = 'plain error'\nreturn row.plainOnly\n";
    for solver in [analysis::Solver::New, analysis::Solver::Old] {
        let source = std::rc::Rc::new(std::cell::RefCell::new(old));
        let analysis = Analysis::new(
            ChangingSource(source.clone()),
            AnalysisOptions { solver, retain_full_type_graphs: true, ..AnalysisOptions::default() },
        )
        .unwrap();
        let check = |expected: &str, token: &str| {
            let report = analysis.check("snapshot", false);
            assert_eq!(report.diagnostics.len(), 1, "{solver:?}: {report:?}");
            // The old solver highlights the local statement; the new one highlights its RHS.
            let marked = if solver == analysis::Solver::Old {
                format!("local wrong: number = {token}")
            } else {
                token.to_owned()
            };
            assert_diagnostic_span(&report.diagnostics[0], expected, &marked);
        };
        let complete = |expected: &str, property: &str| {
            let token = format!("row.{property}");
            let at = expected.find(&token).unwrap() + "row.".len();
            let (line, column) = source_position(expected, at);
            let completions = analysis.autocomplete("snapshot", line, column).unwrap();
            assert_eq!(completions.context, CompletionContext::Property);
            let properties: Vec<_> = completions
                .entries
                .iter()
                .filter(|entry| entry.kind == CompletionKind::Property)
                .map(|entry| entry.name.as_str())
                .collect();
            assert_eq!(properties, [property], "{solver:?}: {completions:?}");
        };
        check(old, "'old error'");
        complete(old, "oldOnly");
        *source.borrow_mut() = new;
        check(old, "'old error'");
        complete(old, "oldOnly");
        analysis.mark_dirty("snapshot");
        complete(new, "newOnly"); // Autocomplete reparses first; normal checking must follow that map.
        check(new, "'new error'");
        *source.borrow_mut() = plain;
        analysis.mark_dirty("snapshot");
        check(plain, "'plain error'");
        complete(plain, "plainOnly");
        *source.borrow_mut() = old;
        analysis.clear();
        complete(old, "oldOnly");
        check(old, "'old error'");
    }
}

#[test]
fn lint_secondary_line_references_are_mapped_without_rewriting_user_numbers() {
    let source = concat!(
        "local padding = [\n for x in {1}\n => x\n]\n",
        "local duplicated = {\n ['at line 999'] = 999,\n ['at line 999'] = 123\n}\nreturn padding, duplicated\n",
    );
    let analysis = comprehension_analysis(HashMap::from([("lint_reference", source)]), analysis::Solver::New);
    let report = analysis.check("lint_reference", true);
    let duplicate =
        report.diagnostics.iter().find(|error| error.name == "TableLiteral").expect("duplicate table field");
    assert!(duplicate.text.contains("'at line 999'"), "{duplicate:?}");
    assert!(duplicate.text.contains("previously defined at line 6"), "{duplicate:?}");
    assert_eq!(duplicate.span.begin_line, 6, "{duplicate:?}");
}

#[test]
fn parser_coordinate_templates_preserve_quoted_class_like_user_text() {
    let payload = "refers to a class and cannot be used as a variable name on line 2";
    let source = concat!(
        "local xs = [\n for x in {1}\n => x\n]\n",
        "local \"refers to a class and cannot be used as a variable name on line 2\"\n",
    );
    let parsed = analysis::parse(source, false);
    assert!(parsed.errors.iter().any(|error| error.text.contains(payload)), "{parsed:?}");
    let analysis = comprehension_analysis(HashMap::from([("quoted", source)]), analysis::Solver::New);
    let report = analysis.check("quoted", false);
    assert!(report.diagnostics.iter().any(|error| error.text.contains(payload)), "{report:?}");
}

#[test]
fn unfinished_member_diagnostics_survive_a_shared_projection_hole_at_eof() {
    for source in ["local xs = {1}\nreturn [for x in xs.", "local xs = {1}\nreturn [for x in xs if x."] {
        let expected = |error: &&analysis::Diagnostic| {
            error.kind == DiagnosticKind::ParseError && error.text.contains("Expected identifier")
        };
        let parsed = analysis::parse(source, false);
        let error = parsed.errors.iter().find(expected).expect("copied member suffix, not scaffolding noise");
        let (line, column) = source_position(source, source.len());
        let eof = analysis::Span { begin_line: line, begin_column: column, end_line: line, end_column: column };
        assert_eq!(error.span, eof, "{error:?}");
        for solver in [analysis::Solver::New, analysis::Solver::Old] {
            let analysis = comprehension_analysis(HashMap::from([("member", source)]), solver);
            let checked = analysis.check("member", false);
            let error =
                checked.diagnostics.iter().find(expected).expect("checking must retain the copied member diagnostic");
            assert_eq!(error.span, eof, "{solver:?}: {error:?}");
        }
    }
}

fn complete_at(analysis: &Analysis, module: &str, source: &str, offset: usize) -> analysis::Completions {
    let (line, column) = source_position(source, offset);
    let completions = analysis.autocomplete(module, line, column).unwrap();
    assert!(
        completions.entries.iter().all(|entry| !entry.name.starts_with("__l3i_comp_")),
        "generated binding at {line}:{column}: {completions:?}"
    );
    completions
}

#[test]
fn incomplete_comprehensions_complete_source_globals_in_keyword_and_projection_binding() {
    let cases = [
        ("source_hole", "local values: {number} = {1}\nreturn [for x in "),
        ("in_hole", "local values: {number} = {1}\nreturn [for x "),
        ("projection_hole", "local values: {number} = {1}\nreturn [for x in values => "),
    ];
    for solver in [analysis::Solver::New, analysis::Solver::Old] {
        let analysis = comprehension_analysis(HashMap::from(cases), solver);
        for (module, source) in cases {
            let parsed = analysis::parse(source, false);
            assert!(!parsed.errors.is_empty(), "{module}: {parsed:?}");
            let completions = complete_at(&analysis, module, source, source.len());
            let expected: &[(&str, CompletionKind)] = match module {
                "source_hole" => &[("values", CompletionKind::Binding), ("math", CompletionKind::Binding)],
                "in_hole" => &[("in", CompletionKind::Keyword)],
                _ => &[("x", CompletionKind::Binding), ("values", CompletionKind::Binding)],
            };
            for (name, kind) in expected {
                assert!(
                    completions.entries.iter().any(|entry| entry.name == *name && entry.kind == *kind),
                    "{solver:?}: {module}: expected {name}: {completions:?}"
                );
            }
            if module == "source_hole" {
                assert!(!completions.entries.iter().any(|entry| entry.name == "x"), "{completions:?}");
            }
            if module == "projection_hole" {
                let binding = completions.entries.iter().find(|entry| entry.name == "x").unwrap();
                assert_eq!(binding.type_text, "number", "{solver:?}: {binding:?}");
            }
        }
    }
}

#[test]
fn partial_member_completions_keep_types_in_projection_filter_and_dependent_source() {
    let prefix = "--!strict\nlocal rows: {{score: number, enabled: boolean, children: {number}}} = {{score = 1, enabled = true, children = {1}}}\n";
    let cases = [
        ("projection_dot", "return [for x in rows => x."),
        ("filter_dot", "return [for x in rows if x."),
        ("source_dot", "return [for x in rows for child in x."),
    ];
    for solver in [analysis::Solver::New, analysis::Solver::Old] {
        // SourceProvider owns dynamic strings here so the cursor is computed from exactly
        // the bytes the frontend sees, rather than a hand-maintained generated coordinate.
        let analysis = Analysis::new(
            MemberSources(
                cases.iter().map(|(name, suffix)| ((*name).to_owned(), format!("{prefix}{suffix}"))).collect(),
            ),
            AnalysisOptions { solver, retain_full_type_graphs: true, ..AnalysisOptions::default() },
        )
        .unwrap();
        for (module, suffix) in cases {
            let source = format!("{prefix}{suffix}");
            let completions = complete_at(&analysis, module, &source, source.len());
            assert_eq!(completions.context, CompletionContext::Property, "{solver:?}: {module}: {completions:?}");
            let mut properties: Vec<_> =
                completions.entries.iter().filter(|e| e.kind == CompletionKind::Property).collect();
            properties.sort_unstable_by(|a, b| a.name.cmp(&b.name));
            let names: Vec<_> = properties.iter().map(|e| e.name.as_str()).collect();
            assert_eq!(names, ["children", "enabled", "score"], "{solver:?}: {module}: {completions:?}");
            for (entry, expected) in properties.iter().zip(["{number}", "boolean", "number"]) {
                assert_eq!(entry.type_text, expected, "{solver:?}: {module}: {entry:?}");
            }
        }
    }
}

struct MemberSources(HashMap<String, String>);

impl SourceProvider for MemberSources {
    fn read_source(&self, name: &str) -> Option<SourceCode> {
        self.0.get(name).map(|text| SourceCode { text: text.clone(), is_script: false })
    }
    fn module_config(&self, _name: &str) -> ModuleConfig {
        ModuleConfig { mode: Mode::Strict, ..ModuleConfig::default() }
    }
}

#[test]
fn nested_partial_projection_retains_outer_capture_and_nullable_filter_refinement() {
    let source = concat!(
        "--!strict\nlocal rows: {{child: {name: string, score: number}?}} = {}\n",
        "return [for row in rows if row.child ~= nil => [for x in {row.child} => x.",
    );
    let capture = concat!(
        "--!strict\nlocal rows: {{score: number}} = {}\n",
        "return [for row in rows => [for x in {1} => function() return row.",
    );
    for solver in [analysis::Solver::New, analysis::Solver::Old] {
        let analysis =
            comprehension_analysis(HashMap::from([("refined_partial", source), ("capture_partial", capture)]), solver);
        for (module, text, expected) in
            [("refined_partial", source, &["name", "score"][..]), ("capture_partial", capture, &["score"][..])]
        {
            let completions = complete_at(&analysis, module, text, text.len());
            assert_eq!(completions.context, CompletionContext::Property, "{solver:?}: {completions:?}");
            let mut properties: Vec<_> = completions
                .entries
                .iter()
                .filter(|e| e.kind == CompletionKind::Property)
                .map(|e| e.name.as_str())
                .collect();
            properties.sort_unstable();
            assert_eq!(properties, expected, "{solver:?}: {module}: {completions:?}");
            assert_eq!(completions.entries.iter().find(|e| e.name == "score").unwrap().type_text, "number");
        }
    }
}

#[test]
fn copied_source_filter_and_projection_type_errors_are_not_suppressed_by_lowering() {
    let cases = [
        (
            "source_copy",
            "--!strict\nlocal obj: {items: {number}} = {items = {1}}\nreturn [for x in obj.absent => x]",
            "obj.absent",
        ),
        (
            "filter_copy",
            "--!strict\nlocal rows: {{score: number}} = {}\nreturn [for x in rows if x.absent => x.score]",
            "x.absent",
        ),
        (
            "projection_copy",
            "--!strict\nlocal rows: {{score: number}} = {}\nreturn [for x in rows => x.absent]",
            "x.absent",
        ),
    ];
    for solver in [analysis::Solver::New, analysis::Solver::Old] {
        let analysis = comprehension_analysis(cases.iter().map(|(name, source, _)| (*name, *source)).collect(), solver);
        for (module, source, marked) in cases {
            let report = analysis.check(module, false);
            assert!(!report.is_clean(), "{solver:?}: {module}: {report:?}");
            assert!(
                report.diagnostics.iter().all(|error| error.kind == DiagnosticKind::TypeError),
                "{solver:?}: {report:?}"
            );
            let error =
                report.diagnostics.iter().find(|error| error.text.contains("absent")).expect("copied property error");
            assert_diagnostic_span(error, source, marked);
            for error in &report.diagnostics {
                assert!(!error.text.contains("__l3i_comp_"), "{solver:?}: {module}: {error:?}");
                let begin = source.lines().nth(error.span.begin_line as usize).unwrap();
                let end = source.lines().nth(error.span.end_line as usize).unwrap();
                assert!(
                    error.span.begin_column as usize <= begin.len() && error.span.end_column as usize <= end.len(),
                    "{error:?}"
                );
            }
        }
    }
}

#[test]
fn partial_comprehension_completion_and_error_caches_update_for_both_solvers() {
    let old = "--!strict\nlocal rows: {{oldOnly: number}} = {}\nreturn [for x in rows => x.";
    let new = "--!strict\n\nlocal rows: {{newOnly: string}} = {}\nreturn [for x in rows if x.";
    for solver in [analysis::Solver::New, analysis::Solver::Old] {
        let source = std::rc::Rc::new(std::cell::RefCell::new(old));
        let analysis = Analysis::new(
            ChangingSource(source.clone()),
            AnalysisOptions { solver, retain_full_type_graphs: true, ..AnalysisOptions::default() },
        )
        .unwrap();
        let check = |text: &str, property: &str, expected_type: &str| {
            let report = analysis.check("snapshot", false);
            assert!(!report.is_clean(), "{solver:?}: {report:?}");
            assert!(report.diagnostics.iter().all(|d| !d.text.contains("__l3i_comp_")), "{report:?}");
            let parsed = analysis::parse(text, false);
            let actual: Vec<_> = report
                .diagnostics
                .iter()
                .filter(|d| d.kind == DiagnosticKind::ParseError)
                .map(|d| (&d.text, d.span))
                .collect();
            let expected: Vec<_> = parsed.errors.iter().map(|d| (&d.text, d.span)).collect();
            assert_eq!(actual, expected, "{solver:?}: cached parse diagnostics");
            let completions = complete_at(&analysis, "snapshot", text, text.len());
            assert_eq!(completions.context, CompletionContext::Property);
            let properties: Vec<_> =
                completions.entries.iter().filter(|e| e.kind == CompletionKind::Property).collect();
            assert_eq!(properties.len(), 1, "{solver:?}: {completions:?}");
            assert_eq!(properties[0].name, property);
            assert_eq!(properties[0].type_text, expected_type);
        };
        check(old, "oldOnly", "number");
        *source.borrow_mut() = new;
        analysis.mark_dirty("snapshot");
        check(new, "newOnly", "string");
        *source.borrow_mut() = old;
        analysis.clear();
        check(old, "oldOnly", "number");
    }
}

#[test]
fn min_max_any_all_reducers_infer_element_and_boolean_types_on_both_solvers() {
    let source = concat!(
        "--!strict\n",
        "local values: { number } = { 1, 2, 3 }\n",
        "local names: { string } = { 'a', 'b' }\n",
        "local least = min[for x in values if x > 1 => x * 2]\n",
        "local greatest = max[for name in names => name]\n",
        "local some = any[for x in values => x > 2]\n",
        "local every = all[for i, x in enumerate(values) => x >= i]\n",
        "return least, greatest, some, every\n",
    );
    for solver in [analysis::Solver::New, analysis::Solver::Old] {
        let analysis = comprehension_analysis(HashMap::from([("reducers", source)]), solver);
        let report = analysis.check("reducers", false);
        assert!(report.is_clean(), "{solver:?}: {report:#?}");
        assert_binding_type(&analysis, "reducers", 7, "least", "number");
        assert_binding_type(&analysis, "reducers", 7, "greatest", "string");
        assert_binding_type(&analysis, "reducers", 7, "some", "boolean");
        assert_binding_type(&analysis, "reducers", 7, "every", "boolean");
    }
}

#[test]
fn sinks_type_as_their_destination_and_check_projections_against_it() {
    let source = concat!(
        "--!strict\n",
        "local values: { number } = { 1, 2, 3 }\n",
        "local out: { number } = {}\n",
        "local filled = into(out)[for x in values if x > 1 => x * 2]\n",
        "local named = into({} :: { string })[for x in values => tostring(x)]\n",
        "return filled, named\n",
    );
    for solver in [analysis::Solver::New, analysis::Solver::Old] {
        let analysis = comprehension_analysis(HashMap::from([("sink", source)]), solver);
        let report = analysis.check("sink", false);
        assert!(report.is_clean(), "{solver:?}: {report:#?}");
        assert_binding_type(&analysis, "sink", 5, "filled", "{number}");
        assert_binding_type(&analysis, "sink", 5, "named", "{string}");
    }
    let wrong = concat!(
        "--!strict\n",
        "local values: { number } = { 1 }\n",
        "local out: { string } = {}\n",
        "local filled = into(out)[for x in values => x * 2]\n",
        "return filled\n",
    );
    let analysis = comprehension_analysis(HashMap::from([("wrong", wrong)]), analysis::Solver::New);
    let report = analysis.check("wrong", false);
    let error = report.diagnostics.iter().find(|d| d.kind == DiagnosticKind::TypeError).expect("a type error");
    assert!(error.text.contains("number") && error.text.contains("string"), "{}", error.text);
    assert_eq!(error.span.begin_line, 3, "{error:?}");
}
