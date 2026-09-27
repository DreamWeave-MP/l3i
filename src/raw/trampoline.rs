//! The single boundary between Rust and Luau's C ABI.
//!
//! Luau is built with C++ exceptions, so a `lua_error` unwinds as a foreign
//! exception. It must never pass through a Rust frame that still owns values. `enter` runs the
//! whole body first, lets every Rust value drop, and only then raises. Panics are contained
//! here as well; nothing Rust ever unwinds into Luau.

use std::any::Any;
use std::ffi::c_int;
use std::panic::{AssertUnwindSafe, catch_unwind};

use super::ffi;
use crate::error::{Error, Result};

/// Runs `body` as the entire implementation of a `lua_CFunction`.
///
/// Returns the result count on success. On `Err` or panic the error is raised into Luau after
/// `body` and everything it owned have been dropped. `Error::LuaErrorOnStack` re-raises the
/// value currently on top of the stack instead of pushing a new message.
///
/// # Safety
/// `state` must be the live `lua_State*` Luau handed to the enclosing C function, and the
/// caller must be that C function's frame, so that raising here unwinds only Luau frames.
#[allow(dead_code)] // the tagged userdata slice adds the first C entry points
pub(crate) unsafe fn enter(state: *mut ffi::lua_State, body: impl FnOnce() -> Result<c_int>) -> c_int {
    match catch_unwind(AssertUnwindSafe(body)) {
        Ok(Ok(count)) => count,
        Ok(Err(error)) => unsafe { raise(state, error) },
        Err(payload) => unsafe { raise(state, Error::Runtime(panic_message(payload.as_ref()))) },
    }
}

/// # Safety
/// Same as `enter`; additionally no Rust value with a destructor may be live in the caller.
#[allow(dead_code)] // the typed binder raises directly from its own entry points
pub(crate) unsafe fn raise(state: *mut ffi::lua_State, error: Error) -> ! {
    unsafe {
        match error {
            Error::LuaErrorOnStack => ffi::lua_error(state),
            other => {
                let message = other.to_string();
                ffi::lua_pushlstring(state, message.as_ptr().cast(), message.len());
                drop(message);
                ffi::lua_error(state)
            }
        }
    }
}

fn panic_message(payload: &(dyn Any + Send)) -> String {
    if let Some(text) = payload.downcast_ref::<&str>() {
        format!("Rust panic in Lua binding: {text}")
    } else if let Some(text) = payload.downcast_ref::<String>() {
        format!("Rust panic in Lua binding: {text}")
    } else {
        String::from("Rust panic in Lua binding")
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::ffi::c_int;
    use std::ptr;

    use super::*;

    // Tests run on their own threads; a thread-local keeps the counts independent.
    thread_local!(static DROPS: Cell<usize> = const { Cell::new(0) });

    fn drops() -> usize {
        DROPS.with(Cell::get)
    }

    struct DropCounter;

    impl Drop for DropCounter {
        fn drop(&mut self) {
            DROPS.with(|drops| drops.set(drops.get() + 1));
        }
    }

    unsafe extern "C-unwind" fn probe(state: *mut ffi::lua_State) -> c_int {
        unsafe {
            enter(state, || {
                let _guard = DropCounter;
                match ffi::lua_tonumber(state, 1) as i32 {
                    1 => {
                        ffi::lua_pushnumber(state, 42.0);
                        Ok(1)
                    }
                    2 => Err(Error::runtime("boom")),
                    3 => panic!("kaboom"),
                    4 => {
                        ffi::lua_pushlstring(state, c"custom object".as_ptr(), 13);
                        Err(Error::LuaErrorOnStack)
                    }
                    other => Err(Error::logic(format!("unexpected probe {other}"))),
                }
            })
        }
    }

    fn run(mode: i32) -> (bool, String) {
        let runtime = crate::runtime::Runtime::new().unwrap();
        let state = runtime.state();
        // SAFETY: fresh live state on this thread; the stack is balanced by the frame.
        unsafe {
            ffi::lua_pushcfunction(state, probe, ptr::null());
            ffi::lua_pushnumber(state, f64::from(mode));
            let ok = ffi::lua_pcall(state, 1, 1, 0) == ffi::LUA_OK;
            let text = if ffi::lua_type(state, -1) == ffi::LUA_TNUMBER {
                ffi::lua_tonumber(state, -1).to_string()
            } else {
                let mut length = 0usize;
                let bytes = ffi::lua_tolstring(state, -1, &mut length);
                String::from_utf8_lossy(std::slice::from_raw_parts(bytes.cast::<u8>(), length)).into_owned()
            };
            ffi::lua_pop(state, 1);
            (ok, text)
        }
    }

    #[test]
    fn success_returns_results() {
        assert_eq!(run(1), (true, "42".to_owned()));
    }

    #[test]
    fn runtime_errors_become_lua_errors_after_rust_drops() {
        assert_eq!(run(2), (false, "boom".to_owned()));
        assert_eq!(drops(), 1);
    }

    #[test]
    fn panics_are_contained() {
        assert_eq!(run(3), (false, "Rust panic in Lua binding: kaboom".to_owned()));
        assert_eq!(drops(), 1);
    }

    #[test]
    fn error_object_on_stack_is_reraised_unchanged() {
        assert_eq!(run(4), (false, "custom object".to_owned()));
    }
}
