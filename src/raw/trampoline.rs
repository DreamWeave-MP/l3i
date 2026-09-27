//! The boundary between Rust and Luau's C ABI.
//!
//! Luau is built with C++ exceptions, so `lua_error` is a foreign unwind. Two facts shape
//! everything here:
//!
//! 1. A foreign exception may pass through Rust frames declared `extern "C-unwind"`, and their
//!    destructors run. It may **not** reach a `catch_unwind`: Rust aborts the process. So this
//!    module never uses `catch_unwind`, and a Luau error raised inside a native function
//!    unwinds straight through the binding's Rust frames to the enclosing Lua `pcall`, exactly
//!    as C++ exceptions did in the C++ binder.
//! 2. A Rust panic must never travel the other way into Luau frames. A panic inside a binding
//!    is a bug with no recovery path that leaves the VM consistent, so [`enter`] turns it into
//!    an immediate abort with the panic message, before any Luau frame sees it.
//!
//! Host code (outside any Lua call) must never be reached by a Luau error either; see
//! [`super::protect`].

use std::ffi::c_int;

use super::ffi;
use crate::error::{Error, Result};

/// Aborts if dropped while a Rust panic is unwinding. Foreign (Luau) unwinds drop it
/// normally: `std::thread::panicking()` is false for them.
pub(crate) struct AbortOnPanic;

impl Drop for AbortOnPanic {
    fn drop(&mut self) {
        if std::thread::panicking() {
            eprintln!("dream-binder: a Rust panic inside a Luau native function cannot be recovered; aborting");
            std::process::abort();
        }
    }
}

/// Runs `body` as the entire implementation of a `lua_CFunction`.
///
/// Returns the result count on success. On `Err` the error is raised into Luau after `body`
/// and everything it owned have been dropped. `Error::LuaErrorOnStack` re-raises the value
/// currently on top of the stack instead of pushing a new message. A panic aborts.
///
/// # Safety
/// `state` must be the live `lua_State*` Luau handed to the enclosing C function, and the
/// caller must be that C function's frame, so that raising here unwinds only Luau frames and
/// `extern "C-unwind"` Rust frames.
pub(crate) unsafe fn enter(state: *mut ffi::lua_State, body: impl FnOnce() -> Result<c_int>) -> c_int {
    let _guard = AbortOnPanic;
    match body() {
        Ok(count) => count,
        Err(error) => unsafe { raise(state, error) },
    }
}

/// # Safety
/// Same as `enter`; additionally no Rust value that must be dropped may be live in the caller.
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
                    3 => {
                        ffi::lua_pushlstring(state, c"custom object".as_ptr(), 13);
                        Err(Error::LuaErrorOnStack)
                    }
                    4 => {
                        // A Luau error raised by the API mid-body: unwinds through this Rust
                        // frame (dropping `_guard`) to the pcall in the test.
                        ffi::lua_pushlstring(state, c"raised by luau".as_ptr(), 14);
                        ffi::lua_error(state)
                    }
                    5 => {
                        // Same, but from a metamethod invoked by lua_gettable on argument 2.
                        ffi::lua_pushlstring(state, c"key".as_ptr(), 3);
                        ffi::lua_gettable(state, 2);
                        Ok(1)
                    }
                    other => Err(Error::logic(format!("unexpected probe {other}"))),
                }
            })
        }
    }

    fn run(mode: i32) -> (bool, String) {
        let runtime = crate::runtime::Runtime::new().unwrap();
        let stack = runtime.stack();
        let frame = stack.frame();
        let state = runtime.state();
        // Argument 2 is a table whose __index errors. Everything here stays inside `frame`;
        // the only raising call is the pcall itself.
        unsafe {
            frame.push_c_function(probe, ptr::null());
            frame.push_number(f64::from(mode));
            frame.push_table(0, 0).unwrap();
            frame.push_table(0, 1).unwrap();
            runtime
                .load(&frame, "=index", "return function() error('from __index', 0) end", &Default::default())
                .unwrap();
            // The chunk only returns a closure; it cannot raise.
            ffi::lua_call(state, 0, 1);
            ffi::lua_setfield(state, -2, c"__index".as_ptr());
            ffi::lua_setmetatable(state, -2);
            let ok = ffi::lua_pcall(state, 2, 1, 0) == ffi::LUA_OK;
            let text = if ffi::lua_type(state, -1) == ffi::LUA_TNUMBER {
                ffi::lua_tonumber(state, -1).to_string()
            } else {
                let mut length = 0usize;
                let bytes = ffi::lua_tolstring(state, -1, &mut length);
                String::from_utf8_lossy(std::slice::from_raw_parts(bytes.cast::<u8>(), length)).into_owned()
            };
            (ok, text)
        }
    }

    #[test]
    fn success_returns_results() {
        assert_eq!(run(1), (true, "42".to_owned()));
        assert_eq!(drops(), 1);
    }

    #[test]
    fn runtime_errors_become_lua_errors_after_rust_drops() {
        assert_eq!(run(2), (false, "boom".to_owned()));
        assert_eq!(drops(), 1);
    }

    #[test]
    fn error_object_on_stack_is_reraised_unchanged() {
        assert_eq!(run(3), (false, "custom object".to_owned()));
        assert_eq!(drops(), 1);
    }

    #[test]
    fn luau_errors_unwind_through_rust_frames_and_run_destructors() {
        assert_eq!(run(4), (false, "raised by luau".to_owned()));
        assert_eq!(drops(), 1, "the Rust guard was dropped by the foreign unwind");
    }

    #[test]
    fn metamethod_errors_inside_api_calls_behave_the_same() {
        assert_eq!(run(5), (false, "from __index".to_owned()));
        assert_eq!(drops(), 1);
    }
}
