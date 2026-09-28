//! Coroutines driven from the host, and yields from bound functions.

use l3i::Runtime;
use l3i::bind::{Break, Yield};
use l3i::source::CompileOptions;
use l3i::thread::{CoroutineStatus, Resume, ThreadStatus};
use l3i::value::Value;

fn number(runtime: &Runtime, value: &Value) -> f64 {
    value.with_value(&runtime.stack(), |_, view| view.read::<f64>()).unwrap()
}

#[test]
fn a_thread_yields_receives_and_finishes() {
    let runtime = Runtime::new().unwrap();
    let generator = runtime
        .load_function(
            "return function(a) local b = coroutine.yield(a + 1) local c = coroutine.yield(b * 2) return a + b + c end",
        )
        .unwrap();
    let thread = runtime.new_thread().unwrap();
    assert_eq!(thread.status(), ThreadStatus::Ok);
    assert!(thread.is_reset() || thread.status() == ThreadStatus::Ok);
    let stack = runtime.stack();
    let Resume::Yielded(values) = thread.start(&stack, &generator, (1,)).unwrap() else { panic!("expected a yield") };
    drop(stack);
    assert_eq!(number(&runtime, &values[0]), 2.0);
    assert_eq!(thread.status(), ThreadStatus::Yielded);
    assert_eq!(thread.coroutine_status(&runtime.stack()).unwrap(), CoroutineStatus::Suspended);
    let stack = runtime.stack();
    let Resume::Yielded(values) = thread.resume(&stack, (10,)).unwrap() else { panic!("expected a yield") };
    drop(stack);
    assert_eq!(number(&runtime, &values[0]), 20.0);
    let stack = runtime.stack();
    let Resume::Finished(values) = thread.resume(&stack, (100,)).unwrap() else { panic!("expected a return") };
    drop(stack);
    assert_eq!(number(&runtime, &values[0]), 111.0);
    assert_eq!(thread.coroutine_status(&runtime.stack()).unwrap(), CoroutineStatus::Finished);
    // A normally finished thread is idle again; a suspended one is not, until reset.
    assert!(matches!(thread.start(&runtime.stack(), &generator, (5,)).unwrap(), Resume::Yielded(_)));
    assert!(thread.start(&runtime.stack(), &generator, (1,)).is_err(), "a suspended thread cannot be restarted");
    thread.reset().unwrap();
    assert!(thread.is_reset());
    assert!(matches!(thread.start(&runtime.stack(), &generator, (5,)).unwrap(), Resume::Yielded(_)));
}

#[test]
fn errors_inside_a_coroutine_are_reported_and_leave_the_thread_dead() {
    let runtime = Runtime::new().unwrap();
    let failing = runtime.load_function("return function() coroutine.yield() error('boom') end").unwrap();
    let thread = runtime.new_thread().unwrap();
    assert!(matches!(thread.start(&runtime.stack(), &failing, ()).unwrap(), Resume::Yielded(_)));
    let error = thread.resume(&runtime.stack(), ()).unwrap_err().to_string();
    assert!(error.ends_with("boom"), "{error}");
    assert!(matches!(thread.status(), ThreadStatus::Error(_)));
    assert_eq!(thread.coroutine_status(&runtime.stack()).unwrap(), CoroutineStatus::FinishedWithError);
    // Raising into a yield from the host.
    let catching =
        runtime.load_function("return function() local ok, e = pcall(coroutine.yield) return ok, e end").unwrap();
    let thread = runtime.new_thread().unwrap();
    assert!(matches!(thread.start(&runtime.stack(), &catching, ()).unwrap(), Resume::Yielded(_)));
    let Resume::Finished(values) = thread.resume_with_error(&runtime.stack(), "injected").unwrap() else { panic!() };
    assert!(!values[0].with_value(&runtime.stack(), |_, v| v.read::<bool>()).unwrap());
    assert_eq!(values[1].with_value(&runtime.stack(), |_, v| v.read::<String>()).unwrap(), "injected");
}

