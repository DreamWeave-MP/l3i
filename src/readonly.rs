//! Read-only tables (`components/lua/readonlytables.cpp`).
//!
//! Two flavours, as OpenMW has them:
//! - freezing a table in place (`lua_setreadonly`), optionally with a shared strict metatable
//!   whose `__index` raises `Key not found` for missing keys;
//! - a read-only *view*: a frozen proxy whose `__index` is the backing table, with shared
//!   `__pairs`/`__iter`/`__ipairs` factories that iterate the backing table without exposing it,
//!   a `__len`, and `__metatable = false`. The proxy's backing table is recorded in a VM-private
//!   weak-keyed registry table so a view can be recognised and re-targeted later.

use std::ffi::{c_int, c_void};

use crate::diagnostics;
use crate::error::{Error, Result};
use crate::raw::ffi;
use crate::runtime::Runtime;
use crate::stack::{Frame, Scope};
use crate::value::{Function, Table, Value};

static VIEW_REGISTRY_KEY: u8 = 0;
static STRICT_METATABLE_KEY: u8 = 0;
static PAIRS_FACTORY_KEY: u8 = 0;
static IPAIRS_FACTORY_KEY: u8 = 0;

fn key(marker: &'static u8) -> *mut c_void {
    (marker as *const u8).cast_mut().cast()
}

/// Pushes the weak table the registry holds under `key`, creating it with `mode` on first use
/// (`pushWeakRegistryTable`).
///
/// # Safety
/// `state` is live with room for three values.
unsafe fn push_weak_registry_table(state: *mut ffi::lua_State, key: *mut c_void, mode: &std::ffi::CStr) {
    unsafe {
        if ffi::lua_rawgetp(state, ffi::LUA_REGISTRYINDEX, key) == ffi::LUA_TTABLE {
            return;
        }
        ffi::lua_pop(state, 1);
        ffi::lua_newtable(state);
        ffi::lua_createtable(state, 0, 1);
        ffi::lua_pushstring(state, mode.as_ptr());
        ffi::lua_rawsetfield(state, -2, c"__mode".as_ptr());
        ffi::lua_setmetatable(state, -2);
        ffi::lua_pushvalue(state, -1);
        ffi::lua_rawsetp(state, ffi::LUA_REGISTRYINDEX, key);
    }
}

/// Pushes the backing table of the view at `view`, or nothing and returns false.
unsafe fn push_backing(state: *mut ffi::lua_State, view: c_int) -> bool {
    unsafe {
        let view = ffi::lua_absindex(state, view);
        push_weak_registry_table(state, key(&VIEW_REGISTRY_KEY), c"k");
        ffi::lua_pushvalue(state, view);
        ffi::lua_rawget(state, -2);
        ffi::lua_remove(state, -2);
        if !ffi::lua_istable(state, -1) {
            ffi::lua_pop(state, 1);
            return false;
        }
        ffi::lua_rawgetfield(state, -1, c"backing".as_ptr());
        ffi::lua_remove(state, -2);
        if !ffi::lua_istable(state, -1) {
            ffi::lua_pop(state, 1);
            return false;
        }
        true
    }
}

