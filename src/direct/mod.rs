//! Luau direct userdata access (`components/luau/directuserdata.*`, `userdatadispatch.hpp`,
//! `components/lua/directuserdata.hpp`).
//!
//! Luau can call a native callback straight from `GETTABLEKS`/`SETTABLEKS`/`NAMECALL` for a
//! tagged userdata when the key is an interned string with an *atom*, skipping the metatable
//! walk and the Lua call frame. Three pieces make that work:
//!
//! - an [`AtomCatalogue`] installed as `lua_Callbacks.useratom`, giving selected strings stable
//!   small integer ids;
//! - a [`registry::Registry`] resolving `(tag, access kind, atom)` to a slot descriptor, with
//!   Luau's per-instruction 16-bit cache validated before use;
//! - per-tag direct callbacks registered with `lua_registeruserdatadirectaccess`, which Luau
//!   runs inside a C frame whose function slot is the *stored metamethod*. The binder installs
//!   wrapper metamethods that keep the original as upvalue 1, so both the direct path and the
//!   ordinary metamethod path run the same typed handler and can fall back to the original.
//!
//! Direct fields ([`field`]) are separate: a per-field getter that writes straight into the
//! destination register with no frame at all.

pub mod field;
pub mod registry;

use std::collections::HashMap;
use std::ffi::{CStr, c_char, c_int, c_void};
use std::sync::OnceLock;

use crate::bind::Call;
use crate::error::{Error, Result};
use crate::raw::{ffi, trampoline};
use crate::runtime::Runtime;
use crate::userdata::metatable::MetatableBuilder;
use crate::userdata::{RuntimeTag, Userdata};

/// A Luau string atom: a small stable id assigned when the string is interned.
pub type Atom = i16;

/// The atom Luau reports for strings outside the catalogue.
pub const UNKNOWN_ATOM: Atom = -1;

/// Which metamethod a direct callback stands in for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum AccessKind {
    Index = 0,
    NewIndex = 1,
    Namecall = 2,
}

pub const ACCESS_KIND_COUNT: usize = 3;

/// Interned member names eligible for direct dispatch. Names and atoms are host data; the
/// binder only requires them to be unique and non-negative.
#[derive(Debug)]
pub struct AtomCatalogue {
    entries: &'static [(&'static str, Atom)],
}

impl AtomCatalogue {
    /// Validates the catalogue at compile time: non-empty names, unique names, unique
    /// non-negative atoms. The error is a static message so the function stays `const`.
    pub const fn try_new(entries: &'static [(&'static str, Atom)]) -> std::result::Result<AtomCatalogue, &'static str> {
        let mut i = 0;
        while i < entries.len() {
            let (name, atom) = entries[i];
            if name.is_empty() {
                return Err("atom catalogue has an empty name");
            }
            if atom < 0 {
                return Err("atom catalogue has a negative atom");
            }
            let mut j = 0;
            while j < i {
                let (other_name, other_atom) = entries[j];
                if other_atom == atom {
                    return Err("atom catalogue has a duplicate atom");
                }
                if const_str_eq(other_name, name) {
                    return Err("atom catalogue has a duplicate name");
                }
                j += 1;
            }
            i += 1;
        }
        Ok(AtomCatalogue { entries })
    }

    /// Validates the catalogue, reporting a logic error.
    pub fn new(entries: &'static [(&'static str, Atom)]) -> Result<AtomCatalogue> {
        AtomCatalogue::try_new(entries).map_err(Error::logic)
    }

    /// Like [`AtomCatalogue::new`] but panics at compile time on an invalid catalogue, for use
    /// in a `const` item.
    pub const fn validated(entries: &'static [(&'static str, Atom)]) -> AtomCatalogue {
        match AtomCatalogue::try_new(entries) {
            Ok(catalogue) => catalogue,
            Err(message) => panic!("{}", message),
        }
    }

    pub fn entries(&self) -> &'static [(&'static str, Atom)] {
        self.entries
    }

    /// The atom for `name`, if catalogued.
    pub fn atom_of(&self, name: &str) -> Option<Atom> {
        self.entries.iter().find(|(entry, _)| *entry == name).map(|(_, atom)| *atom)
    }

    /// The spelling of `atom`, if catalogued.
    pub fn name_of(&self, atom: Atom) -> Option<&'static str> {
        self.entries.iter().find(|(_, entry)| *entry == atom).map(|(name, _)| *name)
    }
}

