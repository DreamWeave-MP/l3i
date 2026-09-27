use std::ffi::c_int;

use super::{Frame, ValueView};
use crate::error::{Error, Result};
use crate::raw::ffi;

/// A borrowed view of a table on the stack.
///
/// Lookups push their result and return a view bound to the frame you pass, exactly as the
/// C++ `TableView::get` left the value on the stack; the frame decides when it is popped.
/// Stores consume the value on top of that frame.
#[derive(Clone, Copy, Debug)]
pub struct TableView<'v> {
    value: ValueView<'v>,
}

impl<'v> TableView<'v> {
    pub(crate) fn new(value: ValueView<'v>) -> Self {
        TableView { value }
    }

    pub fn value(&self) -> ValueView<'v> {
        self.value
    }

    pub fn index(&self) -> c_int {
        self.value.index()
    }

    fn require_live(&self) -> Result<()> {
        if !self.value.exists() {
            return Err(Error::logic("Table view no longer names a live stack slot"));
        }
        Ok(())
    }

    /// Honours `__index`; the result is pushed onto `frame`. A raising `__index` unwinds to
    /// Luau inside a call and becomes `Err` at host level.
    pub fn get<'f>(&self, frame: &'f Frame<'_>, key: &str) -> Result<ValueView<'f>> {
        self.require_live()?;
        let state = frame.state();
        // SAFETY: the table slot exists; operands (table copy, key) sit on top for the raising
        // body, which consumes both and leaves one result.
        unsafe {
            ffi::lua_pushvalue(state, self.index());
            ffi::lua_pushlstring(state, key.as_ptr().cast(), key.len());
            frame.raising(2, 1, |state| {
                ffi::lua_gettable(state, -2);
                ffi::lua_remove(state, -2);
                1
            })?;
        }
        Ok(frame.top_value())
    }

    /// Bypasses `__index`; the result is pushed onto `frame`. Never raises.
    pub fn raw_get<'f>(&self, frame: &'f Frame<'_>, key: &str) -> Result<ValueView<'f>> {
        self.require_live()?;
        let state = frame.state();
        unsafe {
            ffi::lua_pushlstring(state, key.as_ptr().cast(), key.len());
            ffi::lua_rawget(state, self.index());
        }
        Ok(frame.top_value())
    }

    /// Honours `__newindex`. Consumes the value on top of `frame`.
    pub fn set(&self, frame: &Frame<'_>, key: &str) -> Result<()> {
        self.require_store(frame)?;
        let state = frame.state();
        // SAFETY: operands (value, table copy, key) on top; the body consumes all three.
        unsafe {
            ffi::lua_pushvalue(state, self.index());
            ffi::lua_pushlstring(state, key.as_ptr().cast(), key.len());
            frame.raising(3, 0, |state| {
                // [value, table, key] -> settable(table)[key] = value
                ffi::lua_insert(state, -3);
                ffi::lua_insert(state, -3);
                ffi::lua_settable(state, -3);
                ffi::lua_pop(state, 1);
                0
            })
        }
    }

    /// Bypasses `__newindex`. Consumes the value on top of `frame`. Raises (or fails at host
    /// level) when the table is read-only.
    pub fn raw_set(&self, frame: &Frame<'_>, key: &str) -> Result<()> {
        self.require_store(frame)?;
        let state = frame.state();
        unsafe {
            ffi::lua_pushvalue(state, self.index());
            ffi::lua_pushlstring(state, key.as_ptr().cast(), key.len());
            frame.raising(3, 0, |state| {
                ffi::lua_insert(state, -3);
                ffi::lua_insert(state, -3);
                ffi::lua_rawset(state, -3);
                ffi::lua_pop(state, 1);
                0
            })
        }
    }

    /// Raw border length, ignoring `__len`.
    pub fn raw_len(&self) -> usize {
        if !self.value.exists() {
            return 0;
        }
        // lua_objlen returns int; Luau lengths are never negative.
        unsafe { ffi::lua_objlen(self.value.state(), self.index()).max(0) as usize }
    }

    pub fn is_read_only(&self) -> bool {
        self.value.exists() && unsafe { ffi::lua_getreadonly(self.value.state(), self.index()) != 0 }
    }

    pub fn set_read_only(&self, read_only: bool) -> Result<()> {
        self.require_live()?;
        unsafe { ffi::lua_setreadonly(self.value.state(), self.index(), read_only.into()) };
        Ok(())
    }

    fn require_store(&self, frame: &Frame<'_>) -> Result<()> {
        self.require_live()?;
        if self.index() == ffi::LUA_REGISTRYINDEX {
            return Err(Error::logic("Cannot set the Lua registry pseudo-index"));
        }
        if frame.state() != self.value.state() {
            return Err(Error::logic("Frame and table belong to different Lua threads"));
        }
        frame.require_value("store into a table")
    }
}