/// Pushes the `{backing, strict}` metadata of a genuine read-only view at `index`, or nothing
/// and returns false. Genuine means: frozen, frozen protected metatable, registered metadata
/// whose backing is a plain table (not itself a view).
unsafe fn push_view_metadata(state: *mut ffi::lua_State, index: c_int) -> bool {
    unsafe {
        let index = ffi::lua_absindex(state, index);
        if ffi::lua_getreadonly(state, index) == 0 || ffi::lua_getmetatable(state, index) == 0 {
            return false;
        }
        let metatable_read_only = ffi::lua_getreadonly(state, -1) != 0;
        ffi::lua_rawgetfield(state, -1, c"__metatable".as_ptr());
        let protected = ffi::lua_type(state, -1) == ffi::LUA_TBOOLEAN && ffi::lua_toboolean(state, -1) == 0;
        ffi::lua_pop(state, 2);
        if !metatable_read_only || !protected {
            return false;
        }
        push_weak_registry_table(state, key(&VIEW_REGISTRY_KEY), c"k");
        ffi::lua_pushvalue(state, index);
        ffi::lua_rawget(state, -2);
        ffi::lua_remove(state, -2);
        if !ffi::lua_istable(state, -1) || ffi::lua_getreadonly(state, -1) == 0 {
            ffi::lua_pop(state, 1);
            return false;
        }
        ffi::lua_rawgetfield(state, -1, c"backing".as_ptr());
        let has_backing = ffi::lua_istable(state, -1);
        let backing_is_view = has_backing && {
            let backing = ffi::lua_gettop(state);
            let is_view = push_backing(state, backing);
            if is_view {
                ffi::lua_pop(state, 1);
            }
            is_view
        };
        ffi::lua_pop(state, 1);
        ffi::lua_rawgetfield(state, -1, c"strict".as_ptr());
        let has_strictness = ffi::lua_type(state, -1) == ffi::LUA_TBOOLEAN;
        ffi::lua_pop(state, 1);
        if !has_backing || backing_is_view || !has_strictness {
            ffi::lua_pop(state, 1);
            return false;
        }
        true
    }
}

// ---------------------------------------------------------------------------------------------
// Shared C functions
// ---------------------------------------------------------------------------------------------

unsafe extern "C-unwind" fn pairs_iterator(state: *mut ffi::lua_State) -> c_int {
    unsafe {
        ffi::luaL_checktype(state, 1, ffi::LUA_TTABLE);
        ffi::lua_settop(state, 2);
        if !push_backing(state, 1) {
            diagnostics::raise_at_caller(state, "Invalid read-only iterator state");
        }
        ffi::lua_replace(state, 1);
        if ffi::lua_next(state, 1) == 0 { 0 } else { 2 }
    }
}

unsafe extern "C-unwind" fn ipairs_iterator(state: *mut ffi::lua_State) -> c_int {
    unsafe {
        ffi::luaL_checktype(state, 1, ffi::LUA_TTABLE);
        ffi::lua_settop(state, 2);
        if !push_backing(state, 1) {
            diagnostics::raise_at_caller(state, "Invalid read-only iterator state");
        }
        ffi::lua_replace(state, 1);
        ffi::luaL_checktype(state, 2, ffi::LUA_TNUMBER);
        let control = ffi::lua_tonumber(state, 2);
        if !control.is_finite() || control.trunc() != control || control < 0.0 || control >= f64::from(c_int::MAX) {
            diagnostics::raise_at_caller(state, "Invalid read-only ipairs iterator state");
        }
        let index = control as c_int + 1;
        ffi::lua_pushnumber(state, f64::from(index));
        ffi::lua_rawgeti(state, 1, index);
        if ffi::lua_isnil(state, -1) {
            ffi::lua_settop(state, 2);
            return 0;
        }
        2
    }
}

unsafe extern "C-unwind" fn pairs_factory(state: *mut ffi::lua_State) -> c_int {
    unsafe {
        ffi::luaL_checktype(state, 1, ffi::LUA_TTABLE);
        ffi::lua_pushvalue(state, ffi::lua_upvalueindex(1));
        ffi::lua_pushvalue(state, 1);
        ffi::lua_pushnil(state);
        3
    }
}

unsafe extern "C-unwind" fn ipairs_factory(state: *mut ffi::lua_State) -> c_int {
    unsafe {
        ffi::luaL_checktype(state, 1, ffi::LUA_TTABLE);
        ffi::lua_pushvalue(state, ffi::lua_upvalueindex(1));
        ffi::lua_pushvalue(state, 1);
        ffi::lua_pushnumber(state, 0.0);
        3
    }
}

/// `Key not found: <key>` using `luaL_tolstring` for the key text.
unsafe fn raise_key_not_found(state: *mut ffi::lua_State) -> ! {
    unsafe {
        let mut length = 0usize;
        let text = ffi::luaL_tolstring(state, 2, &mut length);
        let key_text = String::from_utf8_lossy(std::slice::from_raw_parts(text.cast::<u8>(), length)).into_owned();
        ffi::lua_pop(state, 1);
        diagnostics::raise_at_caller(state, &format!("Key not found: {key_text}"))
    }
}

