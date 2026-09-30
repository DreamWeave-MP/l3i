//! Luau's debug API through the binder: activation records, locals and upvalues, breakpoints
//! that stop a host-driven coroutine, single stepping, coverage, and the lifecycle hooks.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use l3i::Runtime;
use l3i::bind::Call;
use l3i::debug::{CallSite, DebugAction, DebugInfo, DebugScope, HookSet, RuntimeHooks};
use l3i::source::CompileOptions;
use l3i::stack::Stack;
use l3i::thread::Resume;
use l3i::value::{Function, Value};

#[test]
fn activation_records_locals_arguments_and_upvalues_are_readable_from_a_bound_function() {
    let runtime = Runtime::new().unwrap();
    let seen: Rc<RefCell<Vec<String>>> = Rc::new(RefCell::new(Vec::new()));
    let record = seen.clone();
    let probe = runtime
        .bind_function("dreamweave.probe", move |call: &Call| {
            let mut notes = record.borrow_mut();
            let caller = call.debug_info(1).expect("a Lua caller");
            notes.push(format!(
                "caller {} line {} what {}",
                caller.name.clone().unwrap_or_default(),
                caller.current_line,
                caller.what
            ));
            let me = call.debug_info(0).expect("self");
            notes.push(format!("self what {}", me.what));
            if let Some((name, view)) = call.local(1, 1) {
                notes.push(format!("local {name} = {}", view.read::<i32>().unwrap_or(-1)));
            }
            if let Some(view) = call.argument(1, 1) {
                notes.push(format!("argument 1 = {}", view.read::<i32>().unwrap_or(-1)));
            }
            notes.push(format!("depth {}", call.stack_depth()));
            notes.push(format!("trace has caller: {}", call.debug_trace().contains("inner")));
            notes.push(call.traceback(Some("tb"), 0).unwrap().lines().next().unwrap_or_default().to_owned());
            // The cheap query gives the same chunk name and line as the full record.
            assert_eq!(call.call_site(1), Some(CallSite { source: "probe".to_owned(), line: 3 }));
            assert_eq!(call.call_site(0), Some(CallSite { source: "[C]".to_owned(), line: -1 }));
            assert_eq!(call.call_site(1).map(|site| site.line), call.debug_info(1).map(|info| info.current_line));
            assert_eq!(call.call_site(99), None);
        })
        .unwrap();
    runtime.set_global("probe", &probe).unwrap();
    // Optimisation level 1 keeps `inner` an actual call frame (level 2 would inline it), and
    // debug level 2 records local names.
    let options = CompileOptions { optimization_level: 1, debug_level: 2, ..CompileOptions::default() };
    runtime
        .stack()
        .with_frame(|frame| {
            let chunk = runtime.load(
                frame,
                "=probe",
                "local function inner(a)\n  local x = a * 2\n  probe()\n  return x\nend\ninner(21)",
                &options,
            )?;
            chunk.as_function()?.invoke::<(), ()>(frame, ())
        })
        .unwrap();
    let notes = seen.borrow();
    assert!(notes[0].starts_with("caller inner line 3 what Lua"), "{notes:?}");
    assert_eq!(notes[1], "self what C");
    assert_eq!(notes[2], "local a = 21");
    assert_eq!(notes[3], "argument 1 = 21");
    assert!(notes[4].starts_with("depth "), "{notes:?}");
    assert_eq!(notes[5], "trace has caller: true");
    assert_eq!(notes[6], "tb");

    // Function records and upvalues from the host (upvalue names also need debug level 2).
    let counter: Function = runtime
        .stack()
        .with_frame(|frame| {
            let chunk = runtime.load(
                frame,
                "=counter",
                "local n = 10 return function(step) n = n + step return n end",
                &options,
            )?;
            chunk.as_function()?.invoke::<Function, ()>(frame, ())
        })
        .unwrap();
    let stack = runtime.stack();
    stack
        .with_frame(|frame| {
            let view = counter.push_to(frame)?;
            let info = frame.function_info(view)?;
            assert_eq!(
                (info.what.as_str(), info.parameter_count, info.upvalue_count, info.line_defined),
                ("Lua", 1, 1, 1)
            );
            let (name, value) = frame.upvalue(view, 1).expect("upvalue n");
            assert_eq!((name.as_str(), value.read::<i32>()?), ("n", 10));
            frame.push_number(100.0);
            assert_eq!(frame.set_upvalue(view, 1).as_deref(), Some("n"));
            Ok(())
        })
        .unwrap();
    drop(stack);
    assert_eq!(counter.invoke::<i32, _>(&runtime.stack(), (1,)).unwrap(), 101);
}

