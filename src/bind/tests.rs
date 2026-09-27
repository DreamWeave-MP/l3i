//! Ported from `testluaubinding.cpp` and the handoff's 20 binder contracts.

use std::cell::Cell;
use std::rc::Rc;

use super::*;
use crate::convert::{Integer, Vector3};
use crate::error::Error;
use crate::runtime::Runtime;
use crate::stack::{Scope, Type, ValueView};
use crate::userdata::{Userdata, tagged};
use crate::value::Value;

const NAME: &str = "dreamweave.tests.fn";

fn call<R: crate::call::CallResults, A: crate::call::PushArgs>(
    runtime: &Runtime,
    function: &Function,
    args: A,
) -> Result<R> {
    function.invoke::<R, A>(&runtime.stack(), args)
}

fn error_text<R: crate::call::CallResults + std::fmt::Debug, A: crate::call::PushArgs>(
    runtime: &Runtime,
    function: &Function,
    args: A,
) -> String {
    call::<R, A>(runtime, function, args).unwrap_err().to_string()
}

fn nil(runtime: &Runtime) -> Value {
    let stack = runtime.stack();
    let frame = stack.frame();
    Value::store(frame.push_nil()).unwrap()
}

#[test]
fn required_arguments_and_count_diagnostics() {
    let runtime = Runtime::new().unwrap();
    let add = runtime.bind_function(NAME, |a: i32, b: f64| a as f64 + b).unwrap();
    assert_eq!(call::<f64, _>(&runtime, &add, (2, 3.5)).unwrap(), 5.5);
    assert_eq!(
        error_text::<f64, _>(&runtime, &add, (2,)),
        "Lua error: dreamweave.tests.fn: bad argument count (expected at least 2, got 1)"
    );
    assert_eq!(
        error_text::<f64, _>(&runtime, &add, (2, 3, 4)),
        "Lua error: dreamweave.tests.fn: bad argument count (expected at most 2, got 3)"
    );
    let text = error_text::<f64, _>(&runtime, &add, ("x", 3.0));
    assert!(text.starts_with("Lua error: dreamweave.tests.fn: bad argument #1 (expected number): "), "{text}");
    assert!(text.ends_with("expected number, got string"), "{text}");
    let text = error_text::<f64, _>(&runtime, &add, (1, true));
    assert!(text.contains("bad argument #2 (expected number)"), "{text}");
}

#[test]
fn injected_call_and_borrowed_views_consume_no_slots_and_pin_nothing() {
    let runtime = Runtime::new().unwrap();
    let inspect = runtime
        .bind_function(NAME, |call: &Call, view: ValueView, text: &str| {
            assert_eq!(call.argument_count(), 2);
            format!("{:?}:{text}:{}", view.type_of(), call.stack().top())
        })
        .unwrap();
    assert_eq!(call::<String, _>(&runtime, &inspect, (true, "hi")).unwrap(), "Boolean:hi:2");
    let text = error_text::<String, _>(&runtime, &inspect, (true,));
    assert!(text.contains("expected at least 2, got 1"), "{text}");
}

#[test]
fn optional_trailing_absent_nil_and_middle_optional_rules() {
    let runtime = Runtime::new().unwrap();
    let middle = runtime
        .bind_function(NAME, |value: Option<f64>, enabled: bool| {
            format!("{}:{}", value.map_or("nil".to_owned(), |v| format!("{v:.6}")), enabled)
        })
        .unwrap();
    let nil = nil(&runtime);
    assert_eq!(call::<String, _>(&runtime, &middle, (true,)).unwrap(), "nil:true");
    assert_eq!(call::<String, _>(&runtime, &middle, (&nil, true)).unwrap(), "nil:true");
    assert_eq!(call::<String, _>(&runtime, &middle, (4.5, true)).unwrap(), "4.500000:true");
    assert!(error_text::<String, _>(&runtime, &middle, ("bad", true)).contains("dreamweave.tests.fn: bad argument #1"));
    assert!(error_text::<String, _>(&runtime, &middle, (&nil, "bad")).contains("dreamweave.tests.fn: bad argument #2"));
    assert_eq!(
        error_text::<String, _>(&runtime, &middle, (&nil,)),
        "Lua error: dreamweave.tests.fn: bad argument #2 (expected boolean): missing argument"
    );
    assert!(error_text::<String, _>(&runtime, &middle, (true, true)).contains("unused arguments"));
    assert!(error_text::<String, _>(&runtime, &middle, (&nil, true, false)).contains("expected at most 2"));

    let trailing = runtime.bind_function(NAME, |a: i32, b: Option<i32>| a + b.unwrap_or(100)).unwrap();
    assert_eq!(call::<i32, _>(&runtime, &trailing, (1,)).unwrap(), 101);
    assert_eq!(call::<i32, _>(&runtime, &trailing, (1, &nil)).unwrap(), 101);
    assert_eq!(call::<i32, _>(&runtime, &trailing, (1, 2)).unwrap(), 3);
    assert!(error_text::<i32, _>(&runtime, &trailing, (1, "x")).contains("bad argument #2 (expected number)"));
}

