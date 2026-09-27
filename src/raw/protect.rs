//! Protected execution for host-level code.
//!
//! Outside a Lua call there is no `pcall` above the Rust frames, so a Luau error raised by an
//! API call (a `__index` that errors, a write to a read-only table, `lua_call`) would unwind into
//! host code and abort at the first `catch_unwind` it meets. Host-level scopes therefore run
//! any raising operation through [`protected_call`], which is `lua_pcall` around a C thunk.
//! Native callbacks skip this: they already run under Luau's own protection.

use std::ffi::{CStr, c_int, c_void};
use std::ptr;

use super::ffi;
use super::trampoline::AbortOnPanic;
use crate::error::{Error, Result};

unsafe extern "C-unwind" fn thunk<F: FnOnce(*mut ffi::lua_State) -> c_int>(state: *mut ffi::lua_State) -> c_int {
    let _guard = AbortOnPanic;
    // SAFETY: `protected_call` passed a pointer to an `Option<F>` as argument 1 and keeps that
    // storage alive across the pcall.
    unsafe {
        let slot = ffi::lua_touserdata(state, 1).cast::<Option<F>>();
        let body = (*slot).take().expect("protected thunk runs once");
        ffi::lua_remove(state, 1);
        body(state)
    }
}

/// Moves the top `nargs` values into a protected C call, runs `body` with them at positions
/// `1..=nargs`, and leaves `nresults` results (or all, for `LUA_MULTRET`) where the arguments
/// were. A Luau error becomes `Err` carrying its message, with the stack restored.
///
/// # Safety
/// `state` is a live thread with at least `nargs` values on top and room for two more.
pub(crate) unsafe fn protected_call<F>(state: *mut ffi::lua_State, nargs: c_int, nresults: c_int, body: F) -> Result<()>
where
    F: FnOnce(*mut ffi::lua_State) -> c_int,
{
    let mut slot = Some(body);
    unsafe {
        ffi::lua_pushcfunction(state, thunk::<F>, ptr::null());
        ffi::lua_pushlightuserdata(state, (&mut slot as *mut Option<F>).cast::<c_void>());
        // Both go beneath the arguments: [.. f, ud, arg1 .. argN].
        ffi::lua_insert(state, -(nargs + 2));
        ffi::lua_insert(state, -(nargs + 2));
        let status = ffi::lua_pcall(state, nargs + 1, nresults, 0);
        if status == ffi::LUA_OK {
            return Ok(());
        }
        Err(pop_error(state, status))
    }
}

/// Converts the error object on top of the stack into an `Error`, popping it.
///
/// # Safety
/// `state` is live and an error object (or nothing, for `LUA_ERRMEM`) is on top.
pub(crate) unsafe fn pop_error(state: *mut ffi::lua_State, status: c_int) -> Error {
    unsafe {
        if status == ffi::LUA_ERRMEM && ffi::lua_gettop(state) == 0 {
            return Error::runtime("Lua error: out of memory");
        }
        let mut length = 0usize;
        let text = ffi::lua_tolstring(state, -1, &mut length);
        let message = if text.is_null() {
            CStr::from_ptr(ffi::luaL_typename(state, -1)).to_string_lossy().into_owned()
        } else {
            String::from_utf8_lossy(std::slice::from_raw_parts(text.cast::<u8>(), length)).into_owned()
        };
        ffi::lua_pop(state, 1);
        Error::Runtime(message)
    }
}