/// `__index` of a strict view: reads through the backing table (upvalue 1), honouring its
/// metatable, and raises for a missing key.
unsafe extern "C-unwind" fn strict_view_index(state: *mut ffi::lua_State) -> c_int {
    unsafe {
        ffi::lua_pushvalue(state, ffi::lua_upvalueindex(1));
        ffi::lua_pushvalue(state, 2);
        ffi::lua_gettable(state, -2);
        if ffi::lua_isnil(state, -1) {
            raise_key_not_found(state);
        }
        1
    }
}

unsafe extern "C-unwind" fn strict_index_miss(state: *mut ffi::lua_State) -> c_int {
    unsafe { raise_key_not_found(state) }
}

unsafe extern "C-unwind" fn read_only_length(state: *mut ffi::lua_State) -> c_int {
    unsafe {
        ffi::lua_pushinteger(state, ffi::lua_objlen(state, ffi::lua_upvalueindex(1)));
        1
    }
}

/// Internal helpers are named under the host's first debug root.
fn internal_name(runtime: &Runtime, suffix: &str) -> String {
    format!("{}.internal.{suffix}", runtime.debug_roots().first().copied().unwrap_or("dreamweave"))
}

/// Pushes the shared `pairs`/`ipairs` factory, creating it (and its iterator) on first use.
unsafe fn push_shared_factory(
    runtime: &Runtime,
    state: *mut ffi::lua_State,
    key: *mut c_void,
    factory: ffi::lua_CFunction,
    iterator: ffi::lua_CFunction,
    factory_name: &str,
    iterator_name: &str,
) -> Result<()> {
    unsafe {
        if ffi::lua_rawgetp(state, ffi::LUA_REGISTRYINDEX, key) == ffi::LUA_TFUNCTION {
            return Ok(());
        }
        ffi::lua_pop(state, 1);
        let iterator_name = crate::debug_name::retain(state, &internal_name(runtime, iterator_name), runtime.debug_roots())?;
        let factory_name = crate::debug_name::retain(state, &internal_name(runtime, factory_name), runtime.debug_roots())?;
        ffi::lua_pushcfunction(state, iterator, iterator_name);
        ffi::lua_pushcclosure(state, factory, factory_name, 1);
        ffi::lua_pushvalue(state, -1);
        ffi::lua_rawsetp(state, ffi::LUA_REGISTRYINDEX, key);
        Ok(())
    }
}

/// Shared by every table frozen with [`make_strict_read_only`].
unsafe fn push_strict_metatable(runtime: &Runtime, state: *mut ffi::lua_State) -> Result<()> {
    unsafe {
        if ffi::lua_rawgetp(state, ffi::LUA_REGISTRYINDEX, key(&STRICT_METATABLE_KEY)) == ffi::LUA_TTABLE {
            return Ok(());
        }
        ffi::lua_pop(state, 1);
        let name = crate::debug_name::retain(state, &internal_name(runtime, "strictIndex"), runtime.debug_roots())?;
        ffi::lua_createtable(state, 0, 2);
        ffi::lua_pushcfunction(state, strict_index_miss, name);
        ffi::lua_rawsetfield(state, -2, c"__index".as_ptr());
        ffi::lua_pushboolean(state, 0);
        ffi::lua_rawsetfield(state, -2, c"__metatable".as_ptr());
        ffi::lua_setreadonly(state, -1, 1);
        ffi::lua_pushvalue(state, -1);
        ffi::lua_rawsetp(state, ffi::LUA_REGISTRYINDEX, key(&STRICT_METATABLE_KEY));
        Ok(())
    }
}