const fn const_str_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    let mut i = 0;
    while i < a.len() {
        if a[i] != b[i] {
            return false;
        }
        i += 1;
    }
    true
}

/// `useratom` receives no context pointer, so the installed catalogue is process-global, as in
/// the C++ binder. A process has exactly one.
static INSTALLED: OnceLock<(&'static AtomCatalogue, HashMap<&'static [u8], Atom>)> = OnceLock::new();

/// The Luau callback: no Lua API, no panics; a hash lookup on the interned bytes.
unsafe extern "C" fn user_atom(_: *mut ffi::lua_State, text: *const c_char, length: usize) -> i16 {
    let Some((_, table)) = INSTALLED.get() else { return UNKNOWN_ATOM };
    // SAFETY: Luau passes the string's bytes and length.
    let bytes = unsafe { std::slice::from_raw_parts(text.cast::<u8>(), length) };
    table.get(bytes).copied().unwrap_or(UNKNOWN_ATOM)
}

/// Installs `catalogue` as the VM's `useratom` callback and verifies every spelling resolves.
///
/// Luau 0.740 resolves atoms lazily (`luaS_updateatom`), but OpenMW installs the callback
/// before `luaL_openlibs` and the binder keeps that order: call this on a runtime built with
/// `standard_libraries(false)`, then open them. Installing a different catalogue in the same
/// process, or over a different `useratom`, is a logic error.
pub fn install_atom_callback(runtime: &Runtime, catalogue: &'static AtomCatalogue) -> Result<()> {
    let (installed, _) = INSTALLED.get_or_init(|| {
        let table = catalogue.entries.iter().map(|(name, atom)| (name.as_bytes(), *atom)).collect();
        (catalogue, table)
    });
    if !std::ptr::eq(*installed, catalogue) {
        return Err(Error::logic("A different atom catalogue is already installed in this process"));
    }
    let stack = runtime.stack();
    let state = stack.state_ptr();
    // SAFETY: lua_callbacks returns the VM's callback block; only useratom is touched.
    unsafe {
        let callbacks = ffi::lua_callbacks(state);
        if let Some(existing) = (*callbacks).useratom
            && !std::ptr::fn_addr_eq(
                existing,
                user_atom as unsafe extern "C" fn(*mut ffi::lua_State, *const c_char, usize) -> i16,
            )
        {
            return Err(Error::logic("A different Luau useratom callback is already installed"));
        }
        (*callbacks).useratom = Some(user_atom);
    }
    stack.with_frame(|frame| {
        for (name, atom) in catalogue.entries {
            let view = frame.push_string(name);
            let mut resolved: c_int = UNKNOWN_ATOM as c_int;
            // SAFETY: the string is on the frame.
            let text = unsafe { ffi::lua_tostringatom(frame.state(), view.index(), &mut resolved) };
            if text.is_null() || resolved != c_int::from(*atom) {
                return Err(Error::logic(format!("Luau useratom callback did not resolve '{name}' to atom {atom}")));
            }
        }
        Ok(())
    })
}

/// The installed catalogue, if any.
pub fn installed_catalogue() -> Option<&'static AtomCatalogue> {
    INSTALLED.get().map(|(catalogue, _)| *catalogue)
}

/// The atom of the string at `view`, resolving it now if Luau has not yet.
pub fn atom_of_view(view: crate::stack::ValueView<'_>) -> Option<Atom> {
    if !view.is_string() {
        return None;
    }
    let mut atom: c_int = UNKNOWN_ATOM as c_int;
    // SAFETY: the slot holds a string.
    unsafe { ffi::lua_tostringatom(view.state(), view.index(), &mut atom) };
    (atom >= 0).then_some(atom as Atom)
}

