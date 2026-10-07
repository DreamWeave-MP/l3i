//! Golden snapshots of the surface lowerer: generated text, provenance, tooling sites, and
//! structural errors for every surface feature under every consumer's policy. A lowering
//! refactor that changes nothing observable leaves these files untouched; an intentional
//! change regenerates them with `L3I_UPDATE_LOWERING=1 cargo test lowering::` and the diff
//! is reviewed with the commit.

use std::fmt::Write as _;
use std::path::PathBuf;

use l3i::source::{LoweringMode, lowering_snapshot};

/// Every surface shape the lowerer distinguishes, named for the diff reader.
const CORPUS: &[(&str, &str)] = &[
    // Single ordinary generator and its consumers.
    ("dense", "return [for x in xs => x * 2]"),
    ("filtered", "return [for x in xs if p(x) => f(x)]"),
    ("two_filters", "return [for x in xs if p(x) if q(x) => f(x)]"),
    ("count", "return #[for x in xs if p(x) => f(x)]"),
    ("count_dense", "return #[for x in xs => f(x)]"),
    ("sum", "return sum[for x in xs if p(x) => f(x)]"),
    ("sum_dense", "return sum[for x in xs => x]"),
    ("count_spaced", "return # -- note\n[for x in xs => x]"),
    ("sum_spaced", "return sum --[[ note ]] [for x in xs => x]"),
    ("member_sum_is_not_a_reducer", "return t.sum[for x in xs => x]"),
    ("min", "return min[for x in xs if p(x) => f(x)]"),
    ("max_dense", "return max[for x in xs => x.score]"),
    ("any", "return any[for x in xs if p(x) => q(x)]"),
    ("all_dense", "return all[for x in xs => x.ok]"),
    ("min_range", "return min[for i in range(1, n) => cost(i)]"),
    ("max_slice", "return max[for x in xs[a:b] => x]"),
    ("any_nested", "return any[for x in xs for y in ys => x == y]"),
    ("all_zip", "return all[for a, b in zipStrict(xs, ys) => a <= b]"),
    ("member_min_is_not_a_reducer", "return math.min[for x in xs => x]"),
    // Sinks.
    ("sink_dense", "return into(out)[for x in xs => x * 2]"),
    ("sink_filtered", "return into(out)[for x in xs if p(x) => f(x)]"),
    ("sink_range", "into(out)[for i in range(1, n) => i]"),
    ("sink_slice", "into(out)[for x in xs[a:b] => x]"),
    ("sink_zip", "into(out)[for a, b in zipShortest(xs, ys) => a + b]"),
    ("sink_enumerate", "into(out)[for i, v in enumerate(xs) => i * v]"),
    ("sink_nested", "into(out)[for x in xs for y in x.items if y.ok => y]"),
    ("sink_destination_expression", "into(scratch.buffers[kind] or make())[for x in xs => x]"),
    ("sink_destination_with_comprehension", "into([for i in range(1, n) => 0])[for x in xs => x]"),
    ("sink_inside_comprehension", "return [for row in rows => into(row.out)[for x in row => x]]"),
    ("sink_length", "return #into(out)[for x in xs => x]"),
    ("sink_spaced", "return into (out) -- note\n[for x in xs => x]"),
    ("member_into_is_not_a_sink", "return t.into(out)[for x in xs => x]"),
    ("call_before_comprehension_is_postfix", "return f(out)[for x in xs => x]"),
    ("sink_empty_destination", "return into()[for x in xs => x]"),
    ("sink_unfinished", "return into(out)[for x in"),
    // Numeric ranges.
    ("range", "return [for i in range(1, n) => i]"),
    ("range_step", "return [for i in range(a, b, s) => i * 2]"),
    ("range_filtered", "return [for i in range(1, n) if i % 2 == 0 => i]"),
    ("range_count", "return #[for i in range(1, n) => i]"),
    ("range_sum", "return sum[for i in range(1, n, 2) => i]"),
    // Indexed helpers.
    ("enumerate", "return [for i, v in enumerate(xs) => i + v]"),
    ("enumerate_filtered", "return [for i, v in enumerate(xs) if i > 1 => v]"),
    ("enumerate_sum", "return sum[for i, v in enumerate(xs) => i * v]"),
    ("zip_shortest", "return [for a, b in zipShortest(xs, ys) => a + b]"),
    ("zip_three", "return [for a, b, c in zipShortest(xs, ys, zs) => a + b + c]"),
    ("zip_strict", "return [for a, b in zipStrict(xs, ys) => a * b]"),
    ("zip_filtered_sum", "return sum[for a, b in zipStrict(xs, ys) if a > b => a - b]"),
    ("zip_count", "return #[for a, b in zipShortest(xs, ys) => a]"),
    // Slices.
    ("slice_generator", "return [for x in xs[first:last] => x]"),
    ("slice_generator_filtered", "return [for x in xs[first:last] if p(x) => f(x)]"),
    ("slice_generator_count", "return #[for x in xs[2:limit] => x]"),
    ("slice_generator_sum", "return sum[for x in xs[a:b] if x > 0 => x]"),
    ("slice_generator_call_source", "return [for x in source()[2:4] => x * 2]"),
    ("slice_generator_parenthesized_bounds", "return [for x in xs[(first()):(last())] => x]"),
    ("slice_standalone", "return xs[first:last]"),
    ("slice_standalone_in_expression", "local part = values[2:n]\nreturn #part + 1"),
    ("slice_standalone_call_source", "return make()[i:j]"),
    ("slice_standalone_twice", "return xs[1:2], ys[3:4]"),
    ("slice_inside_comprehension_source", "return [for x in f(xs[1:2]) => x]"),
    ("slice_of_comprehension", "return [for x in xs => x][1:2]"),
    ("comprehension_in_slice_bound", "return xs[1:#[for x in ys => x]]"),
    ("index_is_not_a_slice", "return xs[i], obj[key:method()]"),
    // Nested pipelines: every generator kind in an inner position.
    ("nested_two_generators", "return [for x in xs for y in ys => {x, y}]"),
    ("nested_filter_between", "return [for x in xs if p(x) for y in children(x) if q(y) => f(x, y)]"),
    ("nested_range_inner", "return [for x in xs for i in range(1, x) => i]"),
    ("nested_range_outer", "return [for i in range(1, n) for x in xs => i * x]"),
    ("nested_slice_inner", "return [for row in rows for x in row[1:limit] => x]"),
    ("nested_slice_outer", "return [for row in rows[1:n] for x in row => x]"),
    ("nested_zip_inner", "return [for x in xs for a, b in zipStrict(x.l, x.r) => a + b]"),
    ("nested_enumerate_inner", "return [for outer in rows for i, v in enumerate(outer) => v + i]"),
    ("nested_enumerate_outer", "return [for i, row in enumerate(rows) for x in row => i * x]"),
    ("nested_count", "return #[for x in xs for y in ys if x < y => 1]"),
    ("nested_sum", "return sum[for x in xs for y in ys => x * y]"),
    ("nested_three", "return [for x in xs for y in ys for z in zs => x + y + z]"),
    ("comprehension_in_projection", "return [for row in rows => [for x in row => x * 2]]"),
    ("comprehension_in_source", "return [for x in [for y in ys => y + 1] => x * 2]"),
    ("comprehension_in_filter", "return [for x in xs if #[for y in x.items => y] > 0 => x]"),
    // Trivia, layout, hygiene.
    (
        "comments_inside",
        "return [for x in xs -- source note\n    if p(x) -- filter note\n    => f(x) -- projection note\n]",
    ),
    ("leading_comments", "--!native\n-- leading\nreturn [for x in xs => x]"),
    (
        "multiline",
        "local out = [\n    for x in values[\n        first:\n        last\n    ]\n    if accept(x)\n    => project(x)\n]\nreturn out",
    ),
    ("hygiene_offset_collision", "local __l3i_comp_7 = 1\nreturn [for x in xs => x + __l3i_comp_7]"),
    ("hygiene_user_names", "local error, table, buffer, typeof = 1, 2, 3, 4\nreturn [for x in xs[1:2] => x]"),
    ("interpolated_string", "return `count {#[for x in xs => x]} items`"),
    ("string_literal_is_not_syntax", "return '[for x in xs => x]', [==[[for y in ys]]==]"),
    ("two_comprehensions", "return [for x in xs => x], sum[for y in ys => y]"),
    ("missing_binding_filter_only", "return [for in xs if p => 1]"),
    // Recovery-only shapes: strict compilation leaves these untouched and reports errors.
    ("unfinished_for", "return [for"),
    ("unfinished_binding", "return [for x"),
    ("unfinished_in", "return [for x in"),
    ("unfinished_source", "return [for x in xs"),
    ("unfinished_filter", "return [for x in xs if"),
    ("unfinished_arrow", "return [for x in xs =>"),
    ("unfinished_member", "return [for x in xs => x."),
    ("unfinished_range", "return [for i in range(1, => i]"),
    ("unfinished_slice_first", "return xs[:n]"),
    ("unfinished_slice_last", "return xs[a:]"),
    ("unfinished_slice_generator", "return [for x in xs[:n] => x]"),
    ("unfinished_sum", "return sum[for x in"),
    ("unmatched_call_in_source", "return [for x in f( => x]"),
    ("unmatched_call_in_projection", "return [for x in xs => g(x]"),
    ("postfix_index", "return xs[for x in ys => x]"),
    ("unfinished_nested", "return [for x in xs for y in"),
];