/// Builds the proxy for the table on top of `frame`; leaves the proxy on top.
fn build_proxy(runtime: &Runtime, frame: &Frame<'_>, strict: bool) -> Result<()> {
    let state = frame.state();
    // SAFETY: the backing table is on top of the frame; every index is absolute and the frame
    // owns every temporary.
    unsafe {
        let mut backing = ffi::lua_gettop(state);
        if push_view_metadata(state, backing) {
            // Already a view: reuse it unless a strict view over a lax one is requested.
            let metadata = ffi::lua_gettop(state);
            ffi::lua_rawgetfield(state, metadata, c"strict".as_ptr());
            let existing_strict = ffi::lua_toboolean(state, -1) != 0;
            ffi::lua_pop(state, 1);
            if !strict || existing_strict {
                ffi::lua_pop(state, 1);
                return Ok(());
            }
            ffi::lua_rawgetfield(state, metadata, c"backing".as_ptr());
            ffi::lua_remove(state, metadata);
            backing = metadata;
        }

        ffi::lua_newtable(state);
        let proxy = ffi::lua_gettop(state);
        ffi::lua_newtable(state);
        let metatable = ffi::lua_gettop(state);
        ffi::lua_pushvalue(state, backing);
        if strict {
            let name = crate::debug_name::retain(state, &internal_name(runtime, "strictViewIndex"), runtime.debug_roots())?;
            ffi::lua_pushcclosure(state, strict_view_index, name, 1);
        }
        ffi::lua_rawsetfield(state, metatable, c"__index".as_ptr());
        push_shared_factory(runtime, state, key(&PAIRS_FACTORY_KEY), pairs_factory, pairs_iterator, "readOnlyPairs", "readOnlyPairsIterator")?;
        ffi::lua_pushvalue(state, -1);
        ffi::lua_rawsetfield(state, metatable, c"__pairs".as_ptr());
        ffi::lua_rawsetfield(state, metatable, c"__iter".as_ptr());
        push_shared_factory(runtime, state, key(&IPAIRS_FACTORY_KEY), ipairs_factory, ipairs_iterator, "readOnlyIpairs", "readOnlyIpairsIterator")?;
        ffi::lua_rawsetfield(state, metatable, c"__ipairs".as_ptr());
        let length_name = crate::debug_name::retain(state, &internal_name(runtime, "readOnlyLength"), runtime.debug_roots())?;
        ffi::lua_pushvalue(state, backing);
        ffi::lua_pushcclosure(state, read_only_length, length_name, 1);
        ffi::lua_rawsetfield(state, metatable, c"__len".as_ptr());
        ffi::lua_pushboolean(state, 0);
        ffi::lua_rawsetfield(state, metatable, c"__metatable".as_ptr());
        ffi::lua_setreadonly(state, metatable, 1);
        ffi::lua_setmetatable(state, proxy);
        ffi::lua_setreadonly(state, proxy, 1);

        ffi::lua_newtable(state);
        let metadata = ffi::lua_gettop(state);
        ffi::lua_pushvalue(state, backing);
        ffi::lua_rawsetfield(state, metadata, c"backing".as_ptr());
        ffi::lua_pushboolean(state, c_int::from(strict));
        ffi::lua_rawsetfield(state, metadata, c"strict".as_ptr());
        ffi::lua_setreadonly(state, metadata, 1);
        push_weak_registry_table(state, key(&VIEW_REGISTRY_KEY), c"k");
        ffi::lua_pushvalue(state, proxy);
        ffi::lua_pushvalue(state, metadata);
        ffi::lua_rawset(state, -3);
        ffi::lua_pop(state, 2);
        // Leave only the proxy above the original backing slot.
        ffi::lua_replace(state, backing);
        ffi::lua_settop(state, backing);
    }
    Ok(())
}

fn require_same_vm(runtime: &Runtime, value: &Value) -> Result<()> {
    if !value.belongs_to(runtime.stack().state_ptr()) {
        return Err(Error::logic("Read-only table belongs to a different Lua state"));
    }
    Ok(())
}

/// Freezes `table` in place.
pub fn make_read_only(runtime: &Runtime, table: &Table) -> Result<()> {
    require_same_vm(runtime, table.value())?;
    table.value().with_value(&runtime.stack(), |_, view| view.as_table()?.set_read_only(true))
}

/// A frozen proxy reading through `table`; iteration and length come from the backing table.
pub fn make_read_only_view(runtime: &Runtime, table: &Table) -> Result<Table> {
    make_view(runtime, table, false)
}

