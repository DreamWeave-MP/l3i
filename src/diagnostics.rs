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

/// Raises `message` as a Lua error prefixed with the caller's location, the way `luaL_error`
/// does. For hand-written dispatchers that run directly under Luau.
///
/// # Safety
/// `state` is inside a native call and no Rust value that must be dropped is live in the
/// caller's frame.
pub(crate) unsafe fn raise_at_caller(state: *mut ffi::lua_State, message: &str) -> ! {
    unsafe {
        let text = format!("{}{message}", location(state, 1));
        ffi::lua_pushlstring(state, text.as_ptr().cast(), text.len());
        drop(text);
        ffi::lua_error(state)
    }
}

/// `luaT_objtypename`: `__type` from the metatable when present, else the basic type name.
#[cold]
pub fn object_type_name(value: ValueView<'_>) -> String {
    // SAFETY: luaL_typename accepts any acceptable index, including none.
    unsafe { CStr::from_ptr(ffi::luaL_typename(value.state(), value.index())).to_string_lossy().into_owned() }
}

/// The message `luaL_typeerror(L, narg, expected)` would raise for `value`, including the
/// caller location prefix `luaL_error` adds. Argument numbering is the raw stack position;
/// method binders adjust before calling.
#[cold]
pub fn type_error(value: ValueView<'_>, expected: &str) -> Error {
    type_error_at(value, value.index(), expected)
}

/// [`type_error`] with an explicit argument number.
#[cold]
pub fn type_error_at(value: ValueView<'_>, position: c_int, expected: &str) -> Error {
    let state = value.state();
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
        (true, Some(function)) => {
            format!("{location}missing argument #{position} to '{function}' ({expected} expected)")
        }
        (true, None) => format!("{location}missing argument #{position} ({expected} expected)"),
    };
    Error::Runtime(message)
}

/// UTF-8-safe bounded truncation for diagnostic text (`truncateDiagnostic`).
#[cold]
pub fn truncate_diagnostic(text: &str, max_length: usize) -> String {
    if text.len() <= max_length {
        return text.to_owned();
    }
    let mut keep = max_length;
    while keep > 0 && !text.is_char_boundary(keep) {
        keep -= 1;
    }
    format!("{}...", &text[..keep])
}

/// Escapes quotes, backslashes, and control bytes; multibyte UTF-8 passes through
/// (`escapeDiagnostic`).
#[cold]
pub fn escape_diagnostic(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 || c as u32 == 0x7F => out.push_str(&format!("\\x{:02x}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

/// Non-executing description of a value for error messages (`describeLuaValue`): booleans,
/// numbers, and (truncated, escaped) strings by content, everything else as `<type>`.
#[cold]
pub fn describe_value(value: ValueView<'_>, max_length: usize) -> String {
    let state = value.state();
    // SAFETY: every accessor is guarded by the matching type test and the view proved the slot.
    unsafe {
        match value.type_of() {
            Type::Boolean => {
                return if ffi::lua_toboolean(state, value.index()) != 0 { "true" } else { "false" }.to_owned();
            }
            Type::Integer => {
                let mut is_integer = 0;
                let integer = ffi::lua_tointeger64(state, value.index(), &mut is_integer);
                if is_integer != 0 {
                    return integer.to_string();
                }
            }
            Type::Number => {
                let mut is_number = 0;
                let number = ffi::lua_tonumberx(state, value.index(), &mut is_number);
                if is_number != 0 {
                    return format_number(number);
                }
            }
            Type::String => {
                let mut length = 0usize;
                let text = ffi::lua_tolstring(state, value.index(), &mut length);
                if !text.is_null() {
                    let bytes = std::slice::from_raw_parts(text.cast::<u8>(), length);
                    return escape_diagnostic(&truncate_diagnostic(&String::from_utf8_lossy(bytes), max_length));
                }
            }
            _ => {}
        }
        format!("<{}>", value.type_of().name())
    }
}

/// Shortest round-trip text for a double, as `std::to_chars` prints it.
fn format_number(number: f64) -> String {
    if number.is_finite() && number.fract() == 0.0 && number.abs() < 1e15 {
        // to_chars prints whole doubles without a fraction.
        return format!("{}", number as i64);
    }
    format!("{number}")
}