#[test]
fn middle_optionals_work_during_overload_probing() {
    let runtime = Runtime::new().unwrap();
    let overload = runtime
        .bind_function(
            NAME,
            Overload((
                |value: Option<f64>, enabled: bool| {
                    format!("middle:{}:{enabled}", if value.is_some() { "value" } else { "nil" })
                },
                |value: &str| format!("string:{value}"),
            )),
        )
        .unwrap();
    let nil = nil(&runtime);
    assert_eq!(call::<String, _>(&runtime, &overload, (true,)).unwrap(), "middle:nil:true");
    assert_eq!(call::<String, _>(&runtime, &overload, (&nil, true)).unwrap(), "middle:nil:true");
    assert_eq!(call::<String, _>(&runtime, &overload, (2.5, false)).unwrap(), "middle:value:false");
    assert_eq!(call::<String, _>(&runtime, &overload, ("fallback",)).unwrap(), "string:fallback");
    assert_eq!(
        error_text::<String, _>(&runtime, &overload, (1, 2, 3)),
        "Lua error: dreamweave.tests.fn: no matching overload"
    );
}

#[test]
fn optional_value_view_uses_the_same_probe_and_materialization_rules() {
    let runtime = Runtime::new().unwrap();
    let f = runtime.bind_function(NAME, |value: Option<ValueView>, enabled: bool| value.is_some() && enabled).unwrap();
    let nil = nil(&runtime);
    let table = Value::new_table(&runtime.stack(), 0, 0).unwrap();
    assert!(!call::<bool, _>(&runtime, &f, (&nil, true)).unwrap());
    assert!(call::<bool, _>(&runtime, &f, (&table, true)).unwrap());
    assert!(!call::<bool, _>(&runtime, &f, (&nil, false)).unwrap());
}

#[test]
fn typed_varargs_and_heterogeneous_arg_view_tails() {
    let runtime = Runtime::new().unwrap();
    let sum = runtime
        .bind_function(NAME, |base: i32, rest: VarArgs<i32>| {
            assert_eq!(rest.start_slot, 2);
            base + rest.values.iter().sum::<i32>()
        })
        .unwrap();
    assert_eq!(call::<i32, _>(&runtime, &sum, (1,)).unwrap(), 1);
    assert_eq!(call::<i32, _>(&runtime, &sum, (1, 2, 3, 4)).unwrap(), 10);
    let text = error_text::<i32, _>(&runtime, &sum, (1, 2, "x"));
    assert!(text.contains("bad argument #3 (expected number)"), "{text}");

    let optional_tail = runtime
        .bind_function(NAME, |rest: VarArgs<Option<i32>>| rest.values.iter().filter(|v| v.is_none()).count() as i32)
        .unwrap();
    let nil = nil(&runtime);
    assert_eq!(call::<i32, _>(&runtime, &optional_tail, (1, &nil, 3, &nil)).unwrap(), 2);

    let describe = runtime
        .bind_function(NAME, |first: i32, rest: ArgView| {
            let kinds: Vec<String> = (0..rest.len()).map(|i| format!("{:?}", rest.get(i).type_of())).collect();
            format!("{first}:{}:{}", rest.first_slot(), kinds.join(","))
        })
        .unwrap();
    assert_eq!(call::<String, _>(&runtime, &describe, (7, "a", true, 2.0)).unwrap(), "7:2:String,Boolean,Number");
    assert_eq!(call::<String, _>(&runtime, &describe, (7,)).unwrap(), "7:2:");

    let forward = runtime
        .bind_function(NAME, |call: &Call, rest: ArgView| -> Result<StackResults> {
            rest.copy_to(call)?;
            Ok(StackResults)
        })
        .unwrap();
    assert_eq!(call::<(i32, String), _>(&runtime, &forward, (1, "two")).unwrap(), (1, "two".to_owned()));
}

