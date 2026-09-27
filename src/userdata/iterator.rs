//! `__iter` factories (`binding.hpp`: `makeArrayIterator`, `makeKeyedIterator`,
//! `makeCursorIterator`).
//!
//! - A *stateless* iterator reuses the iterated object as the generic-for state and a number
//!   (array) or nil (keyed) as the initial control; the `next` function is shared by every loop.
//! - A *cursor* iterator gives each loop its own private cursor userdata, so nested and
//!   concurrent loops over one object never share state. The cursor is only reachable as the
//!   loop's state value.
//!
//! The `next` function is kept as a real Lua upvalue of the factory closure, never captured in
//! Rust: a pinned `Value` captured in a Lua-owned closure would run `lua_unref` from the
//! collector's destructor, which the GC contract forbids.

use std::ffi::{c_int, c_void};
use std::marker::PhantomData;
use std::ops::Deref;
use std::ptr;

use super::type_key;
use crate::bind::{Binding, Call, Param, ParamItem, ParamKind, Return, StackResults, function_closure};
use crate::error::{Error, Result};
use crate::raw::{ffi, trampoline};
use crate::stack::{Scope, ValueView};

// ---------------------------------------------------------------------------------------------
// Stateless factories: upvalue 1 is the next function
// ---------------------------------------------------------------------------------------------

unsafe extern "C-unwind" fn stateless_factory<const INITIAL_NIL: bool>(state: *mut ffi::lua_State) -> c_int {
    unsafe {
        ffi::lua_pushvalue(state, ffi::lua_upvalueindex(1));
        ffi::lua_pushvalue(state, 1);
        if INITIAL_NIL {
            ffi::lua_pushnil(state);
        } else {
            ffi::lua_pushnumber(state, 0.0);
        }
        3
    }
}

// ---------------------------------------------------------------------------------------------
// Cursor userdata
// ---------------------------------------------------------------------------------------------

/// The private per-loop state of a cursor iterator, as the `next` function receives it.
/// Mutation goes through interior mutability in `C`.
pub struct Cursor<'c, C> {
    cursor: &'c C,
}

impl<C> Deref for Cursor<'_, C> {
    type Target = C;
    fn deref(&self) -> &C {
        self.cursor
    }
}

impl<C: 'static> Param for Cursor<'_, C> {
    type Item<'c> = Cursor<'c, C>;
}

impl<'c, C: 'static> ParamItem<'c> for Cursor<'c, C> {
    const KIND: ParamKind = ParamKind::Regular;
    const EXPECTED: &'static str = "iterator cursor";
    fn read_slot(view: ValueView<'c>) -> Result<Self> {
        cursor_payload::<C>(view)
            .map(|cursor| Cursor { cursor })
            .ok_or_else(|| crate::diagnostics::type_error(view, "iterator cursor"))
    }
    fn matches(view: ValueView<'c>) -> bool {
        cursor_payload::<C>(view).is_some()
    }
}

unsafe extern "C" fn destroy_cursor<C>(_: *mut ffi::lua_State, userdata: *mut c_void) {
    // SAFETY: only `push_cursor::<C>` creates userdata with this destructor, fully written.
    let outcome = std::panic::catch_unwind(|| unsafe { ptr::drop_in_place(userdata.cast::<C>()) });
    if outcome.is_err() {
        std::process::abort();
    }
}

/// Pushes the shared frozen, protected metatable for cursors of type `C`, creating it on
/// first use.
unsafe fn push_cursor_metatable<C: 'static>(state: *mut ffi::lua_State) -> Result<()> {
    unsafe {
        let key = type_key::<C>();
        let kind = ffi::lua_rawgetp(state, ffi::LUA_REGISTRYINDEX, key);
        if kind == ffi::LUA_TTABLE {
            return Ok(());
        }
        if kind != ffi::LUA_TNIL {
            ffi::lua_pop(state, 1);
            return Err(Error::logic("Iterator cursor metatable registry is corrupted"));
        }
        ffi::lua_pop(state, 1);
        ffi::lua_createtable(state, 0, 1);
        ffi::lua_pushboolean(state, 0);
        ffi::lua_rawsetfield(state, -2, c"__metatable".as_ptr());
        ffi::lua_setreadonly(state, -1, 1);
        ffi::lua_pushvalue(state, -1);
        ffi::lua_rawsetp(state, ffi::LUA_REGISTRYINDEX, key);
        Ok(())
    }
}

