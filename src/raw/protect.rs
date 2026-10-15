//! Protected execution for host-level code.
//!
//! Outside a Lua call there is no `pcall` above the Rust frames, so a Luau error raised by an
//! API call (a `__index` that errors, a write to a read-only table, `lua_call`, an allocation
//! that runs out of memory) would unwind into host code and abort at the first `catch_unwind` it
//! meets. Host-level scopes therefore run any raising operation through [`protected_call`],
//! which is `lua_pcall` around a C thunk. Native callbacks skip this: they already run under
//! Luau's own protection.
//!
//! The protection itself must not raise. The thunk is one C closure per VM, made under
//! `lua_cpcall` when the runtime is built and kept in the registry, so a protected call pushes
//! an existing value and a light userdata (neither allocates) after checking the stack has room.

use std::ffi::{CStr, c_int, c_void};
use std::ptr;

use super::ffi;
use super::trampoline::AbortOnPanic;
use crate::error::{Error, Result};

/// A protected body with its type erased, so one thunk serves every closure type.
struct Erased {
    run: unsafe fn(*mut c_void, *mut ffi::lua_State) -> c_int,
    body: *mut c_void,
}

/// # Safety
/// `body` points at the `Option<F>` [`protected_call`] keeps alive across the pcall.
unsafe fn run_body<F: FnOnce(*mut ffi::lua_State) -> c_int>(body: *mut c_void, state: *mut ffi::lua_State) -> c_int {
    // SAFETY: forwarded contract.
    let body = unsafe { (*body.cast::<Option<F>>()).take() }.expect("protected thunk runs once");
    body(state)
}

unsafe extern "C-unwind" fn thunk(state: *mut ffi::lua_State) -> c_int {
    let _guard = AbortOnPanic::new();
    // SAFETY: `protected_call` passed a pointer to an `Erased` as argument 1 and keeps it and
    // the body it erases alive across the pcall.
    unsafe {
        let erased = ffi::lua_touserdata(state, 1).cast::<Erased>();
        ffi::lua_remove(state, 1);
        ((*erased).run)((*erased).body, state)
    }
}

unsafe extern "C-unwind" fn install(state: *mut ffi::lua_State) -> c_int {
    // SAFETY: `install_thunk` passes a pointer to its result slot as the cpcall userdata.
    unsafe {
        let slot = ffi::lua_touserdata(state, 1).cast::<c_int>();
        ffi::lua_pushcfunction(state, thunk, c"l3i.protected".as_ptr());
        *slot = ffi::lua_ref(state, -1);
        ffi::lua_pop(state, 1);
    }
    0
}

/// Makes the VM's protected-call thunk under `lua_cpcall` and returns its registry reference.
///
/// # Safety
/// `state` is the live main thread of a fresh VM.
pub(crate) unsafe fn install_thunk(state: *mut ffi::lua_State) -> Result<c_int> {
    let mut reference = ffi::LUA_NOREF;
    // SAFETY: forwarded contract; `install` writes the slot before it returns.
    let status = unsafe { ffi::lua_cpcall(state, install, (&raw mut reference).cast()) };
    if status != ffi::LUA_OK {
        // SAFETY: a failed cpcall leaves its error object on top.
        unsafe { ffi::lua_pop(state, 1) };
        return Err(Error::runtime("Unable to allocate a Luau state"));
    }
    Ok(reference)
}

/// Moves the top `nargs` values into a protected C call, runs `body` with them at positions
/// `1..=nargs`, and leaves `nresults` results (or all, for `LUA_MULTRET`) where the arguments
/// were. A Luau error becomes `Err` carrying its message, with the stack restored, and so does a
/// stack with no room for the call.
///
/// # Safety
/// `state` is a live thread with at least `nargs` values on top.
pub(crate) unsafe fn protected_call<F>(state: *mut ffi::lua_State, nargs: c_int, nresults: c_int, body: F) -> Result<()>
where
    F: FnOnce(*mut ffi::lua_State) -> c_int,
{
    let mut slot = Some(body);
    let mut erased = Erased { run: run_body::<F>, body: (&raw mut slot).cast() };
    unsafe {
        if ffi::lua_checkstack(state, 2 + nresults.max(0)) == 0 {
            return Err(Error::runtime("Lua error: stack overflow"));
        }
        let reference = crate::runtime::shared::shared_of(state)
            .as_ref()
            .map_or(ffi::LUA_NOREF, crate::runtime::shared::Shared::protected_thunk);
        if reference > 0 {
            ffi::lua_getref(state, reference);
        } else {
            ffi::lua_pushcfunction(state, thunk, ptr::null());
        }
        ffi::lua_pushlightuserdata(state, (&raw mut erased).cast::<c_void>());
        // Both go beneath the arguments: [.. f, ud, arg1 .. argN].
        ffi::lua_insert(state, -(nargs + 2));
        ffi::lua_insert(state, -(nargs + 2));
        crate::runtime::shared::host_transition(state, || {
            let status = ffi::lua_pcall(state, nargs + 1, nresults, 0);
            if status == ffi::LUA_OK {
                return Ok(());
            }
            Err(crate::runtime::shared::classify_failure(state, pop_error(state, status)))
        })
    }
}

/// Converts the error object on top of the stack into an `Error`, popping it. Never raises: a
/// number is formatted here rather than converted in place, which would allocate.
///
/// # Safety
/// `state` is live and an error object (or nothing, for `LUA_ERRMEM`) is on top.
pub(crate) unsafe fn pop_error(state: *mut ffi::lua_State, status: c_int) -> Error {
    unsafe {
        if status == ffi::LUA_ERRMEM && ffi::lua_gettop(state) == 0 {
            return Error::runtime("Lua error: out of memory");
        }
        let message = match ffi::lua_type(state, -1) {
            ffi::LUA_TSTRING => {
                let mut length = 0usize;
                let text = ffi::lua_tolstring(state, -1, &mut length);
                String::from_utf8_lossy(std::slice::from_raw_parts(text.cast::<u8>(), length)).into_owned()
            }
            ffi::LUA_TNUMBER => crate::diagnostics::format_number(ffi::lua_tonumber(state, -1)),
            _ => CStr::from_ptr(ffi::luaL_typename(state, -1)).to_string_lossy().into_owned(),
        };
        ffi::lua_pop(state, 1);
        Error::Runtime(message)
    }
}
