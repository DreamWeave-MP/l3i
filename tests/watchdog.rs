//! Runtime options: watchdog limits, memory categories, and call scopes (luastate.cpp).

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

use l3i::Runtime;
use l3i::bind::Call;
use l3i::runtime::{CallContext, CallKind, Limits, MemoryCategory};

fn context(id: u64, category: u8) -> CallContext {
    CallContext { id, category: MemoryCategory(category) }
}

#[test]
fn execution_time_limit_interrupts_runaway_scripts_and_leaves_the_vm_usable() {
    let runtime = Runtime::builder().execution_time_limit(Duration::from_millis(20)).build().unwrap();
    let spin = runtime.load_function("return function() while true do end end").unwrap();
    let error = {
        let _scope = runtime.call_scope(context(1, 0), CallKind::ScriptCall);
        spin.invoke::<(), _>(&runtime.stack(), ()).unwrap_err().to_string()
    };
    assert!(error.contains("Lua execution time limit exceeded after"), "{error}");
    assert_eq!(runtime.stack().top(), 0);
    let add = runtime.load_function("return function(a, b) return a + b end").unwrap();
    let _scope = runtime.call_scope(context(1, 0), CallKind::ScriptCall);
    assert_eq!(add.invoke::<i32, _>(&runtime.stack(), (2, 3)).unwrap(), 5);
}

#[test]
fn the_watchdog_only_watches_inside_a_scope_and_never_during_initialization() {
    let runtime = Runtime::builder().execution_time_limit(Duration::from_millis(10)).build().unwrap();
    let busy = runtime
        .load_function("return function() local t = os.clock() while os.clock() - t < 0.03 do end return 1 end")
        .unwrap();
    // No scope: the interrupt sees no active call and lets the script run.
    assert_eq!(busy.invoke::<i32, _>(&runtime.stack(), ()).unwrap(), 1);
    // Initialization scopes are never time-limited.
    let _init = runtime.call_scope(context(9, 0), CallKind::Initialization);
    assert_eq!(busy.invoke::<i32, _>(&runtime.stack(), ()).unwrap(), 1);
}

#[test]
fn memory_limit_stops_allocation_heavy_scripts() {
    let runtime = Runtime::builder().memory_limit(2 * 1024 * 1024).build().unwrap();
    let hog = runtime
        .load_function(
            "return function() local t = {} for i = 1, 10000000 do t[i] = {i, tostring(i)} end return #t end",
        )
        .unwrap();
    let _scope = runtime.call_scope(context(2, 0), CallKind::ScriptCall);
    let error = hog.invoke::<i32, _>(&runtime.stack(), ()).unwrap_err().to_string();
    assert!(error.contains("Lua memory limit exceeded"), "{error}");
    runtime.collect_garbage();
}

#[test]
fn limits_can_change_at_runtime() {
    let runtime = Runtime::new().unwrap();
    assert_eq!(runtime.limits(), Limits::default());
    let spin = runtime.load_function("return function() while true do end end").unwrap();
    runtime.set_limits(Limits { execution_time: Duration::from_millis(15), memory_bytes: 0 });
    let _scope = runtime.call_scope(context(3, 0), CallKind::HostInterface);
    assert!(spin.invoke::<(), _>(&runtime.stack(), ()).is_err());
}

#[test]
fn profiler_switches_memory_categories_and_records_call_time_and_allocations() {
    let runtime = Runtime::builder().profiler(true).build().unwrap();
    let allocate = runtime
        .load_function("return function() local t = {} for i = 1, 20000 do t[i] = {i} end return #t end")
        .unwrap();
    let before_in_five = runtime.total_bytes_in(MemoryCategory(5));
    {
        let _scope = runtime.call_scope(context(7, 5), CallKind::ScriptCall);
        assert_eq!(allocate.invoke::<i32, _>(&runtime.stack(), ()).unwrap(), 20000);
    }
    assert!(runtime.total_bytes_in(MemoryCategory(5)) > before_in_five, "allocations landed in category 5");
    let stats = runtime.call_stats();
    assert_eq!(stats.timed_calls, 1);
    assert!(stats.total_script_ms >= 0.0);
    assert!(stats.time_by_context_ms.contains_key(&7));
    assert!(stats.allocation_by_context.get(&7).copied().unwrap_or(0) > 0, "allocation activity was charged");

    // Nested scopes: the inner call's time is nested time for the outer, not counted twice.
    {
        let _outer = runtime.call_scope(context(1, 1), CallKind::ScriptCall);
        let _inner = runtime.call_scope(context(2, 2), CallKind::ScriptCall);
        assert_eq!(allocate.invoke::<i32, _>(&runtime.stack(), ()).unwrap(), 20000);
    }
    let stats = runtime.call_stats();
    assert_eq!(stats.timed_calls, 3);
    let inner = stats.time_by_context_ms[&2];
    let outer = stats.time_by_context_ms[&1];
    assert!(inner >= 0.0 && outer >= 0.0);
    assert!(outer <= stats.total_script_ms);
    assert!(runtime.total_bytes_in(MemoryCategory(2)) > 0);
}