/// The cursor when `view` is a cursor userdata of type `C` (exact metatable identity).
fn cursor_payload<'v, C: 'static>(view: ValueView<'v>) -> Option<&'v C> {
    if !view.is_userdata() {
        return None;
    }
    let state = view.state();
    // SAFETY: balanced push/pop to read the registered metatable's identity; the userdata's
    // metatable pointer is read without pushes.
    unsafe {
        if ffi::lua_rawgetp(state, ffi::LUA_REGISTRYINDEX, type_key::<C>()) != ffi::LUA_TTABLE {
            ffi::lua_pop(state, 1);
            return None;
        }
        let expected = ffi::lua_topointer(state, -1);
        ffi::lua_pop(state, 1);
        if ffi::lua_getmetatablepointer(state, view.index()) != expected {
            return None;
        }
        ffi::lua_touserdata(state, view.index()).cast::<C>().as_ref()
    }
}

/// Pushes a new cursor userdata holding `cursor`.
fn push_cursor<C: 'static>(scope: &impl Scope, cursor: C) -> Result<()> {
    const { super::assert_userdata_layout::<C>() };
    let state = scope.state();
    // SAFETY: allocate, write immediately, attach the shared metatable.
    unsafe {
        let raw = ffi::lua_newuserdatadtor(state, std::mem::size_of::<C>(), destroy_cursor::<C>);
        if raw.is_null() {
            return Err(Error::runtime("Unable to allocate iterator cursor"));
        }
        ptr::write(raw.cast::<C>(), cursor);
        push_cursor_metatable::<C>(state)?;
        if ffi::lua_setmetatable(state, -2) == 0 {
            return Err(Error::logic("Unable to attach iterator cursor metatable"));
        }
    }
    Ok(())
}

/// Rust state of a cursor factory closure: the `make_cursor` callable.
#[repr(C)]
struct FactoryContext<Make> {
    make: Make,
}

unsafe extern "C" fn destroy_factory<Make>(_: *mut ffi::lua_State, userdata: *mut c_void) {
    let outcome = std::panic::catch_unwind(|| unsafe { ptr::drop_in_place(userdata.cast::<FactoryContext<Make>>()) });
    if outcome.is_err() {
        std::process::abort();
    }
}

