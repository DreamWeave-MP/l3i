//! Script-visible names for native functions and userdata types.
//!
//! Luau borrows the `debugname` pointer passed to `lua_pushcclosurek` for the closure's whole
//! life, so names are interned as Luau strings in a VM-private registry table and the interned
//! pointer is handed out (`components/luau/debugname.cpp`). Interning is permanent for the VM.
//!
//! A debug name is a dot-separated path of identifiers rooted at one of the host's configured
//! roots (`isValidDebugName` in `cfunction.hpp`); OpenMW's roots are `openmw`, `string`,
//! `vector`.

use std::ffi::c_char;

use crate::error::{Error, Result};
use crate::raw::ffi;

fn is_identifier_start(byte: u8) -> bool {
    byte.is_ascii_alphabetic() || byte == b'_'
}

fn is_identifier_byte(byte: u8) -> bool {
    is_identifier_start(byte) || byte.is_ascii_digit()
}

/// True when `name` is `<root>.<ident>(.<ident>)*` for one of `roots`.
pub fn is_valid_debug_name<R: AsRef<str>>(name: &str, roots: &[R]) -> bool {
    let Some((root, rest)) = name.split_once('.') else { return false };
    if !roots.iter().any(|candidate| candidate.as_ref() == root) {
        return false;
    }
    let mut component_start = true;
    for byte in rest.bytes() {
        if byte == b'.' {
            if component_start {
                return false;
            }
            component_start = true;
        } else if component_start {
            if !is_identifier_start(byte) {
                return false;
            }
            component_start = false;
        } else if !is_identifier_byte(byte) {
            return false;
        }
    }
    !component_start
}

pub fn require_valid_debug_name<R: AsRef<str>>(name: &str, roots: &[R]) -> Result<()> {
    if is_valid_debug_name(name, roots) {
        return Ok(());
    }
    let roots: Vec<&str> = roots.iter().map(AsRef::as_ref).collect();
    Err(Error::logic(format!("Lua debug name '{name}' must be dot-separated identifiers rooted at one of {roots:?}")))
}

static DEBUG_NAME_REGISTRY_KEY: u8 = 0;

/// Interns `name` in the VM and returns a pointer valid until `lua_close`.
///
/// Validation against `roots` happens first; the caller normally composes the complete path.
///
/// # Safety
/// `state` is a live thread of the VM with `LUA_MINSTACK` free slots.
pub(crate) unsafe fn retain<R: AsRef<str>>(
    state: *mut ffi::lua_State,
    name: &str,
    roots: &[R],
) -> Result<*const c_char> {
    if name.is_empty() {
        return Err(Error::logic("Lua function debug name cannot be empty"));
    }
    require_valid_debug_name(name, roots)?;
    let key = (&DEBUG_NAME_REGISTRY_KEY as *const u8).cast_mut().cast();

    // SAFETY: all indexes below are relative to values this function pushes; the frame is
    // rebalanced before returning on every path.
    unsafe {
        let top = ffi::lua_gettop(state);
        if ffi::lua_rawgetp(state, ffi::LUA_REGISTRYINDEX, key) != ffi::LUA_TTABLE {
            ffi::lua_pop(state, 1);
            ffi::lua_newtable(state);
            ffi::lua_pushvalue(state, -1);
            ffi::lua_rawsetp(state, ffi::LUA_REGISTRYINDEX, key);
        }
        let names = ffi::lua_gettop(state);

        ffi::lua_pushlstring(state, name.as_ptr().cast(), name.len());
        ffi::lua_rawget(state, names);
        if ffi::lua_type(state, -1) == ffi::LUA_TSTRING {
            let retained = ffi::lua_tolstring(state, -1, std::ptr::null_mut());
            ffi::lua_settop(state, top);
            return Ok(retained);
        }
        ffi::lua_pop(state, 1);

        // The interned string is both key and value, so the table roots it for the VM's life.
        ffi::lua_pushlstring(state, name.as_ptr().cast(), name.len());
        let retained = ffi::lua_tolstring(state, -1, std::ptr::null_mut());
        ffi::lua_pushvalue(state, -1);
        ffi::lua_rawset(state, names);
        ffi::lua_settop(state, top);
        Ok(retained)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ROOTS: &[&str] = &["openmw", "string", "vector"];

    #[test]
    fn grammar_matches_the_cpp_static_asserts() {
        for ok in [
            "openmw.core.GObject.get.position",
            "openmw.util.TransformM.__mul",
            "openmw.types.Actor._startAiCombat",
            "vector.__namecall",
            "string.format",
        ] {
            assert!(is_valid_debug_name(ok, ROOTS), "{ok}");
        }
        for bad in [
            "util.vector2",
            "openmw",
            "openmw.",
            "openmw..core",
            "openmw.core.",
            "openmw.2d",
            "openmw.core-thing",
            "TestFoo",
            "",
        ] {
            assert!(!is_valid_debug_name(bad, ROOTS), "{bad}");
        }
        assert!(is_valid_debug_name("dreamweave.archive.open", &["dreamweave"]));
        assert!(!is_valid_debug_name("openmw.archive.open", &["dreamweave"]));
    }

    #[test]
    fn retained_names_are_stable_and_deduplicated() {
        let runtime = crate::runtime::Runtime::new().unwrap();
        let state = runtime.state();
        let first = unsafe { retain(state, "openmw.test.one", ROOTS) }.unwrap();
        let again = unsafe { retain(state, "openmw.test.one", ROOTS) }.unwrap();
        let other = unsafe { retain(state, "openmw.test.two", ROOTS) }.unwrap();
        assert_eq!(first, again);
        assert_ne!(first, other);
        assert_eq!(unsafe { std::ffi::CStr::from_ptr(first) }.to_str().unwrap(), "openmw.test.one");
        assert_eq!(runtime.stack().top(), 0);
        // A full collection must not move or free the interned string.
        unsafe { ffi::lua_gc(state, ffi::LUA_GCCOLLECT, 0) };
        assert_eq!(unsafe { std::ffi::CStr::from_ptr(first) }.to_str().unwrap(), "openmw.test.one");
        assert!(unsafe { retain(state, "bogus", ROOTS) }.is_err());
    }
}
