//! `vector:writef32x3(buffer, offset)`: the interpreter half of OpenMW's native vector buffer
//! writer (`components/lua/nativevectorbuffer.cpp`).
//!
//! Luau's `vector` metatable ships an `__index` but no `__namecall`. Installing one lets a
//! script write a vector into a buffer in a single call with exactly the semantics of three
//! `buffer.writef32` calls: same offset conversion, same bounds check, same message, host byte
//! order. Any other method name resolves through the original `__index` (kept as upvalue 1) so
//! its diagnostics are unchanged. The code generation half (lowering the call to three native
//! f32 stores) needs Luau's C++ CodeGen hooks, which the C API does not expose; with the `jit`
//! feature this call runs through the interpreter path, still one call instead of three.

use std::ffi::{CStr, c_int};

use crate::error::{Error, Result};
use crate::raw::ffi;
use crate::runtime::Runtime;

const WRITER_NAME: &CStr = c"writef32x3";
const VECTOR_BYTES: usize = 3 * size_of::<f32>();

/// Upvalue 2 holds the writer's atom when the installed catalogue names it, else -1.
unsafe fn writer_atom(state: *mut ffi::lua_State) -> c_int {
    // SAFETY: upvalue 2 is the integer pushed at install time.
    unsafe { ffi::lua_tointegerx(state, ffi::lua_upvalueindex(2), std::ptr::null_mut()) }
}

/// `writef32x3` itself. Raises through `luaL_*`, which unwind straight to Luau's `pcall`: no
/// Rust value that needs dropping is live at any raise.
unsafe fn write_f32x3(state: *mut ffi::lua_State) -> c_int {
    unsafe {
        let components = ffi::luaL_checkvector(state, 1);
        let mut length = 0usize;
        let data = ffi::luaL_checkbuffer(state, 2, &mut length);
        let offset = ffi::luaL_checkinteger(state, 3);
        // Luau's buffer library treats the offset as unsigned so negatives fall out of range.
        if u64::from(offset as u32) + VECTOR_BYTES as u64 > length as u64 {
            ffi::luaL_errorL(state, c"buffer access out of bounds".as_ptr());
        }
        std::ptr::copy_nonoverlapping(
            components.cast::<u8>(),
            data.cast::<u8>().add(offset as u32 as usize),
            VECTOR_BYTES,
        );
        0
    }
}

unsafe extern "C-unwind" fn vector_namecall(state: *mut ffi::lua_State) -> c_int {
    unsafe {
        let mut atom: c_int = -1;
        let name = ffi::lua_namecallatom(state, &mut atom);
        let known = writer_atom(state);
        let is_writer = if known >= 0 { atom == known } else { !name.is_null() && CStr::from_ptr(name) == WRITER_NAME };
        if is_writer {
            return write_f32x3(state);
        }
        // Not ours: look the name up through the original __index and report exactly what the
        // interpreter would have.
        ffi::lua_pushvalue(state, ffi::lua_upvalueindex(1));
        ffi::lua_pushvalue(state, 1);
        ffi::lua_pushstring(state, if name.is_null() { c"".as_ptr() } else { name });
        {
            let _lua_call = crate::runtime::shared::LuaCall::enter(state);
            ffi::lua_call(state, 2, 1);
        }
        ffi::luaL_errorL(state, c"attempt to call a %s value".as_ptr(), ffi::luaL_typename(state, -1));
    }
}

impl Runtime {
    /// Installs `vector:writef32x3(buffer, offset)` on Luau's vector metatable. Fails when the
    /// metatable already has a `__namecall`. Uses this VM's atom for `writef32x3` when its
    /// catalogue has one, else compares the method name.
    pub fn install_vector_buffer_writer(&self) -> Result<()> {
        let atom = self.atom_of("writef32x3").map_or(-1, c_int::from);
        let stack = self.stack();
        stack.with_frame(|frame| {
            let state = frame.state();
            // SAFETY: every push is owned by the frame; the metatable is unfrozen only for the
            // one raw set and refrozen before returning.
            unsafe {
                ffi::lua_pushvector(state, 0.0, 0.0, 0.0);
                if ffi::lua_getmetatable(state, -1) == 0 {
                    return Err(Error::logic("Luau vector metatable is not available"));
                }
                let metatable = ffi::lua_gettop(state);
                if ffi::lua_rawgetfield(state, metatable, c"__index".as_ptr()) != ffi::LUA_TFUNCTION {
                    return Err(Error::logic("Luau vector __index metamethod is not available"));
                }
                if ffi::lua_rawgetfield(state, metatable, c"__namecall".as_ptr()) != ffi::LUA_TNIL {
                    return Err(Error::logic("Luau vector metatable already has a __namecall metamethod"));
                }
                ffi::lua_pop(state, 1);
                let read_only = ffi::lua_getreadonly(state, metatable) != 0;
                if read_only {
                    ffi::lua_setreadonly(state, metatable, 0);
                }
                ffi::lua_pushinteger(state, atom);
                ffi::lua_pushcclosure(state, vector_namecall, c"vector.__namecall".as_ptr(), 2);
                ffi::lua_rawsetfield(state, metatable, c"__namecall".as_ptr());
                if read_only {
                    ffi::lua_setreadonly(state, metatable, 1);
                }
            }
            Ok(())
        })
    }
}
