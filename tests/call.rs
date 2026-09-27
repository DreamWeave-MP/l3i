//! Ported from `testluaucall.cpp`.

use dream_binder::call::CallResults;
use dream_binder::convert::Integer;
use dream_binder::stack::Type;
use dream_binder::value::{Function, Value};
use dream_binder::{Error, Runtime};

fn load(runtime: &Runtime, source: &str) -> Function {
    runtime.load_function(source).unwrap()
}

#[test]
fn call_family_success_semantics() {
    let runtime = Runtime::new().unwrap();
    let nothing = load(&runtime, "return function() end");
    let nil_fn = load(&runtime, "return function() return nil end");
    let multi = load(&runtime, "return function() return 1, 'two' end");
    let stack = runtime.stack();

    assert!(nothing.invoke::<Value, _>(&stack, ()).unwrap().is_nil());
    assert!(nil_fn.invoke::<Value, _>(&stack, ()).unwrap().is_nil());
    assert_eq!(multi.invoke::<i32, _>(&stack, ()).unwrap(), 1);
    nothing.invoke::<(), _>(&stack, ()).unwrap();
    multi.invoke::<(), _>(&stack, ()).unwrap(); // truncates multret without error
    assert_eq!(multi.invoke::<(i32, String), _>(&stack, ()).unwrap(), (1, "two".to_owned()));
    assert_eq!(multi.invoke::<(i32, String, Option<i32>), _>(&stack, ()).unwrap(), (1, "two".to_owned(), None));

    assert!(nothing.invoke_multi(&stack, ()).unwrap().is_empty());
    let results = multi.invoke_multi(&stack, ()).unwrap();
    assert_eq!(results.len(), 2);
    assert_eq!(results[0].type_of(), Type::Number);
    assert_eq!(results[1].type_of(), Type::String);

    assert!(nothing.invoke_with_values::<Value>(&stack, &[]).unwrap().is_nil());
    let mut saw_nil = false;
    nothing
        .invoke_with(&stack, (), |_, view| {
            saw_nil = view.is_nil();
            Ok(())
        })
        .unwrap();
    assert!(saw_nil);
    let first = multi.invoke_with(&stack, (), |_, view| view.read::<i32>()).unwrap();
    assert_eq!(first, 1);
    assert_eq!(stack.top(), 0);
}

#[test]
fn call_family_error_parity_and_balance() {
    let runtime = Runtime::new().unwrap();
    let boom = load(&runtime, "return function() error('boom') end");
    let stack = runtime.stack();

    let expect_boom = |error: Error| {
        let text = error.to_string();
        assert!(text.starts_with("Lua error: ") && text.ends_with("boom"), "{text}");
    };
    expect_boom(boom.invoke::<Value, _>(&stack, ()).unwrap_err());
    expect_boom(boom.invoke::<(), _>(&stack, ()).unwrap_err());
    expect_boom(boom.invoke_multi(&stack, ()).unwrap_err());
    expect_boom(boom.invoke_with_values::<Value>(&stack, &[]).unwrap_err());
    let mut visited = false;
    expect_boom(
        boom.invoke_with(&stack, (), |_, _| {
            visited = true;
            Ok(())
        })
        .unwrap_err(),
    );
    assert!(!visited, "visitor must not run on failure");
    assert_eq!(stack.top(), 0);

    let invalid = Function::default();
    let logic = Error::logic("Cannot call an invalid Lua function reference");
    assert_eq!(invalid.invoke::<(), _>(&stack, ()).unwrap_err(), logic);
    assert_eq!(invalid.invoke_multi(&stack, ()).unwrap_err(), logic);
    assert_eq!(invalid.invoke_with_values::<()>(&stack, &[]).unwrap_err(), logic);
    assert_eq!(invalid.invoke_with(&stack, (), |_, _| Ok(())).unwrap_err(), logic);

    // Non-string error objects report their type name.
    let table_error = load(&runtime, "return function() error({}) end");
    assert_eq!(table_error.invoke::<(), _>(&stack, ()).unwrap_err(), Error::runtime("Lua error: table"));

    // Borrowed-view invocation has its own wording.
    let frame = stack.frame();
    let view = boom.push_to(&frame).unwrap().as_function().unwrap();
    let error = view.invoke::<i32, ()>(&frame, ()).unwrap_err().to_string();
    assert!(error.starts_with("Lua error at stack index 2: "), "{error}");
    assert!(error.ends_with("boom"), "{error}");
    assert_eq!(frame.len(), 1);
}

