//! L3i's first surface-syntax experiment: eager optimizer-visible list comprehensions.

use l3i::Runtime;
use std::cell::Cell;
use std::rc::Rc;

use l3i::debug::{DebugAction, DebugInfo, DebugScope, HookSet, RuntimeHooks};
use l3i::stack::Stack;

#[test]
fn numeric_range_generators_lower_to_numeric_loops_with_exact_semantics() {
    let runtime = Runtime::new().unwrap();
    runtime
        .exec(
            r#"
        local range = function() error("captured range") end
        local events = {}
        local function bound(label, value) table.insert(events, label) return value end
        local forward = [for i in range(bound("first", 2), bound("last", 6), bound("step", 2)) => i]
        assert(#forward == 3 and forward[1] == 2 and forward[2] == 4 and forward[3] == 6)
        assert(table.concat(events, ",") == "first,last,step")
        local reverse = [for i in range(5, 1, -2) => i]
        assert(#reverse == 3 and reverse[1] == 5 and reverse[3] == 1)
        assert(#[for i in range(1, 5) if i % 2 == 1 => i * 2] == 3)
        assert(sum[for i in range(1, 5) if i % 2 == 1 => i * 2] == 18)
        local nested = [for outer in {2, 3} for inner in range(1, outer) => outer * 10 + inner]
        assert(#nested == 5 and nested[1] == 21 and nested[5] == 33)
        local ok, message = pcall(function() return [for i in range(1, 2, 0) => i] end)
        assert(not ok and string.find(tostring(message), "range step must not be zero", 1, true))
    "#,
        )
        .unwrap();
}

#[test]
fn enumerate_generators_lower_to_indexed_loops() {
    let runtime = Runtime::new().unwrap();
    runtime
        .exec(
            r"
        local calls = 0
        local function values()
            calls += 1
            return { 4, 5, 6 }
        end
        local pairs = [for i, value in enumerate(values()) => i * 10 + value]
        assert(calls == 1 and #pairs == 3)
        assert(pairs[1] == 14 and pairs[2] == 25 and pairs[3] == 36)
        local filtered = [for i, value in enumerate({2, 4, 6}) if i > 1 => value]
        assert(#filtered == 2 and filtered[1] == 4 and filtered[2] == 6)
        assert(#[for i, value in enumerate({2, 4, 6}) => i] == 3)
        assert(sum[for i, value in enumerate({2, 4, 6}) => i + value] == 18)
        local nested = [for outer in {{7, 8}} for i, value in enumerate(outer) => value + i]
        assert(#nested == 2 and nested[1] == 8 and nested[2] == 10)
    ",
        )
        .unwrap();
}

#[test]
fn zip_generators_lower_to_indexed_loops_with_explicit_length_rules() {
    let runtime = Runtime::new().unwrap();
    runtime
        .exec(
            r#"
        local calls = 0
        local function left() calls += 1 return {10, 20, 30} end
        local shortest = [for a, b in zipShortest(left(), {1, 2}) => a + b]
        assert(calls == 1 and #shortest == 2 and shortest[1] == 11 and shortest[2] == 22)
        local strict = [for a, b, c in zipStrict({1, 2}, {10, 20}, {100, 200}) => a + b + c]
        assert(#strict == 2 and strict[1] == 111 and strict[2] == 222)
        local ok, message = pcall(function()
            return [for a, b in zipStrict({1}, {2, 3}) => a + b]
        end)
        assert(not ok and string.find(tostring(message), "zipStrict inputs must have equal lengths", 1, true))
        assert(sum[for a, b in zipShortest({1, 2, 3}, {10, 20}) => a + b] == 33)
    "#,
        )
        .unwrap();
}

#[test]
fn inclusive_slices_fuse_into_comprehension_consumers() {
    let runtime = Runtime::new().unwrap();
    runtime
        .exec(
            r"
        local calls = 0
        local function source() calls += 1 return {10, 20, 30, 40, 50} end
        local values = [for x in source()[2:4] => x * 2]
        assert(calls == 1, 'source')
        assert(#values == 3, 'length')
        assert(values[1] == 40 and values[3] == 80, 'values')
        assert(sum[for x in {1, 2, 3, 4, 5}[2:4] if x > 2 => x] == 7, 'sum')
        assert(#[for x in {1, 2, 3, 4, 5}[99:100] => x] == 0, 'high')
        assert(#[for x in {1, 2, 3}[3:1] => x] == 0, 'reverse')
    ",
        )
        .unwrap();
}

#[test]
fn standalone_slices_materialize_dense_tables_with_single_evaluation() {
    let runtime = Runtime::new().unwrap();
    runtime
        .exec(
            r#"
        local calls = 0
        local function source() calls += 1 return {10, 20, 30, 40} end
        local captured = 0
        local table = {create = function() captured += 1 return {} end, move = function() captured += 1 end}
        local typeof = function() captured += 1 return "table" end
        local part = source()[2:3]
        assert(calls == 1 and captured == 0 and #part == 2 and part[1] == 20 and part[2] == 30)
        local high = ({1, 2, 3})[99:100]
        assert(#high == 0)
        local key = {method = function() return 1 end}
        assert(({9})[key:method()] == 9)
    "#,
        )
        .unwrap();
}

#[test]
fn standalone_buffer_slices_copy_inclusive_one_based_byte_ranges() {
    let runtime = Runtime::new().unwrap();
    runtime
        .exec(
            r#"
        local input = buffer.create(5)
        for i = 0, 4 do buffer.writeu8(input, i, 10 + i) end
        local builtinBuffer = buffer
        local calls, captured = 0, 0
        local function source() calls += 1 return input end
        local buffer = {
            create = function() captured += 1 end,
            copy = function() captured += 1 end,
            len = function() captured += 1 end,
        }
        local part = source()[2:4]
        assert(calls == 1 and captured == 0)
        assert(typeof(part) == "buffer" and builtinBuffer.len(part) == 3)
        assert(builtinBuffer.readu8(part, 0) == 11 and builtinBuffer.readu8(part, 2) == 13)
        local empty = input[99:100]
        assert(builtinBuffer.len(empty) == 0)
        assert(sum[for byte in input[1:2] => byte] == 21)
    "#,
        )
        .unwrap();
}

#[test]
fn slice_sources_and_bounds_have_identical_checked_semantics_for_every_consumer() {
    for expression in [
        "values[first:last]",
        "[for x in values[first:last] => x]",
        "sum[for x in values[first:last] => x]",
        "#[for x in values[first:last] => x]",
    ] {
        let runtime = Runtime::new().unwrap();
        runtime
            .exec(&format!(
                r#"
            local values = {{10, 20, 30}}
            local function failure(firstValue, lastValue, expected)
                first, last = firstValue, lastValue
                local ok, message = pcall(function() return {expression} end)
                assert(not ok and string.find(tostring(message), expected, 1, true), tostring(message))
            end
            failure(1.5, 2, "slice first bound must be a finite integer")
            failure(0 / 0, 2, "slice first bound must be a finite integer")
            failure(math.huge, 2, "slice first bound must be a finite integer")
            failure("1", 2, "slice first bound must be a finite integer")
            failure(1, 2.5, "slice last bound must be a finite integer")
            failure(1, -math.huge, "slice last bound must be a finite integer")
        "#,
            ))
            .unwrap();
    }

    for expression in ["source[1:1]", "sum[for x in source[1:1] => x]"] {
        let runtime = Runtime::new().unwrap();
        runtime
            .exec(&format!(
                r#"
            local source = "not a table"
            local ok, message = pcall(function() return {expression} end)
            assert(not ok and string.find(tostring(message), "slice source must be a table", 1, true))
        "#,
            ))
            .unwrap();
    }
}

#[test]
fn slices_evaluate_source_then_bounds_then_length_once() {
    for expression in ["source()[(first()):(last())]", "sum[for x in source()[(first()):(last())] => x]"] {
        let runtime = Runtime::new().unwrap();
        runtime
            .exec(&format!(
                r#"
            local events = {{}}
            local function mark(name) events[#events + 1] = name end
            local function source()
                mark("source")
                return setmetatable({{10, 20, 30}}, {{__len = function() mark("length") return 3 end}})
            end
            local function first() mark("first") return 1 end
            local function last() mark("last") return 2 end
            local result = {expression}
            assert(table.concat(events, ",") == "source,first,last,length", table.concat(events, ","))
        "#,
            ))
            .unwrap();
    }
}

#[test]
fn dependent_slice_generators_recompute_bounds_at_the_nested_loop_position() {
    let runtime = Runtime::new().unwrap();
    runtime
        .exec(
            r#"
        local events = {}
        local rows = {{values = {1, 2, 3}, first = 2}, {values = {10, 20}, first = 1}}
        local function source(row) events[#events + 1] = "source" .. row.first return row.values end
        local function first(row) events[#events + 1] = "first" .. row.first return row.first end
        local function last(row) events[#events + 1] = "last" .. row.first return #row.values end
        local total = sum[for row in rows for x in source(row)[first(row):last(row)] => x]
        assert(total == 35)
        assert(table.concat(events, ",") == "source2,first2,last2,source1,first1,last1")
    "#,
        )
        .unwrap();
}

#[test]
fn language_operations_cannot_be_captured_by_user_bindings() {
    for consume in ["", "#", "sum"] {
        let runtime = Runtime::new().unwrap();
        runtime
            .exec(&format!(
                r#"
            local calls = 0
            local error = function(_) calls += 1 end
            local table = {{create = function(_) calls += 1 return {{99}} end}}
            local ok, message = pcall(function()
                return {consume}[for x in {{1}} => nil]
            end)
            assert(not ok and string.find(tostring(message), "comprehension projection produced nil", 1, true))
            assert(calls == 0, "generated operations used user bindings")
        "#
            ))
            .unwrap();
    }
    let runtime = Runtime::new().unwrap();
    runtime
        .exec(
            r#"
        local calls = 0
        local table = {create = function(_) calls += 1 return {99} end}
        local materialized = [for x in {} => x]
        local fused = #[for x in {} => x]
        assert(#materialized == 0 and fused == 0 and calls == 0)
        local ok, message = pcall(function()
            return [for error in {function(_) calls += 1 end} => nil]
        end)
        assert(not ok and string.find(tostring(message), "comprehension projection produced nil", 1, true))
        assert(calls == 0)
        local copied = [for x in {1, 2} => table.create(x)[1]]
        assert(copied[1] == 99 and copied[2] == 99 and calls == 2, "copied calls must retain lexical lookup")
    "#,
        )
        .unwrap();
}

struct InstructionCounter(Rc<Cell<u64>>);

impl RuntimeHooks for InstructionCounter {
    fn debug_step(&self, _stack: &Stack<'_>, _info: &DebugInfo) -> DebugAction {
        self.0.set(self.0.get() + 1);
        DebugAction::Continue
    }
}

fn executed_instructions(body: &str, items: u32) -> u64 {
    let runtime = Runtime::new().unwrap();
    runtime.exec(&format!("values = table.create({items}) for i = 1, {items} do values[i] = i end")).unwrap();
    let function = runtime.load_function(&format!("return function() {body} end")).unwrap();
    let steps = Rc::new(Cell::new(0));
    runtime.set_hooks(InstructionCounter(steps.clone()), HookSet::DEBUGGER);
    let thread = runtime.new_thread().unwrap();
    let stack = runtime.stack();
    // Debug callbacks lease the running thread's stack; run on a coroutine, not the
    // already-leased host root stack, exactly as a host-driven debugger does.
    thread
        .with_stack(&stack, |scope| {
            scope.single_step(true);
            Ok(())
        })
        .unwrap();
    assert!(matches!(thread.start(&stack, &function, ()).unwrap(), l3i::thread::Resume::Finished(_)));
    drop(stack);
    runtime.clear_hooks();
    steps.get()
}

#[test]
fn executed_instruction_counts_have_no_per_element_wrapper_overhead() {
    let guard = "if value == nil then error('L3i comprehension projection produced nil; filter nil explicitly') end";
    for items in [0, 1, 32, 4096] {
        let dense = executed_instructions("return [for x in values => x * 2]", items);
        let dense_loop = executed_instructions(
            &format!(
                "local src = values local n = #src local out = table.create(n) \
             for i = 1, n do local x = src[i] local value = x * 2 {guard} out[i] = value end return out"
            ),
            items,
        );
        let unchecked = executed_instructions(
            "local src = values local n = #src local out = table.create(n) \
             for i = 1, n do out[i] = src[i] * 2 end return out",
            items,
        );
        let filtered = executed_instructions("return [for x in values if x % 2 == 0 => x * 2]", items);
        let filtered_loop = executed_instructions(
            &format!(
                "local src = values local n = #src local out = table.create(n) local j = 0 \
             for i = 1, n do local x = src[i] if x % 2 == 0 then \
             local value = x * 2 {guard} j += 1 out[j] = value end end return out"
            ),
            items,
        );
        let count = executed_instructions("return #[for x in values if x % 2 == 0 => x * 2]", items);
        let count_loop = executed_instructions(
            &format!(
                "local src = values local n = 0 for i = 1, #src do local x = src[i] \
             if x % 2 == 0 then local value = x * 2 {guard} n += 1 end end return n"
            ),
            items,
        );
        let sum = executed_instructions("return sum[for x in values if x % 2 == 0 => x * 2]", items);
        let sum_loop = executed_instructions(
            &format!(
                "local src = values local total = 0 for i = 1, #src do local x = src[i] \
             if x % 2 == 0 then local value = x * 2 {guard} total += value end end return total"
            ),
            items,
        );
        let materialized =
            executed_instructions("local out = [for x in values if x % 2 == 0 => x * 2] return #out", items);
        println!(
            "{items} items: dense={dense}, checked loop={dense_loop}, unchecked={unchecked}; \
                  filtered={filtered}, checked loop={filtered_loop}; count={count}, checked loop={count_loop}; \
                  sum={sum}, checked loop={sum_loop}; materialized={materialized}"
        );
        // IIFE result-register placement adds a setup MOVE, not an instruction per element.
        assert_eq!(dense, dense_loop + 1);
        assert_eq!(dense_loop, unchecked + u64::from(items), "nil guard costs one instruction per accepted projection");
        assert_eq!(filtered, filtered_loop + 1);
        assert_eq!(count, count_loop + 1);
        assert_eq!(sum, sum_loop + 1);
    }
}

#[test]
fn numeric_range_sum_matches_handwritten_vm_instructions_exactly() {
    let guard = "if value == nil then error('L3i comprehension projection produced nil; filter nil explicitly') end";
    for items in [0, 1, 32, 4096] {
        let surface = executed_instructions(&format!("return sum[for i in range(1, {items}) => i * 2]"), items);
        let handwritten = executed_instructions(
            &format!(
                "local total = 0 for i = 1, {items} do local value = i * 2 {guard} total += value end return total"
            ),
            items,
        );
        assert_eq!(surface, handwritten, "{items} items: range={surface}, loop={handwritten}");
    }
}

#[test]
fn dense_comprehension_executes() {
    let runtime = Runtime::new().unwrap();
    runtime
        .exec(
            r"
            local values = { 1, 2, 3, 4 }
            local squares = [for x in values => x * x]
            assert(#squares == 4)
            assert(squares[1] == 1 and squares[2] == 4 and squares[3] == 9 and squares[4] == 16)
            ",
        )
        .unwrap();
}

#[test]
fn filtered_comprehension_fuses_filter_and_projection() {
    let runtime = Runtime::new().unwrap();
    runtime
        .exec(
            r"
            local values = {
                { id = 10, active = true },
                { id = 20, active = false },
                { id = 30, active = true },
            }
            local ids = [for x in values if x.active => x.id]
            assert(#ids == 2)
            assert(ids[1] == 10 and ids[2] == 30)
            ",
        )
        .unwrap();
}

#[test]
fn source_expression_is_evaluated_once() {
    let runtime = Runtime::new().unwrap();
    runtime
        .exec(
            r"
            local calls = 0
            local function source()
                calls += 1
                return { 2, 4, 6 }
            end
            local doubled = [for x in source() => x * 2]
            assert(calls == 1)
            assert(doubled[1] == 4 and doubled[3] == 12)
            ",
        )
        .unwrap();
}

#[test]
fn strings_comments_and_normal_indexing_are_not_surface_syntax() {
    let runtime = Runtime::new().unwrap();
    runtime
        .exec(
            r"
            local values = { 9 }
            assert(values[1] == 9)
            local text = '[for x in values => x]'
            assert(text == '[for x in values => x]')
            -- [for x in values => x]
            local long = [=[[for x in values => x]]=]
            assert(long == '[for x in values => x]')
            ",
        )
        .unwrap();
}

#[test]
fn multiple_generators_and_filters_execute() {
    let runtime = Runtime::new().unwrap();
    runtime
        .exec(
            r"
            local rows = { { 1, 2 }, { 3, 4 } }
            local pairs = [for row in rows for x in row if x % 2 == 0 => x * 10]
            assert(#pairs == 2)
            assert(pairs[1] == 20 and pairs[2] == 40)
            ",
        )
        .unwrap();
}

#[test]
fn nested_comprehensions_do_not_conflict_with_long_strings() {
    let runtime = Runtime::new().unwrap();
    runtime
        .exec(
            r"
            local rows = {
                { values = { 1, 2 } },
                { values = { 3, 4 } },
            }
            local doubled = [for row in rows => [for x in row.values => x * 2]]
            assert(doubled[1][1] == 2 and doubled[1][2] == 4)
            assert(doubled[2][1] == 6 and doubled[2][2] == 8)
            ",
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
            r"
            local calls = 0
            local function project(x)
                calls += 1
                return x * 10
            end
            local n = #[for x in { 1, 2, 3, 4 } if x % 2 == 0 => project(x)]
            assert(n == 2)
            assert(calls == 2)
            ",
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

#[test]
fn sum_reducer_fuses_without_materializing_and_preserves_effects() {
    let runtime = Runtime::new().unwrap();
    runtime
        .exec(
            r"
            local calls = 0
            local function project(x)
                calls += 1
                return x * 10
            end
            local total = sum[for x in { 1, 2, 3, 4 } if x % 2 == 0 => project(x)]
            assert(total == 60)
            assert(calls == 2)
            assert(sum[for x in {} => x] == 0)
            local nested = sum[for x in { 1, 2 } for y in { x, x + 1 } => y]
            assert(nested == 8)
            ",
        )
        .unwrap();
}

#[test]
fn sum_reducer_retains_non_nil_projection_contract() {
    let runtime = Runtime::new().unwrap();
    runtime
        .exec(
            r#"
            local ok, err = pcall(function()
                return sum[for x in { 1, 2 } => if x == 2 then nil else x]
            end)
            assert(not ok)
            assert(string.find(tostring(err), "comprehension projection produced nil", 1, true) ~= nil)
            "#,
        )
        .unwrap();
}

#[test]
fn dependent_sources_filters_and_projection_preserve_exact_event_order() {
    for consumer in ["local result = ", "local result = #"] {
        let runtime = Runtime::new().unwrap();
        runtime.exec(&format!(r#"
            local events = {{}}
            local function event(s) events[#events + 1] = s end
            local function source() event("source") return {{1, 2, 3}} end
            local function outer(x) event("outer" .. x) return x ~= 2 end
            local function children(x) event("children" .. x) return {{x * 10, x * 10 + 1}} end
            local function first(y) event("first" .. y) return y % 2 == 0 end
            local function second(y) event("second" .. y) return "truthy" end
            local function project(x, y) event("project" .. x .. ":" .. y) return y end
            {consumer}[for x in source() if outer(x) for y in children(x) if first(y) if second(y) => project(x, y)]
            assert(table.concat(events, ",") ==
                "source,outer1,children1,first10,second10,project1:10,first11,outer2,outer3,children3,first30,second30,project3:30,first31")
            if type(result) == "table" then
                assert(#result == 2 and result[1] == 10 and result[2] == 30)
            else assert(result == 2) end
        "#)).unwrap();
    }
}

#[test]
fn nil_and_projection_errors_stop_at_the_same_element_with_or_without_fusion() {
    for consumer in ["", "#"] {
        for failure in ["return nil", "error('projection exploded')"] {
            let runtime = Runtime::new().unwrap();
            runtime
                .exec(&format!(
                    r#"
                local calls = 0
                local function project(x)
                    calls += 1
                    if x == 2 then {failure} end
                    return x
                end
                local ok, err = pcall(function()
                    return {consumer}[for x in {{1, 2, 3}} => project(x)]
                end)
                assert(not ok and calls == 2)
                local expected = if {is_nil} then
                    "L3i comprehension projection produced nil; filter nil explicitly"
                    else "projection exploded"
                assert(string.find(tostring(err), expected, 1, true))
            "#,
                    is_nil = failure == "return nil"
                ))
                .unwrap();
        }
    }
}

#[test]
fn empty_inputs_false_projections_and_all_luau_filter_truth_values() {
    let runtime = Runtime::new().unwrap();
    runtime
        .exec(
            r#"
        local calls = 0
        local function project(x) calls += 1 return false end
        local empty = [for x in {} => project(x)]
        assert(#empty == 0 and calls == 0)
        assert(#[for x in {} => project(x)] == 0 and calls == 0)
        local rows = {{keep = false}, {}, {keep = 0}, {keep = ""}, {keep = {}}, {keep = true}}
        local result = [for row in rows if row.keep => project(row)]
        assert(#result == 4 and calls == 4)
        for i = 1, 4 do assert(result[i] == false) end
        local xs = table.create(3, 1)
        xs[2] = nil -- Explicit boundary remains 3; this is not a promise about arbitrary sparse #.
        local filtered = [for x in xs if x ~= nil => x]
        assert(#filtered == 2 and filtered[1] == 1 and filtered[2] == 1)
    "#,
        )
        .unwrap();
}

#[test]
fn generated_locals_do_not_capture_adversarial_user_names() {
    let runtime = Runtime::new().unwrap();
    runtime
        .exec(
            r"
        local __l3i_src, __l3i_out, __l3i_value, __l3i_count = {1, 2}, 20, 30, 40
        local __l3i_comp_7_g0_src, __l3i_comp_7_out = 50, 60
        local result = [for __l3i_value in __l3i_src => __l3i_value + __l3i_out + __l3i_count]
        assert(result[1] == 61 and result[2] == 62 and __l3i_value == 30)
        assert(__l3i_comp_7_g0_src == 50 and __l3i_comp_7_out == 60)
        local nested = [for x in [for y in {1, 2} => y] if #[for z in {x} => z] => [for w in {x} => w]]
        assert(#nested == 2 and nested[1][1] == 1 and nested[2][1] == 2)
    ",
        )
        .unwrap();
    // Force a collision at the actual source offset (and with its first salted alternative).
    let prefix = "local result = ";
    let stem = format!("__l3i_comp_{}", prefix.len());
    runtime
        .exec(&format!(
            "{prefix}[for x in {{1, 2}} => x] \
         local {stem}_out, {stem}_1_out = 91, 92 \
         assert(result[1] == 1 and result[2] == 2 and {stem}_out == 91 and {stem}_1_out == 92)"
        ))
        .unwrap();
}

#[test]
fn trailing_clause_comments_cannot_swallow_generated_code() {
    let runtime = Runtime::new().unwrap();
    runtime
        .exec(
            r"
        local result = [for x in {1, 2} -- source comment
            if x > 0 -- filter comment
            => x * 2 -- projection comment
        ]
        assert(#result == 2 and result[1] == 2 and result[2] == 4)
        assert(#[for x in {1, 2} -- count source
            if x > 0 -- count filter
            => x -- count projection
        ] == 2)
        local nested = [for x in {1, 2} -- outer source
            for y in {x} -- inner source
            if y -- inner filter
            => y -- nested projection
        ]
        assert(#nested == 2 and nested[1] == 1 and nested[2] == 2)
    ",
        )
        .unwrap();
}

#[test]
fn length_fusion_never_consumes_a_hash_inside_a_preceding_comment() {
    let runtime = Runtime::new().unwrap();
    runtime
        .exec(
            r"
        local result = -- #
            [for x in {1, 2} => x * 2]
        assert(#result == 2 and result[1] == 2 and result[2] == 4)
        local count = # -- an intervening comment prevents lexical fusion, not execution
            [for x in {1, 2} => x]
        assert(count == 2)
    ",
        )
        .unwrap();
}

#[test]
fn min_and_max_reducers_follow_luau_ordering_and_reject_empty_pipelines() {
    let runtime = Runtime::new().unwrap();
    runtime
        .exec(
            r"
        local values = {5, 3, 9, 3, 7}
        assert(min[for x in values => x] == 3 and max[for x in values => x] == 9)
        assert(min[for x in values if x > 4 => x * 10] == 50)
        assert(max[for x in values if x > 4 => x * 10] == 90)
        assert(min[for i in range(1, 5) => -i] == -5 and max[for i in range(1, 5) => -i] == -1)
        assert(min[for x in values[2:4] => x] == 3 and max[for x in values[2:4] => x] == 9)
        assert(min[for w in {'pear', 'apple', 'fig'} => w] == 'apple')
        assert(max[for w in {'pear', 'apple', 'fig'} => w] == 'pear')
        assert(min[for x in {4} => x] == 4 and max[for x in {4} => x] == 4)
        assert(min[for x in {false} => x] == false and max[for x in {true} => x] == true)
        assert(not pcall(function() return min[for x in values => false] end), 'booleans do not order')
        -- A NaN that arrives first is sticky; later NaNs never replace, exactly like a `<` loop.
        local nan = 0 / 0
        assert(min[for x in {nan, 1, 2} => x] ~= min[for x in {nan, 1, 2} => x])
        assert(min[for x in {1, nan, 0} => x] == 0 and max[for x in {1, nan, 5} => x] == 5)
        -- Empty pipelines are errors, not nil: the language has no silent nil result.
        local ok, message = pcall(function() return min[for x in {} => x] end)
        assert(not ok and string.find(message, 'JSL min reducer received no elements', 1, true), message)
        ok, message = pcall(function() return max[for x in values if x > 100 => x] end)
        assert(not ok and string.find(message, 'JSL max reducer received no elements', 1, true), message)
        -- The projection contract is unchanged: nil is rejected, not skipped.
        ok, message = pcall(function() return min[for x in {{}} => x.missing] end)
        assert(not ok and string.find(message, 'produced nil', 1, true), message)
        -- Mixed types fail where a handwritten comparison would.
        ok = pcall(function() return max[for x in {1, 'two'} => x] end)
        assert(not ok)
    ",
        )
        .unwrap();
}

#[test]
fn any_and_all_reducers_short_circuit_at_the_first_deciding_projection() {
    let runtime = Runtime::new().unwrap();
    runtime
        .exec(
            r"
        local events = {}
        local function seen(x) table.insert(events, x) return x end
        assert(any[for x in {1, 2, 3, 4} => seen(x) > 1] == true)
        assert(#events == 2, 'any stops at the first truthy projection')
        events = {}
        assert(all[for x in {2, 4, 5, 6} => seen(x) % 2 == 0] == false)
        assert(#events == 3, 'all stops at the first falsy projection')
        events = {}
        assert(any[for x in {1, 2} => seen(x) > 5] == false and #events == 2)
        events = {}
        assert(all[for x in {2, 4} => seen(x) % 2 == 0] == true and #events == 2)
        -- Empty pipelines: any is false, all is true.
        assert(any[for x in {} => x] == false and all[for x in {} => x] == true)
        -- Luau truthiness: only nil and false are falsy; nil projections are still rejected.
        assert(all[for x in {0, '', {}} => x] == true)
        assert(any[for x in {false, false} => x] == false)
        local ok, message = pcall(function() return any[for x in {{}} => x.missing] end)
        assert(not ok and string.find(message, 'produced nil', 1, true), message)
        -- Short-circuit exits every nesting level and leaves no partial state behind.
        events = {}
        assert(any[for x in {1, 2} for y in {10, 20} => seen(x * y) == 20] == true and #events == 2)
        assert(all[for a, b in zipStrict({1, 2}, {1, 3}) => a == b] == false)
        assert(any[for i in range(1, 3) if i > 1 => i == 3] == true)
        assert(all[for x in {1, 2, 3}[2:3] => x > 1] == true)
        -- Filters run before the projection decides; a rejected element never decides.
        events = {}
        assert(all[for x in {1, 2, 3} if seen(x) > 1 => x > 1] == true and #events == 3)
    ",
        )
        .unwrap();
}

#[test]
fn reducers_are_syntax_not_bindings() {
    let runtime = Runtime::new().unwrap();
    runtime
        .exec(
            r"
        local min, max, any, all = 'shadowed', 'shadowed', 'shadowed', 'shadowed'
        assert(min[for x in {3, 1, 2} => x] == 1 and max[for x in {3, 1, 2} => x] == 3)
        assert(any[for x in {false, 1} => x] == true and all[for x in {false, 1} => x] == false)
        local t = {min = function() return 'member' end}
        assert(t.min(1) == 'member')
        assert(min == 'shadowed')
    ",
        )
        .unwrap();
}
