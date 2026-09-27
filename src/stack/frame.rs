use std::cell::Cell;
use std::ffi::{CString, c_char, c_int};
use std::marker::PhantomData;

use super::{TableView, ValueView, checked_capacity, push_copy};
use crate::error::{Error, Result};
use crate::raw::{ffi, protect};

/// A temporary region of the stack. Everything pushed through it is popped when it drops.
///
/// Views produced by a frame borrow the frame, so they cannot outlive it. Nested frames are
/// opened from a frame with [`Frame::frame`]; sibling frames are legal but drop in the wrong
/// order at your own cost: the later-dropped frame does nothing (it never grows the stack), and
/// its views read as [`super::Type::None`].
pub struct Frame<'p> {
    state: *mut ffi::lua_State,
    floor: c_int,
    armed: bool,
    host_level: bool,
    open_frames: &'p Cell<u32>,
    _parent: PhantomData<&'p ()>,
}

impl<'p> Frame<'p> {
    pub(super) fn open(state: *mut ffi::lua_State, host_level: bool, open_frames: &'p Cell<u32>) -> Self {
        open_frames.set(open_frames.get() + 1);
        // SAFETY: state is live for the parent's lifetime.
        let floor = unsafe { ffi::lua_gettop(state) };
        Frame { state, floor, armed: true, host_level, open_frames, _parent: PhantomData }
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
    pub(crate) unsafe fn raising(&self, nargs: c_int, nresults: c_int, body: impl FnOnce(*mut ffi::lua_State) -> c_int) -> Result<()> {
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
    pub fn frame(&self) -> Frame<'_> {
        Frame::open(self.state, self.host_level, self.open_frames)
    }

    pub fn with_frame<R>(&self, body: impl FnOnce(&Frame<'_>) -> Result<R>) -> Result<R> {
        let frame = self.frame();
        body(&frame)
    }

    /// Pops `count` values, never below the floor. Negative counts pop nothing.
    pub fn pop(&self, count: c_int) {
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

    /// Pushes a copy of `value`, which may come from any scope of this VM.
    pub fn push_value(&self, value: ValueView<'_>) -> Result<ValueView<'_>> {
        push_copy(self.state, value)?;
        Ok(self.top_value())
    }

    /// # Safety
    /// `debug_name` is null or a pointer that stays valid until the VM closes.
    pub unsafe fn push_c_function(&self, function: ffi::lua_CFunction, debug_name: *const c_char) -> ValueView<'_> {
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
        self.open_frames.set(self.open_frames.get() - 1);
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