#[test]
fn return_adapters_cover_every_shape() {
    let runtime = Runtime::new().unwrap();
    let stack = runtime.stack();
    let unit = runtime.bind_function(NAME, || ()).unwrap();
    assert_eq!(unit.invoke_multi(&stack, ()).unwrap().len(), 0);

    let optional = runtime.bind_function(NAME, |present: bool| present.then_some(7i32)).unwrap();
    assert_eq!(call::<Option<i32>, _>(&runtime, &optional, (true,)).unwrap(), Some(7));
    assert_eq!(optional.invoke_multi(&stack, (false,)).unwrap().len(), 1);
    assert!(call::<Value, _>(&runtime, &optional, (false,)).unwrap().is_nil());

    let tuple = runtime.bind_function(NAME, || (1i32, "two", Vector3::new(1.0, 2.0, 3.0))).unwrap();
    assert_eq!(
        call::<(i32, String, Vector3), _>(&runtime, &tuple, ()).unwrap(),
        (1, "two".to_owned(), Vector3::new(1.0, 2.0, 3.0))
    );

    let variadic = runtime.bind_function(NAME, |n: i32| Variadic((0..n).collect::<Vec<i32>>())).unwrap();
    assert_eq!(variadic.invoke_multi(&stack, (3,)).unwrap().len(), 3);
    assert_eq!(variadic.invoke_multi(&stack, (0,)).unwrap().len(), 0);
    let optional_variadic =
        runtime.bind_function(NAME, |some: bool| if some { Some(Variadic(vec![1i32, 2])) } else { None }).unwrap();
    assert_eq!(optional_variadic.invoke_multi(&stack, (true,)).unwrap().len(), 2);
    assert!(optional_variadic.invoke_multi(&stack, (false,)).unwrap()[0].is_nil());

    let result_or_error = runtime
        .bind_function(
            NAME,
            |ok: bool| if ok { ResultOrError::Success(42i32) } else { ResultOrError::Failure("nope".into()) },
        )
        .unwrap();
    assert_eq!(
        call::<(Option<i32>, Option<String>), _>(&runtime, &result_or_error, (true,)).unwrap(),
        (Some(42), None)
    );
    assert_eq!(
        call::<(Option<i32>, Option<String>), _>(&runtime, &result_or_error, (false,)).unwrap(),
        (None, Some("nope".to_owned()))
    );

    let nil_then = runtime
        .bind_function(NAME, |ok: bool| if ok { NilThen::success(5i32) } else { NilThen::failure(9i32) })
        .unwrap();
    assert_eq!(nil_then.invoke_multi(&stack, (true,)).unwrap().len(), 1);
    assert_eq!(call::<(Option<i32>, Option<i32>), _>(&runtime, &nil_then, (false,)).unwrap(), (None, Some(9)));

    let raw = runtime
        .bind_function(NAME, |call: &Call| {
            call.push(&1i32).unwrap();
            call.push(&Integer(2)).unwrap();
            StackResults
        })
        .unwrap();
    let results = raw.invoke_multi(&stack, ()).unwrap();
    assert_eq!(results.len(), 2);
    assert_eq!(results[1].type_of(), Type::Integer);

    let failing = runtime
        .bind_function(NAME, |fail: bool| -> Result<i32> {
            if fail { Err(Error::runtime("custom failure")) } else { Ok(1) }
        })
        .unwrap();
    assert_eq!(call::<i32, _>(&runtime, &failing, (false,)).unwrap(), 1);
    assert_eq!(error_text::<i32, _>(&runtime, &failing, (true,)), "Lua error: custom failure");
}

