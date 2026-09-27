use super::*;

fn with_stack(body: impl FnOnce(&Stack<'_>)) {
    let runtime = crate::runtime::Runtime::new().unwrap();
    body(&runtime.stack());
}

#[test]
fn negative_indexes_resolve_against_the_current_top() {
    with_stack(|stack| {
        let frame = stack.frame();
        let base = frame.floor();
        frame.push_number(1.0);
        frame.push_number(2.0);
        let second = frame.at(-1);
        assert_eq!(second.index(), base + 2);
        frame.push_number(3.0);
        // The view still names slot base+2 after another push.
        assert_eq!(second.type_of(), Type::Number);
        assert_eq!(frame.at(0).type_of(), Type::None);
        assert_eq!(frame.at(-100).type_of(), Type::None);
        assert_eq!(frame.at(base + 4).type_of(), Type::None);
        assert_eq!(frame.at(base + 4000).type_of(), Type::None);
    });
}

#[test]
fn frames_restore_the_entry_height_and_never_grow() {
    with_stack(|stack| {
        let base = stack.top();
        {
            let frame = stack.frame();
            frame.push_nil();
            frame.push_boolean(true);
            assert_eq!(frame.len(), 2);
        }
        assert_eq!(stack.top(), base);

        let mut frame = stack.frame();
        frame.push_nil();
        frame.release();
        drop(frame);
        assert_eq!(stack.top(), base + 1, "a released frame leaves its value behind");

        // Nested frames close in order, each to its own floor.
        let outer = stack.frame();
        outer.push_number(1.0);
        {
            let inner = outer.frame();
            inner.push_number(2.0);
            assert_eq!(inner.len(), 1);
        }
        assert_eq!(outer.len(), 1);
    });
}

#[test]
#[should_panic(expected = "a frame is already open on this scope")]
fn sibling_frames_are_rejected_at_the_opening_line() {
    with_stack(|stack| {
        let _first = stack.frame();
        let _second = stack.frame();
    });
}

#[test]
#[should_panic(expected = "a frame is already open on this scope")]
fn sibling_nested_frames_are_rejected_too() {
    with_stack(|stack| {
        let outer = stack.frame();
        let _a = outer.frame();
        let _b = outer.frame();
    });
}

#[test]
fn a_new_frame_may_open_once_the_previous_one_closed() {
    with_stack(|stack| {
        {
            let first = stack.frame();
            first.push_nil();
        }
        let second = stack.frame();
        second.push_nil();
        assert_eq!(second.len(), 1);
    });
}

#[test]
fn pop_is_clamped_to_the_frame_floor() {
    with_stack(|stack| {
        let outer = stack.frame();
        outer.push_number(1.0);
        let mut inner = outer.frame();
        inner.push_number(2.0);
        inner.push_number(3.0);
        inner.pop(-5);
        assert_eq!(inner.len(), 2);
        inner.pop(50);
        assert_eq!(inner.len(), 0);
        assert_eq!(outer.len(), 1, "popping through the inner frame never reaches the outer value");
    });
}

#[test]
fn preserve_top_keeps_only_the_top_value() {
    with_stack(|stack| {
        let base = stack.top();
        let mut frame = stack.frame();
        frame.push_number(1.0);
        frame.push_number(2.0);
        frame.push_string("error object");
        frame.preserve_top_and_release();
        drop(frame);
        assert_eq!(stack.top(), base + 1);
        assert_eq!(stack.at(-1).type_of(), Type::String);
    });
}

#[test]
fn with_frame_rebalances_on_success_and_error() {
    with_stack(|stack| {
        let base = stack.top();
        let count = stack
            .with_frame(|frame| {
                frame.push_number(7.0);
                Ok(frame.len())
            })
            .unwrap();
        assert_eq!(count, 1);
        assert_eq!(stack.top(), base);

        let error = stack
            .with_frame(|frame| {
                frame.push_number(7.0);
                Err::<(), _>(Error::runtime("nope"))
            })
            .unwrap_err();
        assert_eq!(error.to_string(), "nope");
        assert_eq!(stack.top(), base);
    });
}

#[test]
fn tables_get_and_set_through_frames() {
    with_stack(|stack| {
        let frame = stack.frame();
        let table = frame.push_table(0, 2).unwrap();
        frame.push_number(5.0);
        table.raw_set(&frame, "five").unwrap();
        frame.push_string("text");
        table.set(&frame, "word").unwrap();
        assert_eq!(frame.len(), 1, "stores consumed their values");

        frame
            .with_frame(|lookups| {
                assert!(table.raw_get(lookups, "five").unwrap().is_number());
                assert!(table.get(lookups, "word").unwrap().is_string());
                assert!(table.get(lookups, "missing").unwrap().is_nil());
                assert_eq!(lookups.len(), 3);
                Ok(())
            })
            .unwrap();
        assert_eq!(frame.len(), 1);

        assert_eq!(table.raw_len(), 0);
        assert!(!table.is_read_only());
        table.set_read_only(true).unwrap();
        assert!(table.is_read_only());

        frame.push_number(1.0);
        let error = table.raw_set(&frame, "frozen").unwrap_err();
        assert_eq!(error.to_string(), "attempt to modify a readonly table", "host-level raises become errors");
        assert_eq!(frame.len(), 1, "the failed store still consumed its operands");

        {
            let empty = frame.frame();
            assert!(table.set(&empty, "nothing").is_err(), "storing with no value on the frame is a logic error");
        }
        let error = frame.push_boolean(false).as_table().unwrap_err();
        assert_eq!(error.to_string(), format!("Lua stack index {}: expected table, got boolean", frame.top()));
    });
}

#[test]
fn registry_pseudo_index_cannot_be_assigned() {
    with_stack(|stack| {
        let frame = stack.frame();
        let registry = frame.at(ffi::LUA_REGISTRYINDEX).as_table().unwrap();
        frame.push_nil();
        assert!(registry.raw_set(&frame, "x").is_err());
    });
}

#[test]
fn call_level_pushes_survive_frames_opened_after_them() {
    with_stack(|stack| {
        let result = stack.push_number(42.0);
        {
            let frame = stack.frame();
            frame.push_string("scratch");
            assert_eq!(result.type_of(), Type::Number);
        }
        assert_eq!(result.type_of(), Type::Number);
        assert_eq!(stack.top(), 1);
    });
}
