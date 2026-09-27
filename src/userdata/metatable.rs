//! `MetatableBuilder` (`components/luau/binding.hpp`, `binding.cpp`).
//!
//! This slice carries the skeleton and the methods-only phase: a plain methods table becomes
//! `__index` directly (phase A). The first property getter upgrades to generated
//! `__index` + `__namecall` dispatch (phase B), setters add `__newindex`; those arrive with
//! the builders phase. Conflict rules are enforced from the start so the phase machine can
//! grow without changing behaviour already relied on.

use std::ffi::{CString, c_int};

use crate::debug_name;
use crate::error::{Error, Result};
use crate::raw::ffi;
use crate::stack::Stack;

/// Configures a metatable that lives at a fixed stack index for the builder's lifetime.
///
/// Not `Clone`: copies would alias one metatable while carrying independent registration
/// flags.
pub struct MetatableBuilder<'s> {
    stack: &'s Stack<'s>,
    metatable: c_int,
    roots: &'s [&'s str],
    has_explicit_index: bool,
    /// Registry reference to the methods table once the first method is registered.
    methods_table: Option<c_int>,
}

impl<'s> MetatableBuilder<'s> {
    /// Wraps the mutable table at `metatable`. Metatables are protected by default: a missing
    /// `__metatable` field is set to `false`.
    pub(crate) fn new(stack: &'s Stack<'s>, metatable: c_int, roots: &'s [&'s str]) -> Result<Self> {
        let state = stack.state();
        // SAFETY: `metatable` is an index the caller pushed within the current frame.
        unsafe {
            let metatable = ffi::lua_absindex(state, metatable);
            if !ffi::lua_istable(state, metatable) {
                return Err(Error::logic("Expected a metatable table"));
            }
            if ffi::lua_getreadonly(state, metatable) != 0 {
                return Err(Error::logic("Cannot configure a read-only metatable"));
            }
            ffi::lua_rawgetfield(state, metatable, c"__metatable".as_ptr());
            let already_protected = !ffi::lua_isnil(state, -1);
            ffi::lua_pop(state, 1);
            if !already_protected {
                ffi::lua_pushboolean(state, 0);
                ffi::lua_setfield(state, metatable, c"__metatable".as_ptr());
            }
            Ok(MetatableBuilder { stack, metatable, roots, has_explicit_index: false, methods_table: None })
        }
    }

    /// Sets the script-visible `__type`. Registration does this from the type's `NAME`.
    pub fn set_type(&mut self, name: &str) -> Result<()> {
        let state = self.stack.state();
        unsafe {
            ffi::lua_pushlstring(state, name.as_ptr().cast(), name.len());
            ffi::lua_setfield(state, self.metatable, c"__type".as_ptr());
        }
        Ok(())
    }

    /// The `__type` string, required before members can be named.
    fn debug_prefix(&self) -> Result<String> {
        let state = self.stack.state();
        unsafe {
            ffi::lua_rawgetfield(state, self.metatable, c"__type".as_ptr());
            let mut length = 0usize;
            let text = ffi::lua_tolstring(state, -1, &mut length);
            let result = if text.is_null() {
                Err(Error::logic("metatable has no __type; set it before registering members"))
            } else {
                Ok(String::from_utf8_lossy(std::slice::from_raw_parts(text.cast::<u8>(), length)).into_owned())
            };
            ffi::lua_pop(state, 1);
            result
        }
    }

    /// Registers a raw C function as a method named `<__type>.<name>`.
    ///
    /// Methods-only metatables use a plain table as `__index`; duplicate names, an explicit
    /// `__index`, or a pre-existing `__index` that is not our table are logic errors.
    pub fn raw_method(&mut self, name: &str, function: ffi::lua_CFunction) -> Result<()> {
        if self.has_explicit_index {
            return Err(Error::logic("Explicit __index conflicts with method registration"));
        }
        let type_name = self.debug_prefix()?;
        let debug_name = format!("{type_name}.{name}");
        let state = self.stack.state();
        let key = CString::new(name).map_err(|_| Error::logic("Method name cannot contain NUL"))?;
        // SAFETY: indexes below are relative to values pushed here; the methods table is
        // pinned in the registry so its reference survives frames.
        unsafe {
            let retained = debug_name::retain(state, &debug_name, self.roots)?;
            if self.methods_table.is_none() {
                ffi::lua_rawgetfield(state, self.metatable, c"__index".as_ptr());
                let occupied = !ffi::lua_isnil(state, -1);
                ffi::lua_pop(state, 1);
                if occupied {
                    return Err(Error::logic("Existing __index conflicts with generated member dispatch"));
                }
                ffi::lua_newtable(state);
                ffi::lua_pushvalue(state, -1);
                ffi::lua_setfield(state, self.metatable, c"__index".as_ptr());
                self.methods_table = Some(ffi::lua_ref(state, -1));
                ffi::lua_pop(state, 1);
            }
            let methods = self.methods_table.expect("set above");
            ffi::lua_getref(state, methods);
            ffi::lua_rawgetfield(state, -1, key.as_ptr());
            let taken = !ffi::lua_isnil(state, -1);
            ffi::lua_pop(state, 1);
            if taken {
                ffi::lua_pop(state, 1);
                return Err(Error::logic(format!("{type_name}.{name} already registered")));
            }
            ffi::lua_pushcfunction(state, function, retained);
            ffi::lua_rawsetfield(state, -2, key.as_ptr());
            ffi::lua_pop(state, 1);
        }
        Ok(())
    }

    /// Installs a raw C function as an explicit metamethod named `<__type>.<metamethod>`.
    pub fn raw_metamethod(&mut self, metamethod: &str, function: ffi::lua_CFunction) -> Result<()> {
        if metamethod == "__index" {
            if self.has_explicit_index || self.methods_table.is_some() {
                return Err(Error::logic("Metatable already has an explicit or generated __index"));
            }
            self.has_explicit_index = true;
        }
        let type_name = self.debug_prefix()?;
        let debug_name = format!("{type_name}.{metamethod}");
        let key = CString::new(metamethod).map_err(|_| Error::logic("Metamethod name cannot contain NUL"))?;
        let state = self.stack.state();
        unsafe {
            let retained = debug_name::retain(state, &debug_name, self.roots)?;
            ffi::lua_pushcfunction(state, function, retained);
            ffi::lua_rawsetfield(state, self.metatable, key.as_ptr());
        }
        Ok(())
    }
}

impl Drop for MetatableBuilder<'_> {
    fn drop(&mut self) {
        if let Some(methods) = self.methods_table {
            // SAFETY: the reference was created by lua_ref on this VM and is released once.
            unsafe { ffi::lua_unref(self.stack.state(), methods) };
        }
    }
}