#[test]
fn bound_functions_can_yield_and_break() {
    let runtime = Runtime::new().unwrap();
    let pause = runtime.bind_function("dreamweave.pause", |value: i32| Yield((value * 2, "paused"))).unwrap();
    runtime.set_global("pause", &pause).unwrap();
    let stop = runtime.bind_function("dreamweave.stop", || Break).unwrap();
    runtime.set_global("stop", &stop).unwrap();
    let body = runtime.load_function("return function() local got = pause(21) stop() return got .. '!' end").unwrap();
    let thread = runtime.new_thread().unwrap();
    let Resume::Yielded(values) = thread.start(&runtime.stack(), &body, ()).unwrap() else {
        panic!("expected the yield")
    };
    assert_eq!(number(&runtime, &values[0]), 42.0);
    assert_eq!(values[1].with_value(&runtime.stack(), |_, v| v.read::<String>()).unwrap(), "paused");
    assert!(matches!(thread.resume(&runtime.stack(), ("resumed",)).unwrap(), Resume::Break));
    assert_eq!(thread.status(), ThreadStatus::Break);
    let Resume::Finished(values) = thread.resume(&runtime.stack(), ()).unwrap() else { panic!("expected the return") };
    assert_eq!(values[0].with_value(&runtime.stack(), |_, v| v.read::<String>()).unwrap(), "resumed!");
    // Yielding outside a coroutine is Luau's error, not a crash.
    let error = runtime.exec("pause(1)").unwrap_err().to_string();
    assert!(error.contains("attempt to yield"), "{error}");
}

#[test]
fn thread_data_and_sandboxed_globals() {
    let runtime = Runtime::new().unwrap();
    let thread = runtime.new_thread().unwrap();
    assert!(thread.data().is_null());
    let mut marker = 7u32;
    unsafe { thread.set_data((&mut marker as *mut u32).cast()).unwrap() };
    assert_eq!(thread.data(), (&mut marker as *mut u32).cast());
    runtime.exec("shared_value = 1").unwrap();
    thread.sandbox(&runtime.stack()).unwrap();
    // A chunk loaded on the sandboxed thread captures that thread's globals.
    let writer = thread
        .with_stack(&runtime.stack(), |stack| {
            stack.with_frame(|frame| {
                let chunk = runtime.load(
                    frame,
                    "=sandboxed",
                    "shared_value = 2 new_value = 3 return shared_value",
                    &CompileOptions::default(),
                )?;
                l3i::value::Function::from_value(Value::store(chunk)?)
            })
        })
        .unwrap();
    let Resume::Finished(values) = thread.start(&runtime.stack(), &writer, ()).unwrap() else { panic!() };
    assert_eq!(number(&runtime, &values[0]), 2.0);
    // The sandboxed thread wrote to its own globals proxy, not the main globals.
    assert_eq!(
        runtime.global("shared_value").unwrap().with_value(&runtime.stack(), |_, v| v.read::<f64>()).unwrap(),
        1.0
    );
    assert!(runtime.global("new_value").unwrap().is_nil());
}

#[test]
#[should_panic(expected = "a root stack for this Lua thread is already alive")]
fn a_coroutine_thread_leases_its_stack_like_the_main_thread() {
    let runtime = Runtime::new().unwrap();
    let thread = runtime.new_thread().unwrap();
    let stack = runtime.stack();
    thread
        .with_stack(&stack, |outer| {
            outer.push_number(1.0);
            // A second root on the same coroutine thread would let its frames alias the first's.
            thread.with_stack(&stack, |_inner| Ok(()))
        })
        .unwrap();
}

#[test]
fn thread_stacks_are_leased_per_thread_not_per_vm() {
    let runtime = Runtime::new().unwrap();
    let thread = runtime.new_thread().unwrap();
    let other = runtime.new_thread().unwrap();
    // A root on the main thread and a root on each coroutine coexist: different Lua threads,
    // different stacks.
    let stack = runtime.stack();
    let frame = stack.frame();
    frame.push_number(1.0);
    thread
        .with_stack(&stack, |a| {
            a.push_number(2.0);
            other.with_stack(&stack, |b| {
                b.push_number(3.0);
                assert_eq!((a.top(), b.top(), frame.len()), (1, 1, 1));
                Ok(())
            })
        })
        .unwrap();
    drop(frame);
    drop(stack);
    // Every thread gets its own record, including Lua-created coroutines, and records go away
    // with their threads without disturbing the host's thread data on others.
    runtime.exec("for i = 1, 50 do local co = coroutine.create(function() end) coroutine.resume(co) end").unwrap();
    runtime.collect_garbage();
    runtime.collect_garbage();
    assert!(thread.data().is_null());
}
