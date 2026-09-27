//! Ported from `testluavalue.cpp` (StackViewsAndRegistryReferences, NestedTableInspection,
//! ChainedRawLookups, TypedFunctionView's forEach) and `testluaustack.cpp`
//! (TableCapacityHintsAndIntegerRawWritesAreChecked, CheckedFieldReadsReportContextAndBalance).

use crate::convert::Integer;
use crate::error::Error;
use crate::runtime::Runtime;
use crate::stack::{Scope, Type};
use crate::value::{Table, Value};

#[test]
fn integer_and_number_keys_are_checked_and_distinct() {
    let runtime = Runtime::new().unwrap();
    let stack = runtime.stack();
    let frame = stack.frame();
    let table = frame.push_table(3, 2).unwrap();
    frame.push(&"integer").unwrap();
    table.raw_set_index(&frame, 1).unwrap();
    frame.push(&"size").unwrap();
    table.raw_set_index(&frame, 2).unwrap();
    frame.push(&"negative").unwrap();
    table.raw_set_index(&frame, -2).unwrap();
    frame.push(&"fractional").unwrap();
    table.raw_set_number_key(&frame, 1.5).unwrap();
    assert_eq!(frame.len(), 1);

    frame
        .with_frame(|lookups| {
            assert_eq!(table.raw_get_index(lookups, 1)?.read::<&str>()?, "integer");
            assert_eq!(table.raw_get_index(lookups, 2)?.read::<&str>()?, "size");
            assert_eq!(table.raw_get_index(lookups, -2)?.read::<&str>()?, "negative");
            assert_eq!(table.get_index(lookups, 1)?.read::<&str>()?, "integer");
            assert!(table.raw_get_index(lookups, 3)?.is_nil());
            Ok(())
        })
        .unwrap();
    assert_eq!(table.raw_len(), 2);
    assert_eq!(table.len(&frame).unwrap(), 2);

    let out_of_range = i64::from(i32::MAX) + 1;
    frame.push_nil();
    assert_eq!(
        table.raw_set_index(&frame, out_of_range).unwrap_err(),
        Error::logic("Lua table integer key exceeds the supported range")
    );
    assert!(table.raw_get_index(&frame, out_of_range).is_err());
    assert!(frame.push_table(usize::MAX, 0).is_err());
    assert!(Value::new_table(&frame, 0, usize::MAX).is_err());
}

#[test]
fn checked_field_reads_report_context_and_stay_balanced() {
    let runtime = Runtime::new().unwrap();
    runtime
        .exec("child = {bad = 'not a number'} parent = {derived = 7} setmetatable(child, {__index = parent})")
        .unwrap();
    let stack = runtime.stack();
    let frame = stack.frame();
    let child = Value::get_global(&frame, "child").unwrap().push_to(&frame).unwrap().as_table().unwrap();
    let before = frame.len();

    let error = child.raw_get_optional::<i32>(&frame, "bad", "player stats").unwrap_err();
    assert_eq!(error, Error::logic("player stats \"bad\" has an invalid value \"not a number\""));
    assert_eq!(frame.len(), before);

    // raw bypasses __index; get honours it.
    assert_eq!(child.raw_get_optional::<i32>(&frame, "derived", "").unwrap(), None);
    assert_eq!(child.get_optional::<i32>(&frame, "derived", "").unwrap(), Some(7));
    assert_eq!(child.get_optional::<i32>(&frame, "missing", "").unwrap(), None);
    assert_eq!(child.get_as::<i32>(&frame, "derived").unwrap(), 7);
    assert_eq!(frame.len(), before);

    let long_key = "k".repeat(80);
    child.raw_set_value(&frame, &long_key, &"x\"y\n").unwrap();
    let error = child.get_optional::<i32>(&frame, &long_key, "ctx").unwrap_err().to_string();
    assert_eq!(error, format!("ctx \"{}...\" has an invalid value \"x\\\"y\\n\"", "k".repeat(64)));
    assert_eq!(frame.len(), before);
}

#[test]
fn for_each_and_nested_lookups_do_not_corrupt_iteration() {
    let runtime = Runtime::new().unwrap();
    let stack = runtime.stack();
    let frame = stack.frame();
    let table = frame.push_table(0, 3).unwrap();
    table.raw_set_value(&frame, "one", &1i32).unwrap();
    table.raw_set_value(&frame, "two", &2i32).unwrap();
    let nested = frame.push_table(0, 1).unwrap();
    nested.raw_set_value(&frame, "value", &7i32).unwrap();
    table.raw_set(&frame, "nested").unwrap();
    let before = frame.len();

    let mut sum = 0;
    table
        .for_each(&frame, |step, key, value| {
            if key.is_string() && value.is_number() {
                sum += value.read::<i32>()?;
            }
            if let Ok(inner) = value.as_table() {
                sum += inner.get_as::<i32>(step, "value")?;
            }
            Ok(())
        })
        .unwrap();
    assert_eq!(sum, 10);
    assert_eq!(frame.len(), before);

    assert!(table.find_key(&frame, |key| Ok(key.read::<&str>()? == "two")).unwrap());
    assert!(!table.find_key(&frame, |key| Ok(key.read::<&str>()? == "three")).unwrap());
    assert_eq!(frame.len(), before);

    // Chained lookups stay balanced inside one nested frame.
    frame
        .with_frame(|lookups| {
            let answer = table.get(lookups, "nested")?.as_table()?.get(lookups, "value")?.read::<i32>()?;
            assert_eq!(answer, 7);
            assert_eq!(table.get(lookups, "nested")?.type_of(), Type::Table);
            Ok(())
        })
        .unwrap();
    assert_eq!(frame.len(), before);
    let error = table.with_field(&frame, "one", |value| value.read::<String>()).unwrap_err();
    assert!(error.to_string().ends_with("expected string, got number"));
    assert_eq!(frame.len(), before);
}

#[test]
fn cold_tier_table_access_leaves_the_stack_untouched() {
    let runtime = Runtime::new().unwrap();
    let stack = runtime.stack();
    let table = Table::new(&stack, 0, 2).unwrap();
    table.set(&stack, "answer", &42i32).unwrap();
    table.set(&stack, "big", &Integer(i64::MAX)).unwrap();
    assert_eq!(table.get::<i32>(&stack, "answer").unwrap(), 42);
    assert_eq!(table.get::<i64>(&stack, "big").unwrap(), i64::MAX);
    assert_eq!(table.get::<Option<i32>>(&stack, "missing").unwrap(), None);
    assert!(table.get::<i32>(&stack, "missing").is_err());
    assert_eq!(stack.top(), 0);
    runtime.collect_garbage();
    assert_eq!(table.get::<i32>(&stack, "answer").unwrap(), 42);
}

#[test]
fn length_honours_len_metamethod_and_errors_propagate_at_host_level() {
    let runtime = Runtime::new().unwrap();
    runtime.exec("counted = setmetatable({}, {__len = function() return 9 end}) failing = setmetatable({}, {__len = function() error('no length', 0) end})").unwrap();
    let stack = runtime.stack();
    let frame = stack.frame();
    let counted = Value::get_global(&frame, "counted").unwrap().push_to(&frame).unwrap().as_table().unwrap();
    assert_eq!(counted.len(&frame).unwrap(), 9);
    assert_eq!(counted.raw_len(), 0);
    let failing = Value::get_global(&frame, "failing").unwrap().push_to(&frame).unwrap().as_table().unwrap();
    assert_eq!(failing.len(&frame).unwrap_err(), Error::runtime("no length"));
    assert_eq!(frame.len(), 2);
}
