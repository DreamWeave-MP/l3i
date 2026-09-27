//! Borrowed views over a live Luau stack: the hot-path tier.
//!
//! Nothing here pins a registry reference. A [`ValueView`] is `(stack, index)` and is valid
//! exactly as long as the raw Lua index it names; the lifetimes make a view unable to outlive
//! the [`Stack`] or [`StackFrame`] it was read from, which is the compile-time form of the
//! C++ binder's "borrowed values cannot escape a scoped frame" rule.

mod frame;
mod table;
mod view;

#[cfg(test)]
mod tests;

use std::ffi::{c_char, c_int};
use std::marker::PhantomData;

use crate::error::{Error, Result};
use crate::raw::ffi;

pub use frame::StackFrame;
pub use table::TableView;
pub use view::{Type, ValueView};

/// A non-owning handle to one Lua thread's stack.
///
/// `'vm` is the lifetime for which the caller guarantees the `lua_State` stays alive; inside a
/// native callback that is the callback's frame, inside a host-side operation it is the borrow
/// of the owning [`crate::runtime::Runtime`].
#[derive(Clone, Copy, Debug)]
pub struct Stack<'vm> {
    state: *mut ffi::lua_State,
    _vm: PhantomData<&'vm ()>,
}

impl<'vm> Stack<'vm> {
    /// # Safety
    /// `state` must be a live Luau thread that outlives `'vm`, and no other code may run on
    /// that VM concurrently (Luau VMs are single-threaded).
    pub(crate) unsafe fn from_raw(state: *mut ffi::lua_State) -> Self {
        debug_assert!(!state.is_null(), "Stack requires a live Lua state");
        Stack { state, _vm: PhantomData }
    }

    #[inline]
    pub(crate) fn state(&self) -> *mut ffi::lua_State {
        self.state
    }

    #[inline]
    pub fn top(&self) -> c_int {
        // SAFETY: state is live for 'vm.
        unsafe { ffi::lua_gettop(self.state) }
    }

    #[inline]
    pub fn pop(&self, count: c_int) {
        // SAFETY: state is live; lua_settop below top is always valid.
        unsafe { ffi::lua_pop(self.state, count) }
    }

    /// Records the current height and restores it when the frame drops.
    pub fn frame(&self) -> StackFrame<'_, 'vm> {
        StackFrame::new(self)
    }

    /// Runs `body` with the stack height restored afterwards, whether or not it succeeded.
    ///
    /// The callback must preserve the entry prefix and work only above it. Its result cannot
    /// borrow from the frame: the higher-ranked lifetime on the closure makes returning a view
    /// a compile error, replacing the C++ `requireOwnedFrameResult` assertion.
    pub fn with_frame<R>(&self, body: impl for<'f> FnOnce(&'f Stack<'vm>) -> Result<R>) -> Result<R> {
        let _frame = self.frame();
        body(self)
    }

    /// A view of the value at `index`. Negative indexes are resolved against the current top so
    /// the view stays stable while values are pushed above it. `at(0)` and out-of-range negative
    /// indexes produce a view of type [`Type::None`], as in the C++ binder. Positive indexes must
    /// be acceptable to Luau: at most the current frame's allocated top (`lua_checkstack`).
    pub fn at(&self, index: c_int) -> ValueView<'_> {
        if index > 0 || index <= ffi::LUA_REGISTRYINDEX {
            return ValueView::new(*self, index);
        }
        if index == 0 {
            return ValueView::new(*self, 0);
        }
        let absolute = self.top() + index + 1;
        ValueView::new(*self, if absolute > 0 { absolute } else { 0 })
    }

    pub fn top_value(&self) -> ValueView<'_> {
        self.at(-1)
    }

    pub fn push_nil(&self) -> ValueView<'_> {
        // SAFETY: live state; Luau guarantees LUA_MINSTACK free slots on entry and the binder
        // calls check_stack before pushing more than that.
        unsafe { ffi::lua_pushnil(self.state) };
        self.top_value()
    }

    pub fn push_boolean(&self, value: bool) -> ValueView<'_> {
        unsafe { ffi::lua_pushboolean(self.state, c_int::from(value)) };
        self.top_value()
    }

    pub fn push_number(&self, value: f64) -> ValueView<'_> {
        unsafe { ffi::lua_pushnumber(self.state, value) };
        self.top_value()
    }

    pub fn push_string(&self, value: &str) -> ValueView<'_> {
        unsafe { ffi::lua_pushlstring(self.state, value.as_ptr().cast(), value.len()) };
        self.top_value()
    }

    pub fn push_table(&self, array_capacity: usize, hash_capacity: usize) -> Result<TableView<'_>> {
        let narr = checked_capacity(array_capacity)?;
        let nrec = checked_capacity(hash_capacity)?;
        unsafe { ffi::lua_createtable(self.state, narr, nrec) };
        Ok(TableView::new(self.top_value()))
    }

    /// Pushes a C function with an already-retained debug name (see [`crate::debug_name`]).
    ///
    /// # Safety
    /// `debug_name` is null or a pointer that stays valid until the VM closes.
    pub unsafe fn push_c_function(&self, function: ffi::lua_CFunction, debug_name: *const c_char) -> ValueView<'_> {
        unsafe { ffi::lua_pushcfunction(self.state, function, debug_name) };
        self.top_value()
    }

    /// Pops the top value into the global `name`.
    pub fn set_global(&self, name: &str) -> Result<()> {
        let name = std::ffi::CString::new(name).map_err(|_| Error::logic("Global name cannot contain NUL"))?;
        unsafe { ffi::lua_setglobal(self.state, name.as_ptr()) };
        Ok(())
    }

    /// Ensures `extra` free slots, as `lua_checkstack`.
    pub fn check(&self, extra: c_int) -> Result<()> {
        if unsafe { ffi::lua_checkstack(self.state, extra) } == 0 {
            return Err(Error::runtime("Lua error: stack overflow"));
        }
        Ok(())
    }
}

fn checked_capacity(capacity: usize) -> Result<c_int> {
    c_int::try_from(capacity).map_err(|_| Error::logic("Lua table capacity exceeds the supported range"))
}

/// True when both threads belong to one VM.
#[allow(dead_code)] // used from the tagged/untagged userdata slices
pub(crate) fn same_vm(left: *mut ffi::lua_State, right: *mut ffi::lua_State) -> bool {
    !left.is_null() && !right.is_null() && unsafe { ffi::lua_mainthread(left) == ffi::lua_mainthread(right) }
}