struct Debugger {
    breaks: Rc<RefCell<Vec<i32>>>,
    steps: Rc<Cell<u32>>,
    hits: Cell<u32>,
}

impl RuntimeHooks for Debugger {
    fn debug_break(&self, stack: &Stack<'_>, info: &DebugInfo) -> DebugAction {
        self.hits.set(self.hits.get() + 1);
        // Break on the first visit, step off on the second (the instruction re-executes after a
        // resume), as Luau's conformance tests do.
        if self.hits.get() % 2 == 1 {
            self.breaks.borrow_mut().push(info.current_line);
            // The stopped function's locals are readable from the hook: 1 is the parameter.
            let (name, value) = stack.local(0, 2).expect("local a");
            assert_eq!((name.as_str(), value.read::<i32>().unwrap()), ("a", 1));
            DebugAction::Break
        } else {
            DebugAction::Continue
        }
    }
    fn debug_step(&self, _stack: &Stack<'_>, _info: &DebugInfo) -> DebugAction {
        self.steps.set(self.steps.get() + 1);
        DebugAction::Continue
    }
}

#[test]
fn breakpoints_stop_a_host_driven_coroutine_and_single_stepping_counts_instructions() {
    let runtime = Runtime::new().unwrap();
    let breaks = Rc::new(RefCell::new(Vec::new()));
    let steps = Rc::new(Cell::new(0u32));
    runtime.set_hooks(Debugger { breaks: breaks.clone(), steps: steps.clone(), hits: Cell::new(0) }, HookSet::DEBUGGER);
    // Values flow from the parameter so no line folds away; debug level 2 names the locals.
    let options = CompileOptions { debug_level: 2, ..CompileOptions::default() };
    let body: Function = runtime
        .stack()
        .with_frame(|frame| {
            let chunk = runtime.load(
                frame,
                "=bp",
                "return function(n)\n  local a = n\n  local b = a + 1\n  local c = b + 1\n  return c\nend",
                &options,
            )?;
            chunk.as_function()?.invoke::<Function, ()>(frame, ())
        })
        .unwrap();
    // Line 4 (`local c = ...`) gets the breakpoint.
    let landed = runtime
        .stack()
        .with_frame(|frame| {
            let view = body.push_to(frame)?;
            frame.set_breakpoint(view, 4, true)
        })
        .unwrap();
    assert_eq!(landed, 4);
    let thread = runtime.new_thread().unwrap();
    assert!(matches!(thread.start(&runtime.stack(), &body, (1,)).unwrap(), Resume::Break));
    assert_eq!(breaks.borrow().as_slice(), [4]);
    let Resume::Finished(values) = thread.resume(&runtime.stack(), ()).unwrap() else { panic!("expected the return") };
    assert_eq!(values[0].with_value(&runtime.stack(), |_, v| v.read::<i32>()).unwrap(), 3);

    // Clearing the breakpoint and stepping instead.
    runtime
        .stack()
        .with_frame(|frame| {
            let view = body.push_to(frame)?;
            frame.set_breakpoint(view, 4, false).map(|_| ())
        })
        .unwrap();
    let thread = runtime.new_thread().unwrap();
    thread
        .with_stack(&runtime.stack(), |stack| {
            stack.single_step(true);
            Ok(())
        })
        .unwrap();
    assert!(matches!(thread.start(&runtime.stack(), &body, (1,)).unwrap(), Resume::Finished(_)));
    assert!(steps.get() >= 3, "instructions stepped: {}", steps.get());
    assert_eq!(breaks.borrow().len(), 1, "no further breaks");
    runtime.clear_hooks();
}