#[test]
fn caller_location_names_the_running_script_line() {
    let runtime = Runtime::new().unwrap();
    assert_eq!(runtime.caller_location(), "");
    let seen = Rc::new(RefCell::new(String::new()));
    let probe = runtime
        .bind_function("dreamweave.where", {
            let seen = seen.clone();
            move |call: &Call| *seen.borrow_mut() = l3i::runtime::caller_location(call)
        })
        .unwrap();
    runtime.set_global("where", &probe).unwrap();
    runtime.exec("local x = 1\n\nwhere()").unwrap();
    assert_eq!(*seen.borrow(), "exec:3");
}

#[test]
fn the_sampler_attributes_safepoints_to_the_sampled_context_only() {
    let runtime = Runtime::builder().profiler(true).build().unwrap();
    let spin = runtime
        .load_function(
            "local function inner(n)\n  local s = 0\n  for i = 1, n do s = s + i end\n  return s\nend\n\
             return function()\n  local total = 0\n  for i = 1, 4000 do total = total + inner(50) end\n  return total\nend",
        )
        .unwrap();
    assert_eq!(runtime.sampled_context(), None);
    runtime.set_sampled_context(Some(7));
    {
        let _other = runtime.call_scope(context(8, 0), CallKind::ScriptCall);
        spin.invoke::<f64, _>(&runtime.stack(), ()).unwrap();
    }
    assert_eq!(runtime.samples().count, 0, "another context is never sampled");
    {
        let _sampled = runtime.call_scope(context(7, 0), CallKind::ScriptCall);
        spin.invoke::<f64, _>(&runtime.stack(), ()).unwrap();
    }
    let samples = runtime.samples();
    assert!(samples.count > 0);
    assert!(samples.lines.keys().all(|key| key.starts_with("load_function:")), "{:?}", samples.lines.keys());
    assert!(samples.functions.contains_key("load_function:1"), "{:?}", samples.functions.keys());
    assert_eq!(samples.functions["load_function:1"].function, "inner");
    assert!(samples.functions.contains_key("load_function:6"));
    assert!(samples.lines.values().map(|l| l.samples).sum::<u64>() == samples.count);
    runtime.set_sampled_context(None);
    assert_eq!(runtime.samples(), l3i::runtime::Samples::default());
}

#[test]
fn gc_steps_and_stats_frames_are_host_driven() {
    let runtime = Runtime::new().unwrap();
    runtime.exec("local t = {} for i = 1, 10000 do t[i] = {} end").unwrap();
    let mut finished = false;
    for _ in 0..10_000 {
        let (done, _elapsed) = runtime.gc_step_timed(1);
        if done {
            finished = true;
            break;
        }
    }
    assert!(finished, "incremental steps eventually finish a cycle");
    assert_eq!(runtime.stats_frame(), 0);
    runtime.advance_stats_frame();
    runtime.advance_stats_frame();
    assert_eq!(runtime.stats_frame(), 2);
}

#[test]
fn values_that_outlive_their_runtime_become_invalid_instead_of_dangling() {
    let (value, clone) = {
        let runtime = Runtime::new().unwrap();
        let function = runtime.load_function("return function() end").unwrap().into_value();
        assert!(function.is_valid());
        let clone = function.clone();
        (function, clone)
    };
    assert!(!value.is_valid());
    assert!(!clone.is_valid());
    assert!(!value.is_function());
    drop(value);
    drop(clone);
}

#[test]
#[should_panic(expected = "a root stack for this Lua thread is already alive")]
fn a_second_root_stack_is_refused_while_one_is_alive() {
    let runtime = Runtime::new().unwrap();
    let first = runtime.stack();
    let _second = runtime.stack();
    drop(first);
}

#[test]
fn root_stacks_can_be_taken_again_once_released() {
    let runtime = Runtime::new().unwrap();
    {
        let stack = runtime.stack();
        stack
            .with_frame(|frame| {
                frame.push_number(1.0);
                Ok(())
            })
            .unwrap();
    }
    let stack = runtime.stack();
    assert_eq!(stack.top(), 0);
    drop(stack);
    runtime.exec("local x = 1").unwrap();
}
