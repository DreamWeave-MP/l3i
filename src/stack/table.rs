use super::{Stack, ValueView};
use crate::error::{Error, Result};
use crate::raw::ffi;

/// A borrowed view of a table on the stack.
///
/// `get`/`raw_get` leave the looked-up value on the stack and return a view of it, exactly as
/// the C++ `TableView` did; wrap a lookup chain in [`Stack::with_frame`] to rebalance.
#[derive(Clone, Copy, Debug)]
pub struct TableView<'s> {
    value: ValueView<'s>,
}

impl<'s> TableView<'s> {
    pub(crate) fn new(value: ValueView<'s>) -> Self {
        TableView { value }
    }

    pub fn value(&self) -> ValueView<'s> {
        self.value
    }

    pub fn stack(&self) -> &Stack<'s> {
        self.value.stack()
    }

    pub fn index(&self) -> std::ffi::c_int {
        self.value.index()
    }

    /// Honours `__index`; leaves the result on the stack.
    pub fn get(&self, key: &str) -> ValueView<'s> {
        let state = self.stack().state();
        // SAFETY: live stack, table index verified when the view was created.
        unsafe {
            ffi::lua_pushlstring(state, key.as_ptr().cast(), key.len());
            ffi::lua_gettable(state, self.index());
        }
        self.stack().top_value_static()
    }

    /// Bypasses `__index`; leaves the result on the stack.
    pub fn raw_get(&self, key: &str) -> ValueView<'s> {
        let state = self.stack().state();
        unsafe {
            ffi::lua_pushlstring(state, key.as_ptr().cast(), key.len());
            ffi::lua_rawget(state, self.index());
        }
        self.stack().top_value_static()
    }

    /// Honours `__newindex`. Consumes the value on top of the stack.
    pub fn set_from_top(&self, key: &str) -> Result<()> {
        self.reject_registry()?;
        let state = self.stack().state();
        unsafe {
            ffi::lua_pushlstring(state, key.as_ptr().cast(), key.len());
            ffi::lua_insert(state, -2);
            ffi::lua_settable(state, self.index());
        }
        Ok(())
    }

    /// Bypasses `__newindex`. Consumes the value on top of the stack.
    pub fn raw_set_from_top(&self, key: &str) -> Result<()> {
        self.reject_registry()?;
        let state = self.stack().state();
        unsafe {
            ffi::lua_pushlstring(state, key.as_ptr().cast(), key.len());
            ffi::lua_insert(state, -2);
            ffi::lua_rawset(state, self.index());
        }
        Ok(())
    }

    /// Raw border length, ignoring `__len`.
    pub fn raw_len(&self) -> usize {
        // lua_objlen returns int; Luau lengths are never negative.
        unsafe { ffi::lua_objlen(self.stack().state(), self.index()) as usize }
    }

    pub fn is_read_only(&self) -> bool {
        unsafe { ffi::lua_getreadonly(self.stack().state(), self.index()) != 0 }
    }

    pub fn set_read_only(&self, read_only: bool) {
        unsafe { ffi::lua_setreadonly(self.stack().state(), self.index(), read_only.into()) }
    }

    fn reject_registry(&self) -> Result<()> {
        if self.index() == ffi::LUA_REGISTRYINDEX {
            return Err(Error::logic("Cannot set the Lua registry pseudo-index"));
        }
        Ok(())
    }
}

impl<'vm> Stack<'vm> {
    /// Like `top_value` but carrying the stack's own lifetime, for views derived from another
    /// view of the same stack rather than from a `&Stack` borrow.
    pub(crate) fn top_value_static(&self) -> ValueView<'vm> {
        let top = self.top();
        ValueView::new(*self, top)
    }
}
