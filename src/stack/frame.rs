use std::cell::Cell;
use std::ffi::{CString, c_char, c_int};
use std::marker::PhantomData;

use super::{TableView, ValueView, checked_capacity, push_copy};
use crate::error::{Error, Result};
use crate::raw::{ffi, protect};

/// A temporary region of the stack. Everything pushed through it is popped when it drops.
///
/// Views produced by a frame borrow the frame, so they cannot outlive it; popping needs `&mut`,
/// so they cannot survive a pop either. Nested frames are opened from a frame with
/// [`Frame::frame`], one at a time.
pub struct Frame<'p> {
    state: *mut ffi::lua_State,
    floor: c_int,
    armed: bool,
    host_level: bool,
    /// The parent's "child open" flag, cleared when this frame drops.
    parent_child_open: &'p Cell<bool>,
    /// Whether a frame opened from this one is currently alive.
    child_open: Cell<bool>,
    _parent: PhantomData<&'p ()>,
}

impl<'p> Frame<'p> {
    pub(super) fn open(state: *mut ffi::lua_State, host_level: bool, parent_child_open: &'p Cell<bool>) -> Self {
        assert!(
            !parent_child_open.replace(true),
            "a frame is already open on this scope; open nested frames from the innermost frame"
        );
        // SAFETY: state is live for the parent's lifetime.
        let floor = unsafe { ffi::lua_gettop(state) };
        Frame {
            state,
            floor,
            armed: true,
            host_level,
            parent_child_open,
            child_open: Cell::new(false),
            _parent: PhantomData,
        }
    }

    fn assert_no_open_child(&self) {
        debug_assert!(!self.child_open.get(), "push through the open child Frame, not its parent");
    }

    /// True outside any Lua call; see [`super::Stack::is_host_level`].
    pub fn is_host_level(&self) -> bool {
        self.host_level
    }

    /// Runs an operation that may raise a Luau error. Inside a Lua call it runs directly and a
    /// raise unwinds to Luau's `pcall`; at host level it runs under `protected_call` and a
    /// raise becomes `Err`. `body` sees its `nargs` operands at `1..=nargs` in the protected
    /// case and at the top of the frame otherwise, so it must address them relative to the top.
    ///
    /// # Safety
    /// `nargs` values are on top of the frame; `body` leaves exactly `nresults` results.
    pub(crate) unsafe fn raising(
        &self,
        nargs: c_int,
        nresults: c_int,
        body: impl FnOnce(*mut ffi::lua_State) -> c_int,
    ) -> Result<()> {
        unsafe {
            if self.host_level {
                protect::protected_call(self.state, nargs, nresults, body)
            } else {
                body(self.state);
                Ok(())
            }
        }
    }

    #[inline]
    pub(crate) fn state(&self) -> *mut ffi::lua_State {
        self.state
    }

    /// The height recorded when the frame opened; slots above it belong to the frame.
    #[inline]
    pub fn floor(&self) -> c_int {
        self.floor
    }

    #[inline]
    pub fn top(&self) -> c_int {
        unsafe { ffi::lua_gettop(self.state) }
    }

    /// Number of values the frame currently holds above its floor.
    pub fn len(&self) -> c_int {
        (self.top() - self.floor).max(0)
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// A view of any existing slot, including ones below the floor, borrowing the frame.
    pub fn at(&self, index: c_int) -> ValueView<'_> {
        ValueView::resolve(self.state, index)
    }

