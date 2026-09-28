//! `MetatableBuilder` (`components/luau/binding.hpp`, `binding.cpp`).
//!
//! Member registration is a phase machine with explicit conflict rules:
//! - methods only: a plain methods table becomes `__index` (phase A, no dispatcher);
//! - the first property getter atomically upgrades to a generated `__index` (methods, then
//!   getters) plus a generated `__namecall` keyed by Luau atoms (phase B);
//! - the first setter installs a generated `__newindex`; misses are read-only errors;
//! - explicit `__index`/`__newindex`/`__namecall`/`__len`, native method tables, and duplicate
//!   member names conflict with generated dispatch and fail at registration.
//!
//! Metatables are protected by default (`__metatable = false`), `__type` is explicit, debug
//! names are `<__type>.<member>` / `<__type>.get.<member>` / `<__type>.set.<member>`, and the
//! table is frozen by the registration that owns it.

use std::ffi::{CStr, CString, c_int};
use std::marker::PhantomData;

use super::dispatch;
use crate::bind::{Binding, Call, MemberEntry, ParamKind, function_closure};
use crate::debug_name;
use crate::error::{Error, Result};
use crate::raw::ffi;
use crate::stack::Frame;
use crate::value::Value;

#[derive(Clone, Copy, PartialEq, Eq)]
enum MemberKind {
    Method,
    Getter,
    Setter,
}

/// Configures a metatable that lives at a fixed stack index for the builder's lifetime.
///
/// Not `Clone`: copies would alias one metatable while carrying independent registration
/// flags.
pub struct MetatableBuilder<'s> {
    state: *mut ffi::lua_State,
    metatable: c_int,
    roots: &'s [&'s str],
    has_explicit_index: bool,
    has_explicit_newindex: bool,
    has_explicit_len: bool,
    /// Method closures by name; the plain `__index` before any getter exists, and the
    /// conflict-check and keep-alive table afterwards.
    methods: Option<Value>,
    /// Method entries (or unbound functions) by name, for `__namecall` misses on the atom index.
    method_entries: Option<Value>,
    /// Method entries (or unbound functions) by Luau atom, for `__namecall`.
    method_atoms: Option<Value>,
    /// Method functions and getter entries by name: the generated `__index` table.
    members: Option<Value>,
    /// Getter closures by name (conflict checks, keep-alive for the entries in `members`).
    getters: Option<Value>,
    /// Setter closures by name (conflict checks, keep-alive for `setter_entries`).
    setters: Option<Value>,
    /// Setter entries by name: the generated `__newindex` table.
    setter_entries: Option<Value>,
    native_methods: Option<Value>,
    native_methods_frozen: bool,
    _frame: PhantomData<&'s Frame<'s>>,
}