#[test]
fn multi_enforces_the_result_budget() {
    let runtime = Runtime::new().unwrap();
    runtime
        .exec("function returnValues(n) local t = {} for i = 1, n do t[i] = 1 end return table.unpack(t) end")
        .unwrap();
    let stack = runtime.stack();
    let ok = load(&runtime, "return function() return returnValues(256) end");
    assert_eq!(ok.invoke_multi(&stack, ()).unwrap().len(), 256);
    let too_many = load(&runtime, "return function() return returnValues(300) end");
    assert_eq!(too_many.invoke_multi(&stack, ()).unwrap_err(), Error::runtime("Lua error: too many return values"));
    assert_eq!(stack.top(), 0);
}

#[test]
fn with_result_visitor_and_argument_edges() {
    let runtime = Runtime::new().unwrap();
    let stack = runtime.stack();
    let add = load(&runtime, "return function(a, b) return a + b, a * b end");
    assert_eq!(add.invoke_with(&stack, (2, 3), |_, view| view.read::<i32>()).unwrap(), 5);
    let concat = load(&runtime, "return function(a, b) return a .. b end");
    assert_eq!(concat.invoke::<String, _>(&stack, ("hello", "world")).unwrap(), "helloworld");
    let error = add
        .invoke_with(&stack, (2, 3), |frame, _| {
            frame.push_number(99.0);
            Err::<(), _>(Error::logic("visitor failure"))
        })
        .unwrap_err();
    assert_eq!(error, Error::logic("visitor failure"));
    assert_eq!(stack.top(), 0);

    let plain_add = load(&runtime, "return function(a, b) return a + b end");
    let error = plain_add.invoke::<i32, _>(&stack, ()).unwrap_err().to_string();
    assert!(error.contains("attempt to perform arithmetic"), "{error}");
    assert_eq!(plain_add.invoke::<i32, _>(&stack, (2, 3)).unwrap(), 5);
    assert_eq!(plain_add.invoke::<i32, _>(&stack, (2, 3, 99)).unwrap(), 5, "extras ignored");

    let count = load(&runtime, "return function(...) return select('#', ...) end");
    assert_eq!(count.invoke_with_values::<i32>(&stack, &[]).unwrap(), 0);
    let two = vec![Value::new_table(&stack, 0, 0).unwrap(), Value::new_table(&stack, 0, 0).unwrap()];
    assert_eq!(count.invoke_with_values::<i32>(&stack, &two).unwrap(), 2);
    let many: Vec<Value> = (0..300).map(|_| Value::new_table(&stack, 0, 0).unwrap()).collect();
    assert_eq!(count.invoke_with_values::<i32>(&stack, &many).unwrap(), 300);
    assert_eq!(stack.top(), 0);
}

#[test]
fn integer_kind_is_preserved_through_the_call_boundary() {
    let runtime = Runtime::new().unwrap();
    let stack = runtime.stack();
    let echo = load(&runtime, "return function(v) return v end");
    let kind_of = load(&runtime, "return function(v) return type(v) end");
    let echoed = echo.invoke::<Value, _>(&stack, (Integer(i64::MAX),)).unwrap();
    assert_eq!(echoed.type_of(), Type::Integer);
    assert_eq!(echo.invoke::<i64, _>(&stack, (Integer(i64::MAX),)).unwrap(), i64::MAX);
    assert_eq!(kind_of.invoke::<String, _>(&stack, (Integer(1),)).unwrap(), "integer");
    assert_eq!(echo.invoke::<Value, _>(&stack, (7i64,)).unwrap().type_of(), Type::Number);
    assert_eq!(kind_of.invoke::<String, _>(&stack, (7i64,)).unwrap(), "number");
}

#[test]
fn yield_outside_a_coroutine_errors_cleanly() {
    let runtime = Runtime::new().unwrap();
    let stack = runtime.stack();
    let yielder = load(&runtime, "return function() coroutine.yield('yielded') end");
    let error = yielder.invoke::<(), _>(&stack, ()).unwrap_err().to_string();
    assert!(error.contains("attempt to yield across metamethod/C-call boundary"), "{error}");
    assert_eq!(stack.top(), 0);
    let add = load(&runtime, "return function(a, b) return a + b end");
    assert_eq!(add.invoke::<i32, _>(&stack, (2, 3)).unwrap(), 5);
}

#[test]
fn compiled_source_can_be_invoked_through_a_view() {
    let runtime = Runtime::new().unwrap();
    let stack = runtime.stack();
    let frame = stack.frame();
    let chunk = runtime.load(&frame, "=value-test", "return 6 * 7", &Default::default()).unwrap();
    assert_eq!(chunk.as_function().unwrap().invoke::<i32, ()>(&frame, ()).unwrap(), 42);
    assert_eq!(<() as CallResults>::COUNT, 0);
    assert!(frame.push_number(1.0).as_function().is_err());
}