    /// The most recently pushed value.
    pub fn top_value(&self) -> ValueView<'_> {
        self.at(-1)
    }

    /// Opens a nested frame.
    ///
    /// # Panics
    /// If a nested frame opened from this one is still alive.
    pub fn frame(&self) -> Frame<'_> {
        Frame::open(self.state, self.host_level, &self.child_open)
    }

    pub fn with_frame<R>(&self, body: impl FnOnce(&Frame<'_>) -> Result<R>) -> Result<R> {
        let frame = self.frame();
        body(&frame)
    }

    /// Pops `count` values, never below the floor. Negative counts pop nothing. Takes `&mut`
    /// so no view of this frame can be alive across the pop.
    pub fn pop(&mut self, count: c_int) {
        let count = count.clamp(0, self.len());
        if count > 0 {
            // SAFETY: the target height is between the floor and the current top.
            unsafe { ffi::lua_settop(self.state, self.top() - count) };
        }
    }

    /// Disarms the frame: whatever it holds stays on the stack when it drops. This is how a
    /// value built inside a frame is handed to the enclosing scope.
    pub fn release(&mut self) {
        self.armed = false;
    }

    /// Keeps the current top value (normally a Luau error object), discards every other value
    /// the frame holds, and disarms the frame.
    pub fn preserve_top_and_release(&mut self) {
        self.armed = false;
        let current = self.top();
        if current <= self.floor {
            return;
        }
        // SAFETY: indexes derived from the live top and this frame's floor.
        unsafe {
            if current > self.floor + 1 {
                ffi::lua_replace(self.state, self.floor + 1);
            }
            ffi::lua_settop(self.state, self.floor + 1);
        }
    }

    pub fn push_nil(&self) -> ValueView<'_> {
        self.assert_no_open_child();
        unsafe { ffi::lua_pushnil(self.state) };
        self.top_value()
    }

    pub fn push_boolean(&self, value: bool) -> ValueView<'_> {
        self.assert_no_open_child();
        unsafe { ffi::lua_pushboolean(self.state, c_int::from(value)) };
        self.top_value()
    }

    pub fn push_number(&self, value: f64) -> ValueView<'_> {
        self.assert_no_open_child();
        unsafe { ffi::lua_pushnumber(self.state, value) };
        self.top_value()
    }

    pub fn push_string(&self, value: &str) -> ValueView<'_> {
        self.assert_no_open_child();
        unsafe { ffi::lua_pushlstring(self.state, value.as_ptr().cast(), value.len()) };
        self.top_value()
    }

    pub fn push_table(&self, array_capacity: usize, hash_capacity: usize) -> Result<TableView<'_>> {
        self.assert_no_open_child();
        let narr = checked_capacity(array_capacity)?;
        let nrec = checked_capacity(hash_capacity)?;
        unsafe { ffi::lua_createtable(self.state, narr, nrec) };
        Ok(TableView::new(self.top_value()))
    }

    /// Pushes a copy of `value`, which may come from any scope of this VM.
    pub fn push_value(&self, value: ValueView<'_>) -> Result<ValueView<'_>> {
        self.assert_no_open_child();
        push_copy(self.state, value)?;
        Ok(self.top_value())
    }

    /// # Safety
    /// `debug_name` is null or a pointer that stays valid until the VM closes.
    pub unsafe fn push_c_function(&self, function: ffi::lua_CFunction, debug_name: *const c_char) -> ValueView<'_> {
        self.assert_no_open_child();
        unsafe { ffi::lua_pushcfunction(self.state, function, debug_name) };
        self.top_value()
    }

    /// Pops the frame's top value into the global `name`. Raises (or fails at host level) when
    /// the globals table is read-only.
    pub fn set_global(&self, name: &str) -> Result<()> {
        self.require_value("set a global")?;
        let name = CString::new(name).map_err(|_| Error::logic("Global name cannot contain NUL"))?;
        // SAFETY: one operand (the value) is on top; lua_setglobal consumes it.
        unsafe {
            self.raising(1, 0, |state| {
                ffi::lua_setglobal(state, name.as_ptr());
                0
            })
        }
    }

    pub(crate) fn require_value(&self, action: &str) -> Result<()> {
        if self.is_empty() {
            return Err(Error::logic(format!("Cannot {action}: the frame holds no value")));
        }
        Ok(())
    }
}

impl Drop for Frame<'_> {
    fn drop(&mut self) {
        self.parent_child_open.set(false);
        if self.armed {
            // SAFETY: the target height is not above the current one; restoring downwards is
            // always valid. Never grow: an earlier-dropped sibling may have lowered the top.
            unsafe {
                if ffi::lua_gettop(self.state) > self.floor {
                    ffi::lua_settop(self.state, self.floor);
                }
            }
        }
    }
}