// ---------------------------------------------------------------------------------------------
// Typed direct callbacks
// ---------------------------------------------------------------------------------------------

/// Outcome of a direct `__index`/`__newindex` handler.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Dispatch {
    /// The handler produced the result (exactly one pushed value for index, none for newindex).
    Handled,
    /// Let the original metamethod answer.
    Fallback,
}

/// Direct member access for a tagged userdata type.
///
/// Handlers run inside Luau's direct-access C frame: for index the stack is `[ud, key]`, for
/// newindex `[ud, key, value]`, for namecall `[ud, args...]` with `lua_namecallatom` valid.
/// `slot` is Luau's per-instruction 16-bit cache, shared between every userdata type and
/// starting at 0; validate it with [`registry::Registry::resolve_cached_slot`] before trusting
/// it. Errors are raised into Luau; panics abort.
pub trait DirectAccess: Userdata {
    fn direct_index(call: &Call<'_>, data: &Self, atom: Atom, slot: &mut u16) -> Result<Dispatch> {
        let _ = (call, data, atom, slot);
        Ok(Dispatch::Fallback)
    }

    fn direct_newindex(call: &Call<'_>, data: &Self, atom: Atom, slot: &mut u16) -> Result<Dispatch> {
        let _ = (call, data, atom, slot);
        Ok(Dispatch::Fallback)
    }

    /// `Some(result_count)` when handled, `None` to fall back to the original `__namecall`.
    fn direct_namecall(call: &Call<'_>, data: &Self, atom: Atom, slot: &mut u16) -> Result<Option<c_int>> {
        let _ = (call, data, atom, slot);
        Ok(None)
    }
}

/// The payload for a direct callback: `data` is the userdata payload Luau passed and `utag` its
/// tag, which must be `T`'s.
unsafe fn payload<'a, T: Userdata>(data: *mut c_void, utag: c_int) -> Result<&'a T> {
    if T::TAG.map(c_int::from) != Some(utag) || data.is_null() {
        return Err(Error::logic(format!("direct callback for '{}' received userdata of tag {utag}", T::NAME)));
    }
    // SAFETY: the tag matched, so Luau's payload is a T written by tagged::push.
    Ok(unsafe { &*data.cast::<T>() })
}

/// Original `__index` fallback: upvalue 1 of the running wrapper is the original metamethod. A
/// table is raw-indexed; a function is called with `(ud, key)`.
unsafe fn index_fallback(state: *mut ffi::lua_State) -> Result<c_int> {
    unsafe {
        if ffi::lua_istable(state, ffi::lua_upvalueindex(1)) {
            ffi::lua_pushvalue(state, 2);
            ffi::lua_rawget(state, ffi::lua_upvalueindex(1));
            return Ok(1);
        }
        call_original(state)
    }
}

/// Calls the original metamethod (upvalue 1) with the frame's arguments, forwarding every result.
unsafe fn call_original(state: *mut ffi::lua_State) -> Result<c_int> {
    unsafe {
        let call = Call::from_raw(state);
        let argument_count = call.argument_count();
        if !ffi::lua_isfunction(state, ffi::lua_upvalueindex(1)) {
            return Err(Error::logic("direct dispatch fallback has no original metamethod"));
        }
        ffi::lua_settop(state, argument_count);
        ffi::lua_checkstack(state, argument_count + 1);
        ffi::lua_pushvalue(state, ffi::lua_upvalueindex(1));
        for index in 1..=argument_count {
            ffi::lua_pushvalue(state, index);
        }
        if ffi::lua_pcall(state, argument_count, ffi::LUA_MULTRET, 0) != ffi::LUA_OK {
            return Err(Error::LuaErrorOnStack);
        }
        Ok(ffi::lua_gettop(state) - argument_count)
    }
}