const MODES: &[(LoweringMode, &str)] = &[
    (LoweringMode::Compile, "compile"),
    (LoweringMode::Analysis, "analysis"),
    (LoweringMode::AnalysisBuffers, "analysis_buffers"),
    (LoweringMode::Tooling, "tooling"),
];

fn golden_path(mode: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/support/lowering").join(format!("{mode}.golden"))
}

fn render(mode: LoweringMode) -> String {
    let mut out = String::new();
    for (name, source) in CORPUS {
        let snapshot = lowering_snapshot(source, mode).unwrap_or_else(|error| panic!("{name}: {error}"));
        writeln!(out, "==== {name}").unwrap();
        writeln!(out, "{source}").unwrap();
        writeln!(out, "----").unwrap();
        out.push_str(&snapshot);
    }
    out
}

fn check(mode: LoweringMode, label: &str) {
    let actual = render(mode);
    let path = golden_path(label);
    if std::env::var_os("L3I_UPDATE_LOWERING").is_some() {
        std::fs::write(&path, &actual).unwrap();
        return;
    }
    let expected = std::fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("{}: {error}; run with L3I_UPDATE_LOWERING=1 to create it", path.display()));
    if expected != actual {
        let scratch = std::env::temp_dir().join(format!("l3i-lowering-{label}.actual"));
        std::fs::write(&scratch, &actual).unwrap();
        let mismatch =
            expected.lines().zip(actual.lines()).enumerate().find(|(_, (left, right))| left != right).map_or_else(
                || format!("line count {} vs {}", expected.lines().count(), actual.lines().count()),
                |(index, (left, right))| format!("line {}:\n expected: {left}\n actual:   {right}", index + 1),
            );
        panic!(
            "lowering snapshot {label} changed; actual written to {}\nfirst difference at {mismatch}",
            scratch.display()
        );
    }
}

#[test]
fn lowering_snapshots_are_unchanged() {
    for (mode, label) in MODES {
        check(*mode, label);
    }
}

#[test]
fn every_corpus_case_has_a_unique_name() {
    let mut names: Vec<&str> = CORPUS.iter().map(|(name, _)| *name).collect();
    names.sort_unstable();
    names.dedup();
    assert_eq!(names.len(), CORPUS.len());
}
