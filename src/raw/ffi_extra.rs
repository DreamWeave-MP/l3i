//! Luau C API that the vendored VM exports but `mlua-sys` 0.12 does not declare.
//!
//! These prototypes are copied from `luau/VM/include/lua.h` (Luau 0.736) and link against
//! the same static library `mlua-sys` builds. Nothing here is a Luau modification.

#![allow(non_camel_case_types, dead_code)] // C names on purpose; consumers arrive with the direct-access phase

use std::ffi::{c_char, c_int, c_void};

use mlua::ffi::lua_State;

/// `void (*)(lua_State*, void* data, int atom, uint16_t* cachedslot, int utag)`
pub type lua_UserdataDirectAccess =
    unsafe extern "C-unwind" fn(*mut lua_State, *mut c_void, c_int, *mut u16, c_int);

/// `int (*)(lua_State*, void* data, int atom, uint16_t* cachedslot, int utag)`
pub type lua_UserdataDirectNamecall =
    unsafe extern "C-unwind" fn(*mut lua_State, *mut c_void, c_int, *mut u16, c_int) -> c_int;

/// `void (*)(void* ud, void* result)`; `result` is written with the
/// `lua_userdatadirectfield_set_*` helpers.
pub type lua_UserdataDirectFieldGet = unsafe extern "C-unwind" fn(*mut c_void, *mut c_void);

unsafe extern "C-unwind" {
    pub fn lua_registeruserdatadirectaccess(
        L: *mut lua_State,
        tag: c_int,
        get: Option<lua_UserdataDirectAccess>,
        set: Option<lua_UserdataDirectAccess>,
        namecall: Option<lua_UserdataDirectNamecall>,
    ) -> c_int;

    pub fn lua_registeruserdatadirectfieldget(
        L: *mut lua_State,
        tag: c_int,
        field: *const c_char,
        f: lua_UserdataDirectFieldGet,
    );

    pub fn lua_userdatadirectfield_setnumber(result: *mut c_void, value: f64);
    pub fn lua_userdatadirectfield_setvector(result: *mut c_void, x: f32, y: f32, z: f32);
    pub fn lua_userdatadirectfield_setboolean(result: *mut c_void, value: c_int);
    pub fn lua_userdatadirectfield_setinteger64(result: *mut c_void, value: i64);
    pub fn lua_userdatadirectfield_setnil(result: *mut c_void);
}