/// `lua_UserdataDirectAccess` for `__index`.
///
/// # Safety
/// Called only by Luau's direct-access VM path (or through [`register`]) for userdata of `T`'s
/// tag; `state` is the live thread and `slot` is Luau's cache word or null.
pub unsafe extern "C-unwind" fn index_callback<T: DirectAccess>(
    state: *mut ffi::lua_State,
    data: *mut c_void,
    atom: c_int,
    slot: *mut u16,
    utag: c_int,
) {
    unsafe {
        trampoline::enter(state, || {
            let call = Call::from_raw(state);
            let payload = payload::<T>(data, utag)?;
            let mut cached = if slot.is_null() { 0 } else { *slot };
            let outcome = T::direct_index(&call, payload, atom as Atom, &mut cached)?;
            if !slot.is_null() {
                *slot = cached;
            }
            match outcome {
                Dispatch::Handled => Ok(1),
                Dispatch::Fallback => index_fallback(state),
            }
        });
    }
}

/// `lua_UserdataDirectAccess` for `__newindex`.
///
/// # Safety
/// Called only by Luau's direct-access VM path (or through [`register`]) for userdata of `T`'s
/// tag; `state` is the live thread and `slot` is Luau's cache word or null.
pub unsafe extern "C-unwind" fn newindex_callback<T: DirectAccess>(
    state: *mut ffi::lua_State,
    data: *mut c_void,
    atom: c_int,
    slot: *mut u16,
    utag: c_int,
) {
    unsafe {
        trampoline::enter(state, || {
            let call = Call::from_raw(state);
            let payload = payload::<T>(data, utag)?;
            let mut cached = if slot.is_null() { 0 } else { *slot };
            let outcome = T::direct_newindex(&call, payload, atom as Atom, &mut cached)?;
            if !slot.is_null() {
                *slot = cached;
            }
            match outcome {
                Dispatch::Handled => Ok(0),
                Dispatch::Fallback => {
                    call_original(state)?;
                    Ok(0)
                }
            }
        });
    }
}

/// `lua_UserdataDirectNamecall`.
///
/// # Safety
/// Called only by Luau's direct-access VM path (or through [`register`]) for userdata of `T`'s
/// tag; `state` is the live thread and `slot` is Luau's cache word or null.
pub unsafe extern "C-unwind" fn namecall_callback<T: DirectAccess>(
    state: *mut ffi::lua_State,
    data: *mut c_void,
    atom: c_int,
    slot: *mut u16,
    utag: c_int,
) -> c_int {
    unsafe {
        trampoline::enter(state, || {
            let call = Call::from_raw(state);
            let payload = payload::<T>(data, utag)?;
            let mut cached = if slot.is_null() { 0 } else { *slot };
            let outcome = T::direct_namecall(&call, payload, atom as Atom, &mut cached)?;
            if !slot.is_null() {
                *slot = cached;
            }
            match outcome {
                Some(results) => Ok(results),
                None => call_original(state),
            }
        })
    }
}

// ---------------------------------------------------------------------------------------------
// Ordinary metamethod wrappers: same handler, entered through the metatable
// ---------------------------------------------------------------------------------------------

/// `__index` wrapper: string keys with an atom go to the typed handler (a fresh cache slot each
/// time, since ordinary metamethod calls have no instruction cache); anything else falls back.
///
/// # Safety
/// Installed only by [`MetatableBuilder::direct_dispatch`] with the original metamethod as
/// upvalue 1; `state` is the live thread of an ordinary metamethod call.
pub unsafe extern "C-unwind" fn index_wrapper<T: DirectAccess>(state: *mut ffi::lua_State) -> c_int {
    unsafe {
        trampoline::enter(state, || {
            let Some(tag) = T::TAG else { return index_fallback(state) };
            let data = ffi::lua_touserdatatagged(state, 1, c_int::from(tag));
            if data.is_null() || ffi::lua_type(state, 2) != ffi::LUA_TSTRING {
                return index_fallback(state);
            }
            let mut atom: c_int = -1;
            ffi::lua_tostringatom(state, 2, &mut atom);
            if atom < 0 {
                return index_fallback(state);
            }
            let call = Call::from_raw(state);
            let mut slot: u16 = 0;
            match T::direct_index(&call, &*data.cast::<T>(), atom as Atom, &mut slot)? {
                Dispatch::Handled => Ok(1),
                Dispatch::Fallback => index_fallback(state),
            }
        })
    }
}