impl<'s> MetatableBuilder<'s> {
    /// Wraps the mutable table at `metatable`. Metatables are protected by default: a missing
    /// `__metatable` field is set to `false`.
    pub(crate) fn new(frame: &'s Frame<'_>, metatable: c_int, roots: &'s [&'s str]) -> Result<Self> {
        let state = frame.state();
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
            Ok(MetatableBuilder {
                state,
                metatable,
                roots,
                has_explicit_index: false,
                has_explicit_newindex: false,
                has_explicit_len: false,
                methods: None,
                method_entries: None,
                method_atoms: None,
                members: None,
                getters: None,
                setters: None,
                setter_entries: None,
                native_methods: None,
                native_methods_frozen: false,
                _frame: PhantomData,
            })
        }
    }

    /// Called by the owning registration once configuration ends; releases builder pins.
    pub(crate) fn finish(self) -> Result<()> {
        Ok(())
    }

    pub(crate) fn state_ptr(&self) -> *mut ffi::lua_State {
        self.state
    }

    pub(crate) fn retain_name(&self, complete_name: &str) -> Result<*const std::ffi::c_char> {
        self.retain(complete_name)
    }

    pub(crate) fn field_is_nil(&self, name: &CStr) -> bool {
        self.rawget_field_is_nil(name)
    }

    /// Raw-sets the value on top of the stack into the metatable under `name`.
    ///
    /// # Safety
    /// A value is on top of the stack.
    pub(crate) unsafe fn raw_set_metatable_top(&self, name: &CStr) {
        unsafe { ffi::lua_rawsetfield(self.state, self.metatable, name.as_ptr()) }
    }

    /// Pushes the raw metatable field `name`.
    ///
    /// # Safety
    /// Room for one value on the stack.
    pub(crate) unsafe fn push_metatable_field(&self, name: &CStr) {
        unsafe { ffi::lua_rawgetfield(self.state, self.metatable, name.as_ptr()) };
    }

    fn rawget_field_is_nil(&self, name: &CStr) -> bool {
        unsafe {
            ffi::lua_rawgetfield(self.state, self.metatable, name.as_ptr());
            let is_nil = ffi::lua_isnil(self.state, -1);
            ffi::lua_pop(self.state, 1);
            is_nil
        }
    }

    /// Sets the script-visible `__type`. Registration does this from the type's `NAME`.
    pub fn set_type(&mut self, name: &str) -> Result<()> {
        unsafe {
            ffi::lua_pushlstring(self.state, name.as_ptr().cast(), name.len());
            ffi::lua_setfield(self.state, self.metatable, c"__type".as_ptr());
        }
        Ok(())
    }

    /// The `__type` string, required before members can be named.
    pub fn type_name(&self) -> Result<String> {
        unsafe {
            ffi::lua_rawgetfield(self.state, self.metatable, c"__type".as_ptr());
            let mut length = 0usize;
            let text = ffi::lua_tolstring(self.state, -1, &mut length);
            let result = if text.is_null() {
                Err(Error::logic("metatable has no __type; set it before registering members"))
            } else {
                Ok(String::from_utf8_lossy(std::slice::from_raw_parts(text.cast::<u8>(), length)).into_owned())
            };
            ffi::lua_pop(self.state, 1);
            result
        }
    }

    /// Raw-sets `value` under `name` in the metatable.
    pub fn set_field(&mut self, name: &str, value: &Value) -> Result<()> {
        if !value.belongs_to(self.state) {
            return Err(Error::logic("Metatable field belongs to a different Lua state"));
        }
        let key = CString::new(name).map_err(|_| Error::logic("Metatable field name cannot contain NUL"))?;
        unsafe {
            ffi::lua_getref(self.state, value.reference_id());
            ffi::lua_rawsetfield(self.state, self.metatable, key.as_ptr());
        }
        Ok(())
    }

    fn retain(&self, complete_name: &str) -> Result<*const std::ffi::c_char> {
        // SAFETY: live state; retain rebalances the stack itself.
        unsafe { debug_name::retain(self.state, complete_name, self.roots) }
    }

    /// Pushes a C function named `debug_name` and raw-sets it under `field`.
    fn set_c_field(&mut self, field: &str, function: ffi::lua_CFunction, debug_name: &str) -> Result<()> {
        let retained = self.retain(debug_name)?;
        let key = CString::new(field).map_err(|_| Error::logic("Metatable field name cannot contain NUL"))?;
        unsafe {
            ffi::lua_pushcfunction(self.state, function, retained);
            ffi::lua_rawsetfield(self.state, self.metatable, key.as_ptr());
        }
        Ok(())
    }

    /// Pushes a bound closure named `debug_name` and raw-sets it under `field`.
    fn set_bound_field<F: Binding<M>, M>(&mut self, field: &str, callable: F, debug_name: &str) -> Result<()> {
        let retained = self.retain(debug_name)?;
        let key = CString::new(field).map_err(|_| Error::logic("Metatable field name cannot contain NUL"))?;
        unsafe {
            function_closure(self.state, callable, retained)?;
            ffi::lua_rawsetfield(self.state, self.metatable, key.as_ptr());
        }
        Ok(())
    }

    // ---------------------------------------------------------------------------------------
    // Explicit metamethods
    // ---------------------------------------------------------------------------------------

    fn assert_explicit_index_installable(&self) -> Result<()> {
        if !self.rawget_field_is_nil(c"__index") || self.has_explicit_index {
            return Err(Error::logic("Metatable already has an explicit __index"));
        }
        if self.methods.is_some() || self.getters.is_some() {
            return Err(Error::logic("Explicit __index conflicts with registered methods/properties"));
        }
        Ok(())
    }

    fn assert_explicit_namecall_installable(&self) -> Result<()> {
        if !self.rawget_field_is_nil(c"__namecall") {
            return Err(Error::logic("Metatable already has an explicit or generated __namecall"));
        }
        Ok(())
    }

    fn assert_explicit_newindex_installable(&self) -> Result<()> {
        if self.setters.is_some() {
            return Err(Error::logic("Explicit __newindex conflicts with registered properties"));
        }
        Ok(())
    }

    fn assert_explicit_len_installable(&self) -> Result<()> {
        if !self.rawget_field_is_nil(c"__len") || self.has_explicit_len {
            return Err(Error::logic("Metatable already has an explicit __len"));
        }
        Ok(())
    }

    /// Installs a raw C function as `<metamethod>`, named `<__type>.<metamethod>`. The
    /// dispatch-bearing metamethods enforce their conflict rules.
    pub fn raw_metamethod(&mut self, metamethod: &str, function: ffi::lua_CFunction) -> Result<()> {
        self.check_metamethod_slot(metamethod)?;
        let debug_name = format!("{}.{metamethod}", self.type_name()?);
        self.set_c_field(metamethod, function, &debug_name)?;
        self.note_metamethod(metamethod);
        Ok(())
    }

    /// Installs a bound closure as `<metamethod>`, named `<__type>.<metamethod>`.
    pub fn metamethod<F: Binding<M>, M>(&mut self, metamethod: &str, callable: F) -> Result<()> {
        self.check_metamethod_slot(metamethod)?;
        let debug_name = format!("{}.{metamethod}", self.type_name()?);
        self.set_bound_field(metamethod, callable, &debug_name)?;
        self.note_metamethod(metamethod);
        Ok(())
    }

    /// Installs an already-built function as `<metamethod>`.
    pub fn metamethod_value(&mut self, metamethod: &str, function: &Value) -> Result<()> {
        self.check_metamethod_slot(metamethod)?;
        self.set_field(metamethod, function)?;
        self.note_metamethod(metamethod);
        Ok(())
    }

    fn check_metamethod_slot(&self, metamethod: &str) -> Result<()> {
        match metamethod {
            "__index" => self.assert_explicit_index_installable(),
            "__newindex" => self.assert_explicit_newindex_installable(),
            "__namecall" => self.assert_explicit_namecall_installable(),
            "__len" => self.assert_explicit_len_installable(),
            _ => Ok(()),
        }
    }

    fn note_metamethod(&mut self, metamethod: &str) {
        match metamethod {
            "__index" => self.has_explicit_index = true,
            "__newindex" => self.has_explicit_newindex = true,
            "__len" => self.has_explicit_len = true,
            _ => {}
        }
    }

    // ---------------------------------------------------------------------------------------
    // Methods and properties
    // ---------------------------------------------------------------------------------------

    /// Registers `callable` as method `name`. Its first parameter is the receiver (`&T` for a
    /// registered userdata type whose `NAME` is this metatable's `__type`); argument numbering
    /// in diagnostics excludes it.
    pub fn method<F: Binding<M>, M>(&mut self, name: &str, callable: F) -> Result<()> {
        self.check_member_allowed(MemberKind::Method)?;
        let type_name = self.validated_receiver::<F, M>()?;
        let (closure, entry) = self.member_closure(&format!("{type_name}.{name}"), callable)?;
        self.register_member(MemberKind::Method, &type_name, name, closure, Some(entry))
    }

    /// Registers a read-only property: `getter` takes the receiver and returns the value.
    pub fn property<G: Binding<MG>, MG>(&mut self, name: &str, getter: G) -> Result<()> {
        self.check_member_allowed(MemberKind::Getter)?;
        let type_name = self.validated_receiver::<G, MG>()?;
        let (closure, entry) = self.member_closure(&format!("{type_name}.get.{name}"), getter)?;
        self.register_member(MemberKind::Getter, &type_name, name, closure, Some(entry))
    }

    /// Registers a read/write property. The setter takes the receiver and exactly one Lua value.
    pub fn property_rw<G: Binding<MG>, MG, S: Binding<MS>, MS>(
        &mut self,
        name: &str,
        getter: G,
        setter: S,
    ) -> Result<()> {
        self.check_member_allowed(MemberKind::Getter)?;
        self.check_member_allowed(MemberKind::Setter)?;
        if self.has_explicit_index || self.has_explicit_newindex {
            return Err(Error::logic("Explicit __index/__newindex conflicts with setProperty"));
        }
        let setter_kinds = &S::PARAM_KINDS[1..];
        let required = setter_kinds.iter().filter(|k| **k == ParamKind::Regular).count();
        let max = setter_kinds.iter().filter(|k| **k != ParamKind::Injected).count();
        let terminator = setter_kinds.iter().any(|k| matches!(k, ParamKind::VarArgs | ParamKind::ArgView));
        if required > 1 || !(terminator || max >= 1) {
            return Err(Error::logic("A property setter must accept one Lua value argument"));
        }
        let type_name = self.validated_receiver::<G, MG>()?;
        let setter_type = self.validated_receiver::<S, MS>()?;
        if setter_type != type_name {
            return Err(Error::logic(format!("receiverTypeName mismatch for {type_name}")));
        }
        let (getter_closure, getter_entry) = self.member_closure(&format!("{type_name}.get.{name}"), getter)?;
        self.register_member(MemberKind::Getter, &type_name, name, getter_closure, Some(getter_entry))?;
        let (setter_closure, setter_entry) = self.member_closure(&format!("{type_name}.set.{name}"), setter)?;
        self.register_member(MemberKind::Setter, &type_name, name, setter_closure, Some(setter_entry))
    }

    /// Registers an already-bound function as a callable member without a native receiver
    /// type; it stays visible through the generated dispatchers.
    pub fn unbound_method(&mut self, name: &str, function: &Value) -> Result<()> {
        if !function.belongs_to(self.state) {
            return Err(Error::logic("Unbound method function belongs to a different Lua state"));
        }
        self.check_member_allowed(MemberKind::Method)?;
        let type_name = self.type_name()?;
        self.register_member(MemberKind::Method, &type_name, name, function.clone(), None)
    }

    fn check_member_allowed(&self, kind: MemberKind) -> Result<()> {
        if self.has_explicit_index {
            return Err(Error::logic(match kind {
                MemberKind::Method => "Explicit __index conflicts with setMethod",
                _ => "Explicit __index conflicts with setProperty",
            }));
        }
        if self.native_methods.is_some() {
            return Err(Error::logic(match kind {
                MemberKind::Method => "setMethod conflicts with native method registration",
                _ => "setProperty conflicts with native method registration",
            }));
        }
        Ok(())
    }

    /// The receiver type a member binding declares must be this metatable's `__type`.
    fn validated_receiver<F: Binding<M>, M>(&self) -> Result<String> {
        let type_name = self.type_name()?;
        match F::RECEIVER_NAME {
            None => Err(Error::logic("Member bindings need a receiver as their first parameter")),
            Some(receiver) if receiver != type_name => {
                Err(Error::logic(format!("receiverTypeName mismatch for {receiver}: metatable is {type_name}")))
            }
            Some(_) => Ok(type_name),
        }
    }

    /// Builds a method-mode closure, pins it, and returns its direct entry.
    fn member_closure<F: Binding<M>, M>(&mut self, debug_name: &str, callable: F) -> Result<(Value, MemberEntry)> {
        let retained = self.retain(debug_name)?;
        unsafe {
            let entry = crate::bind::method_member(self.state, callable, retained)?;
            let value = Value::store(crate::stack::ValueView::resolve(self.state, -1))?;
            ffi::lua_pop(self.state, 1);
            Ok((value, entry))
        }
    }

    /// Raw-sets `name` in `table` to the member's direct entry (a plain userdata holding a
    /// `MemberEntry`), or to `closure` itself for an unbound function.
    fn store_member(&self, table: &Value, name: &str, closure: &Value, entry: Option<MemberEntry>) {
        unsafe {
            ffi::lua_getref(self.state, table.reference_id());
            ffi::lua_pushlstring(self.state, name.as_ptr().cast(), name.len());
            self.push_member(closure, entry);
            ffi::lua_rawset(self.state, -3);
            ffi::lua_pop(self.state, 1);
        }
    }

    /// Pushes the entry userdata, or the closure when there is no entry.
    unsafe fn push_member(&self, closure: &Value, entry: Option<MemberEntry>) {
        const { crate::userdata::assert_userdata_layout::<MemberEntry>() };
        unsafe {
            match entry {
                Some(entry) => {
                    // SAFETY: a fresh untagged userdata of the entry's size, written before
                    // anything can read it; entries hold no destructor and no owned data.
                    let raw = ffi::lua_newuserdatatagged(self.state, std::mem::size_of::<MemberEntry>(), 0);
                    std::ptr::write(raw.cast::<MemberEntry>(), entry);
                }
                None => {
                    ffi::lua_getref(self.state, closure.reference_id());
                }
            }
        }
    }

    fn table_has_key(&self, table: &Value, name: &str) -> bool {
        unsafe {
            ffi::lua_getref(self.state, table.reference_id());
            ffi::lua_pushlstring(self.state, name.as_ptr().cast(), name.len());
            ffi::lua_rawget(self.state, -2);
            let taken = !ffi::lua_isnil(self.state, -1);
            ffi::lua_pop(self.state, 2);
            taken
        }
    }

    fn assert_member_free(&self, type_name: &str, name: &str, kind: MemberKind) -> Result<()> {
        // A getter and a setter of the same name are a pair; everything else must be unique.
        let paired = match kind {
            MemberKind::Getter => self.setters.as_ref(),
            MemberKind::Setter => self.getters.as_ref(),
            MemberKind::Method => None,
        };
        for table in [self.methods.as_ref(), self.getters.as_ref(), self.setters.as_ref()].into_iter().flatten() {
            if paired.is_some_and(|p| std::ptr::eq(p, table)) {
                continue;
            }
            if self.table_has_key(table, name) {
                return Err(Error::logic(format!("{type_name}.{name} already registered")));
            }
        }
        Ok(())
    }

    fn new_table(&self) -> Result<Value> {
        unsafe {
            ffi::lua_newtable(self.state);
            let value = Value::store(crate::stack::ValueView::resolve(self.state, -1))?;
            ffi::lua_pop(self.state, 1);
            Ok(value)
        }
    }

    fn store_in(&self, table: &Value, name: &str, closure: &Value) {
        unsafe {
            ffi::lua_getref(self.state, table.reference_id());
            ffi::lua_pushlstring(self.state, name.as_ptr().cast(), name.len());
            ffi::lua_getref(self.state, closure.reference_id());
            ffi::lua_rawset(self.state, -3);
            ffi::lua_pop(self.state, 1);
        }
    }

    fn assert_generated_index_installable(&self, allow_owned_methods_table: bool) -> Result<()> {
        if self.has_explicit_index {
            return Err(Error::logic("Explicit __index conflicts with generated member dispatch"));
        }
        unsafe {
            ffi::lua_rawgetfield(self.state, self.metatable, c"__index".as_ptr());
            let mut owned = false;
            if allow_owned_methods_table && let Some(methods) = &self.methods {
                ffi::lua_getref(self.state, methods.reference_id());
                owned = ffi::lua_rawequal(self.state, -1, -2) != 0;
                ffi::lua_pop(self.state, 1);
            }
            let occupied = !ffi::lua_isnil(self.state, -1) && !owned;
            ffi::lua_pop(self.state, 1);
            if occupied {
                return Err(Error::logic("Existing __index conflicts with generated member dispatch"));
            }
        }
        if !self.rawget_field_is_nil(c"__namecall") {
            return Err(Error::logic("Existing __namecall conflicts with generated member dispatch"));
        }
        Ok(())
    }

    fn assert_generated_newindex_installable(&self) -> Result<()> {
        if self.has_explicit_newindex {
            return Err(Error::logic("Explicit __newindex conflicts with generated member dispatch"));
        }
        if !self.rawget_field_is_nil(c"__newindex") {
            return Err(Error::logic("Existing __newindex conflicts with generated member dispatch"));
        }
        Ok(())
    }

    fn register_member(
        &mut self,
        kind: MemberKind,
        type_name: &str,
        name: &str,
        closure: Value,
        entry: Option<MemberEntry>,
    ) -> Result<()> {
        let getters_were_absent = self.getters.is_none();
        let setters_were_absent = self.setters.is_none();
        match kind {
            MemberKind::Method if getters_were_absent => self.assert_generated_index_installable(true)?,
            MemberKind::Getter if getters_were_absent => self.assert_generated_index_installable(true)?,
            MemberKind::Setter if setters_were_absent => self.assert_generated_newindex_installable()?,
            _ => {}
        }
        let missing = match kind {
            MemberKind::Method => self.methods.is_none(),
            MemberKind::Getter => self.getters.is_none(),
            MemberKind::Setter => self.setters.is_none(),
        };
        if missing {
            let table = self.new_table()?;
            match kind {
                MemberKind::Method => self.methods = Some(table),
                MemberKind::Getter => self.getters = Some(table),
                MemberKind::Setter => self.setters = Some(table),
            }
        }
        self.assert_member_free(type_name, name, kind)?;
        let target = match kind {
            MemberKind::Method => self.methods.as_ref(),
            MemberKind::Getter => self.getters.as_ref(),
            MemberKind::Setter => self.setters.as_ref(),
        }
        .expect("created above");
        self.store_in(target, name, &closure);

        // The dispatch tables: entries for bound members, the function itself otherwise.
        match kind {
            MemberKind::Method => {
                if self.method_entries.is_none() {
                    self.method_entries = Some(self.new_table()?);
                }
                let entries = self.method_entries.clone().expect("created above");
                self.store_member(&entries, name, &closure, entry);
            }
            MemberKind::Setter => {
                if self.setter_entries.is_none() {
                    self.setter_entries = Some(self.new_table()?);
                }
                let entries = self.setter_entries.clone().expect("created above");
                self.store_member(&entries, name, &closure, entry);
            }
            MemberKind::Getter => {
                if self.members.is_none() {
                    self.members = Some(self.members_from_methods()?);
                }
            }
        }
        if let Some(members) = self.members.clone() {
            match kind {
                MemberKind::Method => self.store_in(&members, name, &closure),
                MemberKind::Getter => self.store_member(&members, name, &closure, entry),
                MemberKind::Setter => {}
            }
        }

        match kind {
            MemberKind::Method if self.getters.is_none() => {
                // Phase A: the plain methods table is __index.
                let methods = self.methods.clone().expect("methods table exists");
                self.set_field("__index", &methods)?;
            }
            MemberKind::Method => self.register_method_atom(name, &closure, entry)?,
            MemberKind::Getter if getters_were_absent => self.install_index_and_namecall_dispatchers(type_name)?,
            MemberKind::Setter if setters_were_absent => self.install_newindex_dispatcher(type_name)?,
            _ => {}
        }
        Ok(())
    }

    /// A new `members` table seeded with every method function registered so far.
    fn members_from_methods(&self) -> Result<Value> {
        let members = self.new_table()?;
        if let Some(methods) = &self.methods {
            unsafe {
                ffi::lua_getref(self.state, members.reference_id());
                ffi::lua_getref(self.state, methods.reference_id());
                let (target, source) = (ffi::lua_gettop(self.state) - 1, ffi::lua_gettop(self.state));
                let mut iterator: c_int = 0;
                loop {
                    iterator = ffi::lua_rawiter(self.state, source, iterator);
                    if iterator < 0 {
                        break;
                    }
                    // rawiter pushed key, value: copy the key beneath and raw-set into `target`.
                    ffi::lua_pushvalue(self.state, -2);
                    ffi::lua_insert(self.state, -2);
                    ffi::lua_rawset(self.state, target);
                    ffi::lua_pop(self.state, 1);
                }
                ffi::lua_pop(self.state, 2);
            }
        }
        Ok(members)
    }

    /// Methods whose name has a Luau atom are also indexed by atom for `__namecall`.
    fn register_method_atom(&mut self, name: &str, closure: &Value, entry: Option<MemberEntry>) -> Result<()> {
        let atom = unsafe {
            ffi::lua_pushlstring(self.state, name.as_ptr().cast(), name.len());
            let mut atom: c_int = -1;
            ffi::lua_tostringatom(self.state, -1, &mut atom);
            ffi::lua_pop(self.state, 1);
            atom
        };
        if atom < 0 {
            return Ok(());
        }
        if self.method_atoms.is_none() {
            self.method_atoms = Some(self.new_table()?);
        }
        let atoms = self.method_atoms.clone().expect("created above");
        unsafe {
            ffi::lua_getref(self.state, atoms.reference_id());
            self.push_member(closure, entry);
            ffi::lua_rawseti(self.state, -2, atom);
            ffi::lua_pop(self.state, 1);
        }
        Ok(())
    }

    /// Phase B: the `members` table (method functions, getter entries) behind a generated
    /// `__index`, and a generated `__namecall` over the method entries and their atom index.
    fn install_index_and_namecall_dispatchers(&mut self, type_name: &str) -> Result<()> {
        if self.methods.is_none() {
            self.methods = Some(self.new_table()?);
        }
        if self.method_entries.is_none() {
            self.method_entries = Some(self.new_table()?);
        }
        if self.method_atoms.is_none() {
            self.method_atoms = Some(self.new_table()?);
        }
        if self.getters.is_none() {
            self.getters = Some(self.new_table()?);
        }
        let members = match &self.members {
            Some(members) => members.clone(),
            None => {
                let members = self.members_from_methods()?;
                self.members = Some(members.clone());
                members
            }
        };
        // Methods registered before the first getter have entries by name but no atoms yet.
        let entries = self.method_entries.clone().expect("method entries table exists");
        let atoms = self.method_atoms.clone().expect("atoms table exists");
        unsafe {
            ffi::lua_getref(self.state, atoms.reference_id());
            ffi::lua_getref(self.state, entries.reference_id());
            let (atoms_index, source) = (ffi::lua_gettop(self.state) - 1, ffi::lua_gettop(self.state));
            let mut iterator: c_int = 0;
            loop {
                iterator = ffi::lua_rawiter(self.state, source, iterator);
                if iterator < 0 {
                    break;
                }
                let mut atom: c_int = -1;
                ffi::lua_tostringatom(self.state, -2, &mut atom);
                if atom >= 0 {
                    ffi::lua_rawseti(self.state, atoms_index, atom);
                    ffi::lua_pop(self.state, 1);
                } else {
                    ffi::lua_pop(self.state, 2);
                }
            }
            ffi::lua_pop(self.state, 2);
        }

        let index_name = self.retain(&format!("{type_name}.__index"))?;
        let namecall_name = self.retain(&format!("{type_name}.__namecall"))?;
        let getters = self.getters.clone().expect("getters table exists");
        unsafe {
            ffi::lua_getref(self.state, members.reference_id());
            ffi::lua_getref(self.state, getters.reference_id());
            ffi::lua_pushcclosure(self.state, dispatch::index, index_name, 2);
            ffi::lua_rawsetfield(self.state, self.metatable, c"__index".as_ptr());

            ffi::lua_getref(self.state, entries.reference_id());
            ffi::lua_pushlstring(self.state, type_name.as_ptr().cast(), type_name.len());
            ffi::lua_getref(self.state, atoms.reference_id());
            ffi::lua_pushcclosure(self.state, dispatch::namecall, namecall_name, 3);
            ffi::lua_rawsetfield(self.state, self.metatable, c"__namecall".as_ptr());
        }
        Ok(())
    }

    fn install_newindex_dispatcher(&mut self, type_name: &str) -> Result<()> {
        let setters = self.setters.clone().expect("setters table exists");
        let entries = self.setter_entries.clone().expect("setter entries table exists");
        let debug_name = self.retain(&format!("{type_name}.__newindex"))?;
        unsafe {
            ffi::lua_getref(self.state, entries.reference_id());
            ffi::lua_pushlstring(self.state, type_name.as_ptr().cast(), type_name.len());
            ffi::lua_getref(self.state, setters.reference_id());
            ffi::lua_pushcclosure(self.state, dispatch::newindex, debug_name, 3);
            ffi::lua_rawsetfield(self.state, self.metatable, c"__newindex".as_ptr());
        }
        Ok(())
    }

    // ---------------------------------------------------------------------------------------
    // Native methods: hand-written C functions with an integer discriminator upvalue
    // ---------------------------------------------------------------------------------------

    pub fn begin_native_methods(&mut self) -> Result<()> {
        if self.native_methods.is_some() {
            return Err(Error::logic("Native method registration already started"));
        }
        if self.has_explicit_index {
            return Err(Error::logic("Native methods cannot be added after an explicit __index"));
        }
        if self.methods.is_some() || self.getters.is_some() || self.setters.is_some() {
            return Err(Error::logic("Native methods cannot be mixed with registered methods/properties"));
        }
        self.native_methods = Some(self.new_table()?);
        Ok(())
    }

    /// Adds `body` under `name` with `discriminator` as upvalue 1, named `<__type>.<name>`.
    pub fn add_native_method(&mut self, name: &str, body: ffi::lua_CFunction, discriminator: c_int) -> Result<()> {
        let Some(table) = self.native_methods.clone() else {
            return Err(Error::logic("Call beginNativeMethods before addNativeMethod"));
        };
        if self.native_methods_frozen {
            return Err(Error::logic(format!("Native method table is frozen; cannot add '{name}'")));
        }
        if name.is_empty() || name.contains('\0') {
            return Err(Error::logic("Native method name must be a non-empty C string"));
        }
        if self.table_has_key(&table, name) {
            return Err(Error::logic(format!("Duplicate native method '{name}'")));
        }
        let debug_name = self.retain(&format!("{}.{name}", self.type_name()?))?;
        unsafe {
            ffi::lua_getref(self.state, table.reference_id());
            ffi::lua_pushlstring(self.state, name.as_ptr().cast(), name.len());
            ffi::lua_pushinteger(self.state, discriminator);
            ffi::lua_pushcclosure(self.state, body, debug_name, 1);
            ffi::lua_rawset(self.state, -3);
            ffi::lua_pop(self.state, 1);
        }
        Ok(())
    }

    fn freeze_native_methods(&mut self) {
        if self.native_methods_frozen {
            return;
        }
        if let Some(table) = &self.native_methods {
            unsafe {
                ffi::lua_getref(self.state, table.reference_id());
                ffi::lua_setreadonly(self.state, -1, 1);
                ffi::lua_pop(self.state, 1);
            }
        }
        self.native_methods_frozen = true;
    }

    /// Freezes the native methods table and installs it as `__index`.
    pub fn install_native_method_index(&mut self) -> Result<()> {
        let Some(table) = self.native_methods.clone() else {
            return Err(Error::logic("Call beginNativeMethods before installNativeMethodIndex"));
        };
        self.assert_explicit_index_installable()?;
        self.freeze_native_methods();
        self.set_field("__index", &table)?;
        self.has_explicit_index = true;
        Ok(())
    }

    /// The frozen native methods table.
    pub fn frozen_native_methods(&mut self) -> Result<Value> {
        if self.native_methods.is_none() {
            return Err(Error::logic("Call beginNativeMethods before requesting native methods"));
        }
        self.freeze_native_methods();
        Ok(self.native_methods.clone().expect("checked above"))
    }

    /// The call context for hand-written native method bodies: `Call::upvalue(1)` is the
    /// discriminator passed to [`MetatableBuilder::add_native_method`].
    pub fn native_discriminator(call: &Call<'_>) -> c_int {
        call.upvalue(1).read::<i32>().unwrap_or(0)
    }
}