#[test]
fn closure_context_is_owned_by_lua_and_dropped_by_gc() {
    struct Tracked(Rc<Cell<usize>>);
    impl Drop for Tracked {
        fn drop(&mut self) {
            self.0.set(self.0.get() + 1);
        }
    }
    let drops = Rc::new(Cell::new(0));
    let runtime = Runtime::new().unwrap();
    {
        let tracked = Tracked(drops.clone());
        let counter = Rc::new(Cell::new(0));
        let counter_in_closure = counter.clone();
        let f = runtime
            .bind_function(NAME, move |x: i32| {
                let _keep = &tracked;
                counter_in_closure.set(counter_in_closure.get() + 1);
                x * 2
            })
            .unwrap();
        assert_eq!(call::<i32, _>(&runtime, &f, (21,)).unwrap(), 42);
        assert_eq!(counter.get(), 1);
        runtime.collect_garbage();
        assert_eq!(drops.get(), 0, "the pinned function keeps its context alive");
        drop(f);
    }
    runtime.collect_garbage();
    runtime.collect_garbage();
    assert_eq!(drops.get(), 1, "the context dropped exactly once after the last pin went away");
}

#[test]
fn table_error_objects_survive_forwarding_unchanged() {
    let runtime = Runtime::new().unwrap();
    let erroring = runtime
        .load_function("local t = {code = 42} t.self = t AnchorTable = t return function() error(t) end")
        .unwrap();
    let proxy = runtime
        .bind_function(NAME, move |call: &Call| -> Result<StackResults> {
            call.forward_to(&erroring, call.argument_count() + 1)?;
            Ok(StackResults)
        })
        .unwrap();
    {
        let stack = runtime.stack();
        let frame = stack.frame();
        proxy.push_to(&frame).unwrap();
        frame.set_global("proxy").unwrap();
    }
    runtime
        .exec("local ok, err = pcall(proxy) assert(not ok) assert(type(err) == 'table') assert(err == AnchorTable) assert(err.self == err) assert(err.code == 42)")
        .unwrap();
}

#[test]
fn tagged_userdata_arguments_are_borrowed_and_checked() {
    struct Probe(f64);
    unsafe impl Userdata for Probe {
        const TAG: Option<u8> = Some(9);
        const NAME: &'static str = "dreamweave.tests.Probe";
    }
    let runtime = Runtime::new().unwrap();
    tagged::register::<Probe>(&runtime, |_| Ok(())).unwrap();
    let make = runtime
        .bind_function(NAME, |call: &Call, value: f64| -> Result<StackResults> {
            tagged::push(call, Probe(value))?;
            Ok(StackResults)
        })
        .unwrap();
    let read = runtime.bind_function(NAME, |probe: &Probe, scale: f64| probe.0 * scale).unwrap();
    let stack = runtime.stack();
    let probe = call::<Value, _>(&runtime, &make, (2.5,)).unwrap();
    assert_eq!(call::<f64, _>(&runtime, &read, (&probe, 2.0)).unwrap(), 5.0);
    let text = error_text::<f64, _>(&runtime, &read, (1, 2.0));
    assert!(text.contains("bad argument #1 (expected dreamweave.tests.Probe)"), "{text}");
    assert!(text.contains("dreamweave.tests.Probe expected, got number"), "{text}");
    assert_eq!(stack.top(), 0);
}

#[test]
fn debug_names_are_validated_against_the_runtime_roots() {
    let runtime = Runtime::new().unwrap();
    assert!(runtime.bind_function("openmw.not.ours", || 1i32).is_err());
    assert!(runtime.bind_function("dreamweave.ok", || 1i32).is_ok());
    let text = runtime.load_function("return function(f) local ok, e = pcall(f, 'x') return e end").unwrap();
    let typed = runtime.bind_function("dreamweave.typed.thing", |n: i32| n).unwrap();
    let message = text.invoke::<String, _>(&runtime.stack(), (&typed,)).unwrap();
    assert!(message.contains("dreamweave.typed.thing: bad argument #1"), "{message}");
}
