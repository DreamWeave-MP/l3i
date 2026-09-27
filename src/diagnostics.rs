//! Error text that matches Luau's own `lauxlib` wording, so scripts and tests see the same
//! messages the C++ binder produced through `luaL_typeerror` / `luaL_argerror`.

use std::ffi::{CStr, c_int};
use std::ptr;

use crate::error::Error;
use crate::raw::ffi;
use crate::stack::{Type, ValueView};

/// `luaL_where(L, level)`: `source:line: ` for a Lua frame, empty otherwise.
///
/// # Safety
/// `state` is a live thread.
unsafe fn location(state: *mut ffi::lua_State, level: c_int) -> String {
    let mut ar = std::mem::MaybeUninit::<ffi::lua_Debug>::zeroed();
    // SAFETY: lua_getinfo fills the fields named by "sl"; short_src points into ar.ssbuf.
    unsafe {
        if ffi::lua_getinfo(state, level, c"sl".as_ptr(), ar.as_mut_ptr()) != 0 {
            let ar = ar.assume_init_ref();
            if ar.currentline > 0 && !ar.short_src.is_null() {
                return format!("{}:{}: ", CStr::from_ptr(ar.short_src).to_string_lossy(), ar.currentline);
            }
        }
    }
    String::new()
}

/// `currfuncname` from laux.cpp: the running C closure's debug name, or for a `__namecall`
/// dispatcher the method name being called.
///
/// # Safety
/// `state` is a live thread currently executing a C function.
unsafe fn current_function_name(state: *mut ffi::lua_State) -> Option<String> {
    let mut ar = std::mem::MaybeUninit::<ffi::lua_Debug>::zeroed();
    unsafe {
        if ffi::lua_getinfo(state, 0, c"n".as_ptr(), ar.as_mut_ptr()) == 0 {
            return None;
        }
        let name = ar.assume_init_ref().name;
        if name.is_null() {
            return None;
        }
        let name = CStr::from_ptr(name);
        if name.to_bytes() == b"__namecall" {
            let method = ffi::lua_namecallatom(state, ptr::null_mut());
            return (!method.is_null()).then(|| CStr::from_ptr(method).to_string_lossy().into_owned());
        }
        Some(name.to_string_lossy().into_owned())
    }
}

/// `luaT_objtypename`: `__type` from the metatable when present, else the basic type name.
pub fn object_type_name(value: ValueView<'_>) -> String {
    // SAFETY: luaL_typename accepts any acceptable index, including none.
    unsafe { CStr::from_ptr(ffi::luaL_typename(value.stack().state(), value.index())).to_string_lossy().into_owned() }
}

/// The message `luaL_typeerror(L, narg, expected)` would raise for `value`, including the
/// caller location prefix `luaL_error` adds. Argument numbering is the raw stack position;
/// method binders adjust before calling.
pub fn type_error(value: ValueView<'_>, expected: &str) -> Error {
    type_error_at(value, value.index(), expected)
}

/// [`type_error`] with an explicit argument number.
pub fn type_error_at(value: ValueView<'_>, position: c_int, expected: &str) -> Error {
    let state = value.stack().state();
    // SAFETY: the view proves the state is live and we are inside a native call.
    let (location, function) = unsafe { (location(state, 1), current_function_name(state)) };
    let message = match (value.type_of() == Type::None, function) {
        (false, Some(function)) => format!(
            "{location}invalid argument #{position} to '{function}' ({expected} expected, got {})",
            object_type_name(value)
        ),
        (false, None) => {
            format!("{location}invalid argument #{position} ({expected} expected, got {})", object_type_name(value))
        }
        (true, Some(function)) => format!("{location}missing argument #{position} to '{function}' ({expected} expected)"),
        (true, None) => format!("{location}missing argument #{position} ({expected} expected)"),
    };
    Error::Runtime(message)
}
