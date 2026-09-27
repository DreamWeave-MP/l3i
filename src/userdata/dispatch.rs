//! Generated member dispatchers (`binding.cpp`: `indexDispatcherThunk`,
//! `namecallDispatcherThunk`, `newindexDispatcherThunk`).
//!
//! These are hand-written C functions on the hot path: they raise directly and let Luau's
//! exception unwind through them, exactly as the C++ thunks did. Messages use `luaL_error`
//! wording with the caller's location prefix.

use std::ffi::{CStr, c_int};

use crate::diagnostics;
use crate::raw::ffi;

/// `__index` for metatables with getters: upvalue 1 is the methods table, upvalue 2 the
/// getters table. Methods win, then a getter is called with the receiver, else nil.
pub(crate) unsafe extern "C-unwind" fn index(state: *mut ffi::lua_State) -> c_int {
    unsafe {
        ffi::lua_pushvalue(state, ffi::lua_upvalueindex(1));
        ffi::lua_pushvalue(state, 2);
        ffi::lua_rawget(state, -2);
        ffi::lua_remove(state, -2);
        if !ffi::lua_isnil(state, -1) {
            return 1;
        }
        ffi::lua_pop(state, 1);

        ffi::lua_pushvalue(state, ffi::lua_upvalueindex(2));
        ffi::lua_pushvalue(state, 2);
        ffi::lua_rawget(state, -2);
        ffi::lua_remove(state, -2);
        if ffi::lua_isfunction(state, -1) {
            ffi::lua_pushvalue(state, 1);
            let _lua_call = crate::runtime::shared::LuaCall::enter(state);
            ffi::lua_call(state, 1, 1);
            return 1;
        }
        ffi::lua_pop(state, 1);
        ffi::lua_pushnil(state);
        1
    }
}

/// `__namecall`: upvalue 1 is the methods table, upvalue 2 the type name, upvalue 3 the
/// atom-keyed methods table. Names with a Luau atom resolve without re-hashing.
pub(crate) unsafe extern "C-unwind" fn namecall(state: *mut ffi::lua_State) -> c_int {
    unsafe {
        let mut atom: c_int = -1;
        let name = ffi::lua_namecallatom(state, &mut atom);
        if name.is_null() {
            diagnostics::raise_at_caller(state, "attempt to call a non-callable object");
        }
        let top = ffi::lua_gettop(state);
        if atom >= 0 {
            ffi::lua_rawgeti(state, ffi::lua_upvalueindex(3), atom);
        } else {
            ffi::lua_pushvalue(state, ffi::lua_upvalueindex(1));
            ffi::lua_pushstring(state, name);
            ffi::lua_rawget(state, -2);
            ffi::lua_remove(state, -2);
        }
        if !ffi::lua_isfunction(state, -1) {
            let method = CStr::from_ptr(name).to_string_lossy().into_owned();
            ffi::lua_pop(state, 1);
            diagnostics::raise_at_caller(state, &format!("attempt to call method '{method}'"));
        }
        ffi::lua_insert(state, 1);
        let _lua_call = crate::runtime::shared::LuaCall::enter(state);
        ffi::lua_call(state, top, ffi::LUA_MULTRET);
        ffi::lua_gettop(state)
    }
}

/// `__newindex`: upvalue 1 is the setters table, upvalue 2 the type name. Misses are
/// read-only errors.
pub(crate) unsafe extern "C-unwind" fn newindex(state: *mut ffi::lua_State) -> c_int {
    unsafe {
        ffi::lua_pushvalue(state, ffi::lua_upvalueindex(1));
        ffi::lua_pushvalue(state, 2);
        ffi::lua_rawget(state, -2);
        ffi::lua_remove(state, -2);
        if ffi::lua_isfunction(state, -1) {
            ffi::lua_pushvalue(state, 1);
            ffi::lua_pushvalue(state, 3);
            let _lua_call = crate::runtime::shared::LuaCall::enter(state);
            ffi::lua_call(state, 2, 0);
            return 0;
        }
        ffi::lua_pop(state, 1);
        let type_name =
            CStr::from_ptr(ffi::lua_tostring(state, ffi::lua_upvalueindex(2))).to_string_lossy().into_owned();
        let message = if ffi::lua_type(state, 2) == ffi::LUA_TSTRING {
            let field = CStr::from_ptr(ffi::lua_tostring(state, 2)).to_string_lossy().into_owned();
            format!("{type_name} field '{field}' is read-only")
        } else {
            let kind = CStr::from_ptr(ffi::lua_typename(state, ffi::lua_type(state, 2))).to_string_lossy();
            format!("{type_name}: cannot assign to {kind} key")
        };
        diagnostics::raise_at_caller(state, &message)
    }
}