/// `__newindex` wrapper; see [`index_wrapper`].
///
/// # Safety
/// Installed only by [`MetatableBuilder::direct_dispatch`] with the original metamethod as
/// upvalue 1; `state` is the live thread of an ordinary metamethod call.
pub unsafe extern "C-unwind" fn newindex_wrapper<T: DirectAccess>(state: *mut ffi::lua_State) -> c_int {
    unsafe {
        trampoline::enter(state, || {
            let Some(tag) = T::TAG else { return call_original(state).map(|_| 0) };
            let data = ffi::lua_touserdatatagged(state, 1, c_int::from(tag));
            if data.is_null() || ffi::lua_type(state, 2) != ffi::LUA_TSTRING {
                return call_original(state).map(|_| 0);
            }
            let mut atom: c_int = -1;
            ffi::lua_tostringatom(state, 2, &mut atom);
            if atom < 0 {
                return call_original(state).map(|_| 0);
            }
            let call = Call::from_raw(state);
            let mut slot: u16 = 0;
            match T::direct_newindex(&call, &*data.cast::<T>(), atom as Atom, &mut slot)? {
                Dispatch::Handled => Ok(0),
                Dispatch::Fallback => call_original(state).map(|_| 0),
            }
        })
    }
}

/// `__namecall` wrapper; see [`index_wrapper`].
///
/// # Safety
/// Installed only by [`MetatableBuilder::direct_dispatch`] with the original metamethod as
/// upvalue 1; `state` is the live thread of an ordinary metamethod call.
pub unsafe extern "C-unwind" fn namecall_wrapper<T: DirectAccess>(state: *mut ffi::lua_State) -> c_int {
    unsafe {
        trampoline::enter(state, || {
            let Some(tag) = T::TAG else { return call_original(state) };
            let data = ffi::lua_touserdatatagged(state, 1, c_int::from(tag));
            let mut atom: c_int = -1;
            let name = ffi::lua_namecallatom(state, &mut atom);
            if data.is_null() || name.is_null() || atom < 0 {
                return call_original(state);
            }
            let call = Call::from_raw(state);
            let mut slot: u16 = 0;
            match T::direct_namecall(&call, &*data.cast::<T>(), atom as Atom, &mut slot)? {
                Some(results) => Ok(results),
                None => call_original(state),
            }
        })
    }
}

/// Which of the three metamethods a type dispatches directly.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DirectMetamethods {
    pub index: bool,
    pub newindex: bool,
    pub namecall: bool,
}

impl DirectMetamethods {
    pub const ALL: DirectMetamethods = DirectMetamethods { index: true, newindex: true, namecall: true };
    pub const INDEX: DirectMetamethods = DirectMetamethods { index: true, newindex: false, namecall: false };
    pub const NAMECALL: DirectMetamethods = DirectMetamethods { index: false, newindex: false, namecall: true };

    fn any(self) -> bool {
        self.index || self.newindex || self.namecall
    }
}