/// Like [`make_read_only_view`], but reading a missing key raises `Key not found`.
pub fn make_strict_read_only_view(runtime: &Runtime, table: &Table) -> Result<Table> {
    make_view(runtime, table, true)
}

fn make_view(runtime: &Runtime, table: &Table, strict: bool) -> Result<Table> {
    require_same_vm(runtime, table.value())?;
    let stack = runtime.stack();
    stack.with_frame(|frame| {
        table.push_to(frame)?;
        build_proxy(runtime, frame, strict)?;
        Table::from_value(Value::store(frame.top_value())?)
    })
}

/// Freezes `table` in place with the shared strict metatable: missing keys raise `Key not
/// found`. The table must not already be frozen or have a metatable.
pub fn make_strict_read_only(runtime: &Runtime, table: &Table) -> Result<()> {
    require_same_vm(runtime, table.value())?;
    let stack = runtime.stack();
    stack.with_frame(|frame| {
        let view = table.push_to(frame)?;
        let state = frame.state();
        unsafe {
            if ffi::lua_getreadonly(state, view.index()) != 0 {
                return Err(Error::logic("Strict read-only table is already frozen"));
            }
            if ffi::lua_getmetatable(state, view.index()) != 0 {
                return Err(Error::logic("Strict read-only table already has a metatable"));
            }
            push_strict_metatable(runtime, state)?;
            ffi::lua_setmetatable(state, view.index());
            ffi::lua_setreadonly(state, view.index(), 1);
        }
        Ok(())
    })
}

/// Sets `key` on a frozen table (or the backing table of a view), restoring the frozen state
/// afterwards. For tables the host owns.
pub fn set_read_only_field<T: crate::convert::Push + ?Sized>(runtime: &Runtime, table: &Table, key: &str, value: &T) -> Result<()> {
    require_same_vm(runtime, table.value())?;
    let stack = runtime.stack();
    stack.with_frame(|frame| {
        let view = table.push_to(frame)?;
        let state = frame.state();
        unsafe {
            if ffi::lua_getreadonly(state, view.index()) == 0 {
                return Err(Error::logic("Expected a read-only table"));
            }
            let mut target = view.index();
            if push_view_metadata(state, target) {
                let metadata = ffi::lua_gettop(state);
                ffi::lua_rawgetfield(state, metadata, c"backing".as_ptr());
                ffi::lua_remove(state, metadata);
                target = metadata;
            }
            let was_read_only = ffi::lua_getreadonly(state, target) != 0;
            ffi::lua_setreadonly(state, target, 0);
            let stored = (|| {
                frame.push(value)?;
                ffi::lua_pushlstring(state, key.as_ptr().cast(), key.len());
                ffi::lua_insert(state, -2);
                ffi::lua_rawset(state, target);
                Ok(())
            })();
            ffi::lua_setreadonly(state, target, c_int::from(was_read_only));
            stored
        }
    })
}

/// Freezes a package table after giving it a metatable with only `__tostring`.
pub fn make_frozen_package(runtime: &Runtime, package: &Table, to_string: &Function) -> Result<()> {
    require_same_vm(runtime, package.value())?;
    if !to_string.value().belongs_to(runtime.stack().state_ptr()) {
        return Err(Error::logic("Expected a tostring function from the same Lua state"));
    }
    let stack = runtime.stack();
    stack.with_frame(|frame| {
        let view = package.push_to(frame)?;
        let state = frame.state();
        unsafe {
            if ffi::lua_getmetatable(state, view.index()) != 0 {
                return Err(Error::logic("Frozen package already has a metatable"));
            }
            ffi::lua_createtable(state, 0, 2);
            to_string.push_to(frame)?;
            ffi::lua_rawsetfield(state, -2, c"__tostring".as_ptr());
            ffi::lua_pushboolean(state, 0);
            ffi::lua_rawsetfield(state, -2, c"__metatable".as_ptr());
            ffi::lua_setreadonly(state, -1, 1);
            ffi::lua_setmetatable(state, view.index());
            ffi::lua_setreadonly(state, view.index(), 1);
        }
        Ok(())
    })
}