/// `__iter` of a cursor iterator: upvalue 1 is the factory context, upvalue 2 the shared
/// `next` function. Returns `(next, cursor, nil)`.
unsafe extern "C-unwind" fn cursor_factory<C: 'static, Make>(state: *mut ffi::lua_State) -> c_int
where
    Make: Fn(&Call<'_>) -> Result<C> + 'static,
{
    unsafe {
        trampoline::enter(state, || {
            let context = ffi::lua_touserdata(state, ffi::lua_upvalueindex(1)).cast::<FactoryContext<Make>>();
            if context.is_null() {
                return Err(Error::logic("Invalid iterator factory context"));
            }
            let call = Call::from_raw(state);
            let cursor = ((*context).make)(&call)?;
            push_cursor(&call, cursor)?;
            ffi::lua_pushvalue(state, ffi::lua_upvalueindex(2));
            ffi::lua_replace(state, 1);
            ffi::lua_pushnil(state);
            Ok(3)
        })
    }
}

// ---------------------------------------------------------------------------------------------
// Builder entry points
// ---------------------------------------------------------------------------------------------

impl super::metatable::MetatableBuilder<'_> {
    /// Installs `__iter` returning `(next, object, 0)`: `next` receives the object and the
    /// numeric control and returns the next `(control, value)` or `None`.
    pub fn array_iterator<F: Binding<M>, M>(&mut self, next: F) -> Result<()> {
        self.stateless_iterator::<F, M, false>(next)
    }

    /// Installs `__iter` returning `(next, object, nil)`: `next` receives the object and the
    /// previous key (nil first) and returns the next `(key, value)` or `None`.
    pub fn keyed_iterator<F: Binding<M>, M>(&mut self, next: F) -> Result<()> {
        self.stateless_iterator::<F, M, true>(next)
    }

    fn stateless_iterator<F: Binding<M>, M, const INITIAL_NIL: bool>(&mut self, next: F) -> Result<()> {
        self.check_iter_installable()?;
        let type_name = self.type_name()?;
        let next_name = self.retain_name(&format!("{type_name}.next"))?;
        let iter_name = self.retain_name(&format!("{type_name}.__iter"))?;
        let state = self.state_ptr();
        // SAFETY: the next closure is pushed and becomes upvalue 1 of the factory, which is then
        // raw-set into the metatable; every push is consumed.
        unsafe {
            function_closure(state, next, next_name)?;
            ffi::lua_pushcclosure(state, stateless_factory::<INITIAL_NIL>, iter_name, 1);
            self.raw_set_metatable_top(c"__iter");
        }
        Ok(())
    }

    /// Installs `__iter` that creates a private cursor per loop: `make_cursor` receives the call
    /// (argument 1 is the iterated object) and `next` receives `Cursor<C>` (plus the ignored
    /// control) and returns the next `(key, value)` or `None`.
    pub fn cursor_iterator<C, Make, F, M>(&mut self, make_cursor: Make, next: F) -> Result<()>
    where
        C: 'static,
        Make: Fn(&Call<'_>) -> Result<C> + 'static,
        F: Binding<M>,
    {
        self.check_iter_installable()?;
        let type_name = self.type_name()?;
        let next_name = self.retain_name(&format!("{type_name}.next"))?;
        let iter_name = self.retain_name(&format!("{type_name}.__iter"))?;
        let state = self.state_ptr();
        const { super::assert_userdata_layout::<FactoryContext<Make>>() };
        // SAFETY: context userdata is written immediately after allocation; it and the next
        // closure become the factory's two upvalues.
        unsafe {
            let raw =
                ffi::lua_newuserdatadtor(state, std::mem::size_of::<FactoryContext<Make>>(), destroy_factory::<Make>);
            if raw.is_null() {
                return Err(Error::runtime("Unable to allocate iterator factory context"));
            }
            ptr::write(raw.cast::<FactoryContext<Make>>(), FactoryContext { make: make_cursor });
            function_closure(state, next, next_name)?;
            ffi::lua_pushcclosure(state, cursor_factory::<C, Make>, iter_name, 2);
            self.raw_set_metatable_top(c"__iter");
        }
        Ok(())
    }

    fn check_iter_installable(&self) -> Result<()> {
        if !self.field_is_nil(c"__iter") {
            return Err(Error::logic("Metatable already has an __iter metamethod"));
        }
        Ok(())
    }
}

/// Convenience return for iterator `next` functions: `Some((key, value))` continues, `None`
/// ends the loop. Equivalent to `Option<(K, V)>`; provided for readability.
pub type Step<K, V> = Option<(K, V)>;

/// Marker so `Step` reads as a [`Return`]; nothing to implement beyond `Option<(K, V)>`.
#[doc(hidden)]
pub struct StepReturnMarker<K, V>(PhantomData<(K, V)>);

const _: () = {
    fn _assert_step_is_return<K: crate::convert::Push, V: crate::convert::Push>()
    where
        Step<K, V>: Return,
        StackResults: Return,
    {
    }
};
