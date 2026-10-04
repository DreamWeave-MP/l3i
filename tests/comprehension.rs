//! L3i's first surface-syntax experiment: eager optimizer-visible list comprehensions.

use l3i::Runtime;
use std::cell::Cell;
use std::rc::Rc;

use l3i::debug::{DebugAction, DebugInfo, DebugScope, HookSet, RuntimeHooks};
use l3i::stack::Stack;

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
        let materialized =
            executed_instructions("local out = [for x in values if x % 2 == 0 => x * 2] return #out", items);
        println!(
            "{items} items: dense={dense}, checked loop={dense_loop}, unchecked={unchecked}; \
                  filtered={filtered}, checked loop={filtered_loop}; count={count}, checked loop={count_loop}, materialized={materialized}"
        );
        // IIFE result-register placement adds a setup MOVE, not an instruction per element.
        assert_eq!(dense, dense_loop + 1);
        assert_eq!(dense_loop, unchecked + u64::from(items), "nil guard costs one instruction per accepted projection");
        assert_eq!(filtered, filtered_loop + 1);
        assert_eq!(count, count_loop + 1);
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