impl MetatableBuilder<'_> {
    /// Installs the direct-dispatch wrappers over the metamethods already present, keeping each
    /// original as the wrapper's upvalue 1. Call after every method/property/metamethod
    /// registration for `T`, and register the VM callbacks with [`register`] afterwards.
    pub fn direct_dispatch<T: DirectAccess>(&mut self, which: DirectMetamethods) -> Result<()> {
        if !which.any() {
            return Err(Error::logic("Direct userdata access requires a callback"));
        }
        let type_name = self.type_name()?;
        if type_name != T::NAME {
            return Err(Error::logic(format!("Direct dispatch for '{}' on a '{type_name}' metatable", T::NAME)));
        }
        if which.index {
            self.install_wrapper(c"__index", index_wrapper::<T>, &format!("{type_name}.__index"), true)?;
        }
        if which.newindex {
            self.install_wrapper(c"__newindex", newindex_wrapper::<T>, &format!("{type_name}.__newindex"), false)?;
        }
        if which.namecall {
            self.install_wrapper(c"__namecall", namecall_wrapper::<T>, &format!("{type_name}.__namecall"), false)?;
        }
        Ok(())
    }

    fn install_wrapper(
        &mut self,
        name: &CStr,
        wrapper: ffi::lua_CFunction,
        debug_name: &str,
        allow_table: bool,
    ) -> Result<()> {
        let state = self.state_ptr();
        let retained = self.retain_name(debug_name)?;
        // SAFETY: the original metamethod is read into the frame and becomes the wrapper's
        // only upvalue; the wrapper replaces it in the metatable.
        unsafe {
            self.push_metatable_field(name);
            let kind = ffi::lua_type(state, -1);
            let acceptable = kind == ffi::LUA_TFUNCTION || (allow_table && kind == ffi::LUA_TTABLE);
            if !acceptable {
                ffi::lua_pop(state, 1);
                return Err(Error::logic(format!(
                    "Shared userdata dispatch requires an original {} metamethod",
                    name.to_string_lossy()
                )));
            }
            ffi::lua_pushcclosure(state, wrapper, retained, 1);
            self.raw_set_metatable_top(name);
        }
        Ok(())
    }
}

/// Registers `T`'s direct callbacks with the VM for the metamethods in `which`.
///
/// Requires `T` registered with a read-only metatable whose corresponding metamethods are the
/// wrappers installed by [`MetatableBuilder::direct_dispatch`]; anything else is a logic error,
/// because a fallback would otherwise re-enter the callback.
pub fn register<T: DirectAccess>(runtime: &Runtime, which: DirectMetamethods) -> Result<()> {
    if !which.any() {
        return Err(Error::logic("Direct userdata access requires a callback"));
    }
    let Some(tag) = T::TAG else {
        return Err(Error::logic(format!("'{}' is untagged; direct access needs a runtime tag", T::NAME)));
    };
    require_tag(tag)?;
    let stack = runtime.stack();
    stack.with_frame(|frame| {
        let state = frame.state();
        // SAFETY: balanced pushes within the frame.
        unsafe {
            ffi::lua_getuserdatametatable(state, c_int::from(tag));
            if !ffi::lua_istable(state, -1) {
                return Err(Error::logic("Luau userdata tag has no registered metatable"));
            }
            if ffi::lua_getreadonly(state, -1) == 0 {
                return Err(Error::logic("Luau userdata metatable must be read-only"));
            }
            let metatable = ffi::lua_gettop(state);
            let check = |name: &CStr, wrapper: ffi::lua_CFunction| -> Result<()> {
                ffi::lua_rawgetfield(state, metatable, name.as_ptr());
                let is_ours = ffi::lua_tocfunction(state, -1).is_some_and(|f| std::ptr::fn_addr_eq(f, wrapper));
                ffi::lua_pop(state, 1);
                if !is_ours {
                    return Err(Error::logic(format!(
                        "Luau userdata direct callback for {} has no corresponding wrapper metamethod",
                        name.to_string_lossy()
                    )));
                }
                Ok(())
            };
            if which.index {
                check(c"__index", index_wrapper::<T>)?;
            }
            if which.newindex {
                check(c"__newindex", newindex_wrapper::<T>)?;
            }
            if which.namecall {
                check(c"__namecall", namecall_wrapper::<T>)?;
            }
            let registered = ffi::lua_registeruserdatadirectaccess(
                state,
                c_int::from(tag),
                which.index.then_some(index_callback::<T> as ffi::lua_UserdataDirectAccess),
                which.newindex.then_some(newindex_callback::<T> as ffi::lua_UserdataDirectAccess),
                which.namecall.then_some(namecall_callback::<T> as ffi::lua_UserdataDirectNamecall),
            );
            if registered == 0 {
                return Err(Error::logic("Luau userdata direct access registration failed"));
            }
        }
        Ok(())
    })
}

pub(crate) fn require_tag(tag: RuntimeTag) -> Result<()> {
    if tag == 0 || tag >= crate::TAG_LIMIT {
        return Err(Error::logic("Luau userdata tag is outside the supported range"));
    }
    Ok(())
}