#[test]
fn coverage_counts_line_hits_when_compiled_with_coverage() {
    let runtime = Runtime::new().unwrap();
    let options = CompileOptions { coverage_level: 1, ..CompileOptions::default() };
    let function: Function = runtime
        .stack()
        .with_frame(|frame| {
            let chunk = runtime.load(
                frame,
                "=cov",
                "return function(n)\n  local s = 0\n  for i = 1, n do\n    s = s + i\n  end\n  return s\nend",
                &options,
            )?;
            chunk.as_function()?.invoke::<Function, ()>(frame, ())
        })
        .unwrap();
    assert_eq!(function.invoke::<i32, _>(&runtime.stack(), (5,)).unwrap(), 15);
    let entries = runtime
        .stack()
        .with_frame(|frame| {
            let view = function.push_to(frame)?;
            frame.coverage(view)
        })
        .unwrap();
    assert!(!entries.is_empty());
    let body = &entries[0];
    assert_eq!(body.line_defined, 1);
    // Line 4 (the loop body) ran five times; line 2 once. Lines without code are -1.
    assert!(body.hits.len() > 4, "{body:?}");
    assert_eq!(body.hits[4], 5, "{body:?}");
    assert_eq!(body.hits[2], 1, "{body:?}");
}

struct Lifecycle {
    threads: Rc<Cell<u32>>,
    resumes: Rc<Cell<u32>>,
    frees: Rc<Cell<u32>>,
    protected_errors: Rc<Cell<u32>>,
}

impl RuntimeHooks for Lifecycle {
    fn user_thread(&self, parent: Option<*mut l3i::ffi::lua_State>, _thread: *mut l3i::ffi::lua_State) {
        if parent.is_some() {
            self.threads.set(self.threads.get() + 1);
        }
    }
    fn pre_resume(&self, _: *mut l3i::ffi::lua_State) {
        self.resumes.set(self.resumes.get() + 1);
    }
    fn on_free(&self, _: *mut l3i::ffi::lua_State, _: *mut std::ffi::c_void) {
        self.frees.set(self.frees.get() + 1);
    }
    fn debug_protected_error(&self, stack: &Stack<'_>) {
        assert!(stack.top() >= 1, "the error object is on the stack");
        self.protected_errors.set(self.protected_errors.get() + 1);
    }
}

#[test]
fn lifecycle_hooks_observe_threads_resumes_frees_and_protected_errors() {
    let runtime = Runtime::new().unwrap();
    let (threads, resumes, frees, errors) =
        (Rc::new(Cell::new(0)), Rc::new(Cell::new(0)), Rc::new(Cell::new(0)), Rc::new(Cell::new(0)));
    runtime.set_hooks(
        Lifecycle {
            threads: threads.clone(),
            resumes: resumes.clone(),
            frees: frees.clone(),
            protected_errors: errors.clone(),
        },
        HookSet { user_thread: true, pre_resume: true, on_free: true, debug_protected_error: true, ..HookSet::NONE },
    );
    runtime
        .exec("local co = coroutine.create(function() coroutine.yield() end) coroutine.resume(co) coroutine.resume(co)")
        .unwrap();
    assert_eq!(threads.get(), 1);
    assert_eq!(resumes.get(), 2);
    // The protected-error hook fires for errors inside coroutines (yieldable threads).
    runtime.exec("coroutine.wrap(function() pcall(error, 'x') end)()").unwrap();
    assert!(errors.get() >= 1);
    runtime.exec("local t = {} for i = 1, 100 do t[i] = {} end").unwrap();
    runtime.collect_garbage();
    runtime.collect_garbage();
    assert!(frees.get() > 0);
    runtime.clear_hooks();
    let before = frees.get();
    runtime.exec("local t = {} for i = 1, 100 do t[i] = {} end").unwrap();
    runtime.collect_garbage();
    runtime.collect_garbage();
    assert_eq!(frees.get(), before, "cleared hooks are not called");
    let _ = Value::invalid();
}
