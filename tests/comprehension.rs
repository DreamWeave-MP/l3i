//! L3i's first surface-syntax experiment: eager optimizer-visible list comprehensions.

use l3i::Runtime;

#[test]
fn dense_comprehension_executes() {
    let runtime = Runtime::new().unwrap();
    runtime
        .exec(
            r#"
            local values = { 1, 2, 3, 4 }
            local squares = [for x in values => x * x]
            assert(#squares == 4)
            assert(squares[1] == 1 and squares[2] == 4 and squares[3] == 9 and squares[4] == 16)
            "#,
        )
        .unwrap();
}

#[test]
fn filtered_comprehension_fuses_filter_and_projection() {
    let runtime = Runtime::new().unwrap();
    runtime
        .exec(
            r#"
            local values = {
                { id = 10, active = true },
                { id = 20, active = false },
                { id = 30, active = true },
            }
            local ids = [for x in values if x.active => x.id]
            assert(#ids == 2)
            assert(ids[1] == 10 and ids[2] == 30)
            "#,
        )
        .unwrap();
}

#[test]
fn source_expression_is_evaluated_once() {
    let runtime = Runtime::new().unwrap();
    runtime
        .exec(
            r#"
            local calls = 0
            local function source()
                calls += 1
                return { 2, 4, 6 }
            end
            local doubled = [for x in source() => x * 2]
            assert(calls == 1)
            assert(doubled[1] == 4 and doubled[3] == 12)
            "#,
        )
        .unwrap();
}

#[test]
fn strings_comments_and_normal_indexing_are_not_surface_syntax() {
    let runtime = Runtime::new().unwrap();
    runtime
        .exec(
            r#"
            local values = { 9 }
            assert(values[1] == 9)
            local text = '[for x in values => x]'
            assert(text == '[for x in values => x]')
            -- [for x in values => x]
            local long = [=[[for x in values => x]]=]
            assert(long == '[for x in values => x]')
            "#,
        )
        .unwrap();
}

#[test]
fn multiple_generators_and_filters_execute() {
    let runtime = Runtime::new().unwrap();
    runtime
        .exec(
            r#"
            local rows = { { 1, 2 }, { 3, 4 } }
            local pairs = [for row in rows for x in row if x % 2 == 0 => x * 10]
            assert(#pairs == 2)
            assert(pairs[1] == 20 and pairs[2] == 40)
            "#,
        )
        .unwrap();
}

#[test]
fn nested_comprehensions_do_not_conflict_with_long_strings() {
    let runtime = Runtime::new().unwrap();
    runtime
        .exec(
            r#"
            local rows = {
                { values = { 1, 2 } },
                { values = { 3, 4 } },
            }
            local doubled = [for row in rows => [for x in row.values => x * 2]]
            assert(doubled[1][1] == 2 and doubled[1][2] == 4)
            assert(doubled[2][1] == 6 and doubled[2][2] == 8)
            "#,
        )
        .unwrap();
}


#[test]
fn nil_projection_is_rejected_instead_of_creating_a_sparse_result() {
    let runtime = Runtime::new().unwrap();
    runtime
        .exec(
            r#"
            local ok, err = pcall(function()
                return [for x in { 1, 2, 3 } => if x == 2 then nil else x]
            end)
            assert(not ok)
            assert(string.find(tostring(err), "comprehension projection produced nil", 1, true) ~= nil)
            "#,
        )
        .unwrap();
}

#[test]
fn nil_is_removed_only_by_an_explicit_filter_and_filters_use_luau_truthiness() {
    let runtime = Runtime::new().unwrap();
    runtime
        .exec(
            r#"
            local rows = {
                { value = 10, keep = "yes" },
                { keep = false },
                { value = 30, keep = 0 },
            }
            local values = [for row in rows if row.keep if row.value ~= nil => row.value]
            assert(#values == 2)
            assert(values[1] == 10 and values[2] == 30)
            "#,
        )
        .unwrap();
}

#[test]
fn length_of_comprehension_is_fused_without_dropping_projection_effects() {
    let runtime = Runtime::new().unwrap();
    runtime
        .exec(
            r#"
            local calls = 0
            local function project(x)
                calls += 1
                return x * 10
            end
            local n = #[for x in { 1, 2, 3, 4 } if x % 2 == 0 => project(x)]
            assert(n == 2)
            assert(calls == 2)
            "#,
        )
        .unwrap();
}

#[test]
fn fused_length_still_rejects_nil_projection() {
    let runtime = Runtime::new().unwrap();
    runtime
        .exec(
            r#"
            local ok, err = pcall(function()
                return #[for x in { 1, 2 } => if x == 2 then nil else x]
            end)
            assert(not ok)
            assert(string.find(tostring(err), "comprehension projection produced nil", 1, true) ~= nil)
            "#,
        )
        .unwrap();
}
