//! Generated member dispatchers (`binding.cpp`: `indexDispatcherThunk`,
//! `namecallDispatcherThunk`, `newindexDispatcherThunk`).
//!
//! These are hand-written C functions on the hot path: they raise directly and let Luau's
//! exception unwind through them, exactly as the C++ thunks did. Messages use `luaL_error`
//! wording with the caller's location prefix.
//!
//! Bound members are stored in the dispatch tables as [`MemberEntry`] userdata and run
//! directly on the dispatcher's stack; a plain function (an unbound method) is `lua_call`ed.

use std::ffi::{CStr, c_int};

use crate::bind::MemberEntry;
use crate::diagnostics;
use crate::raw::ffi;

/// The entry stored in the userdata on top of the stack, popped.
///
/// # Safety
/// The top value is a `MemberEntry` userdata written by the metatable builder.
#[inline(always)]
unsafe fn pop_entry(state: *mut ffi::lua_State) -> MemberEntry {
    unsafe {
        let entry = *ffi::lua_touserdata(state, -1).cast::<MemberEntry>();
        ffi::lua_pop(state, 1);
        entry
    }
}

/// `__index` for metatables with getters: upvalue 1 maps member names to method functions and
/// getter entries (upvalue 2 keeps the getter closures alive). A method is returned; a getter
/// runs directly with the receiver as its only argument; anything else reads as nil.
pub(crate) unsafe extern "C-unwind" fn index(state: *mut ffi::lua_State) -> c_int {
    unsafe {
        ffi::lua_pushvalue(state, 2);
        if ffi::lua_rawget(state, ffi::lua_upvalueindex(1)) == ffi::LUA_TUSERDATA {
            // Stack: receiver, key. The getter sees one argument; its result lands above.
            return pop_entry(state).call(state, 1);
        }
        // A method function, or nil for a miss: either is the answer.
        1
    }
}

/// `__namecall`: upvalue 1 maps method names to entries or functions, upvalue 2 is the type
/// name, upvalue 3 maps Luau atoms to the same. Names with an atom resolve without re-hashing.
pub(crate) unsafe extern "C-unwind" fn namecall(state: *mut ffi::lua_State) -> c_int {
    unsafe {
        let mut atom: c_int = -1;
        let name = ffi::lua_namecallatom(state, &mut atom);
        if name.is_null() {
            diagnostics::raise_at_caller(state, "attempt to call a non-callable object");
        }
        let top = ffi::lua_gettop(state);
        let kind = if atom >= 0 {
            ffi::lua_rawgeti(state, ffi::lua_upvalueindex(3), atom)
        } else {
            ffi::lua_pushstring(state, name);
            ffi::lua_rawget(state, ffi::lua_upvalueindex(1))
        };
        match kind {
            ffi::LUA_TUSERDATA => pop_entry(state).call(state, top),
            ffi::LUA_TFUNCTION => {
                ffi::lua_insert(state, 1);
                let _lua_call = crate::runtime::shared::LuaCall::enter(state);
                ffi::lua_call(state, top, ffi::LUA_MULTRET);
                ffi::lua_gettop(state)
            }
            _ => {
                let method = CStr::from_ptr(name).to_string_lossy().into_owned();
                ffi::lua_pop(state, 1);
                diagnostics::raise_at_caller(state, &format!("attempt to call method '{method}'"))
            }
        }
    }
}

/// `__newindex`: upvalue 1 maps property names to setter entries, upvalue 2 is the type name
/// (upvalue 3 keeps the setter closures alive). Misses are read-only errors.
pub(crate) unsafe extern "C-unwind" fn newindex(state: *mut ffi::lua_State) -> c_int {
    unsafe {
        ffi::lua_pushvalue(state, 2);
        match ffi::lua_rawget(state, ffi::lua_upvalueindex(1)) {
            ffi::LUA_TUSERDATA => {
                let entry = pop_entry(state);
                // Stack: receiver, key, value. The setter takes the receiver and the value.
                ffi::lua_remove(state, 2);
                entry.call(state, 2);
                return 0;
            }
            ffi::LUA_TFUNCTION => {
                ffi::lua_pushvalue(state, 1);
                ffi::lua_pushvalue(state, 3);
                let _lua_call = crate::runtime::shared::LuaCall::enter(state);
                ffi::lua_call(state, 2, 0);
                return 0;
            }
            _ => ffi::lua_pop(state, 1),
        }
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
