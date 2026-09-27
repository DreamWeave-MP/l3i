use super::*;

/// Runs `body` on the raw state of a fresh VM, inside mlua's protected closure.
fn with_stack(body: impl FnOnce(&Stack<'_>)) {
    let lua = mlua::Lua::new();
    unsafe {
        lua.exec_raw::<()>((), |state| {
            let stack = Stack::from_raw(state);
            body(&stack);
        })
    }
    .unwrap();
}

#[test]
fn negative_indexes_resolve_against_the_current_top() {
    with_stack(|stack| {
        let base = stack.top();
        stack.push_number(1.0);
        stack.push_number(2.0);
        let second = stack.at(-1);
        assert_eq!(second.index(), base + 2);
        stack.push_number(3.0);
        // The view still names slot base+2 after another push.
        assert_eq!(second.type_of(), Type::Number);
        assert_eq!(stack.at(0).type_of(), Type::None);
        assert_eq!(stack.at(-100).type_of(), Type::None);
        // Above the top but inside the frame's allocated slots: None. Further up is not an
        // acceptable index and Luau's api_check rejects it, so the binder never asks.
        assert_eq!(stack.at(base + 4).type_of(), Type::None);
    });
}

#[test]
fn frames_restore_the_entry_height() {
    with_stack(|stack| {
        let base = stack.top();
        {
            let frame = stack.frame();
            frame.push_nil();
            frame.push_boolean(true);
            assert_eq!(frame.top(), base + 2);
        }
        assert_eq!(stack.top(), base);

        let mut frame = stack.frame();
        frame.push_nil();
        frame.release();
        drop(frame);
        assert_eq!(stack.top(), base + 1);
        stack.pop(1);
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
        stack.pop(1);
    });
}

#[test]
fn with_frame_rebalances_on_success_and_error() {
    with_stack(|stack| {
        let base = stack.top();
        let value = stack
            .with_frame(|stack| {
                stack.push_number(7.0);
                Ok(stack.top() - base)
            })
            .unwrap();
        assert_eq!(value, 1);
        assert_eq!(stack.top(), base);

        let error = stack
            .with_frame(|stack| {
                stack.push_number(7.0);
                Err::<(), _>(Error::runtime("nope"))
            })
            .unwrap_err();
        assert_eq!(error.to_string(), "nope");
        assert_eq!(stack.top(), base);
    });
}

#[test]
fn tables_get_and_set_through_borrowed_views() {
    with_stack(|stack| {
        let table = stack.push_table(0, 2).unwrap();
        stack.push_number(5.0);
        table.raw_set_from_top("five").unwrap();
        stack.push_string("text");
        table.set_from_top("word").unwrap();

        stack
            .with_frame(|_| {
                assert!(table.raw_get("five").is_number());
                assert!(table.get("word").is_string());
                assert!(table.get("missing").is_nil());
                Ok(())
            })
            .unwrap();
        assert_eq!(table.raw_len(), 0);
        assert!(!table.is_read_only());
        table.set_read_only(true);
        assert!(table.is_read_only());

        assert!(stack.at(1).as_table().is_ok());
        let error = stack.push_boolean(false).as_table().unwrap_err();
        assert_eq!(error.to_string(), "table expected, got boolean");
    });
}

#[test]
fn registry_pseudo_index_cannot_be_assigned() {
    with_stack(|stack| {
        let registry = stack.at(ffi::LUA_REGISTRYINDEX).as_table().unwrap();
        stack.push_nil();
        assert!(registry.raw_set_from_top("x").is_err());
        stack.pop(1);
    });
}
