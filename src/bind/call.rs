use std::ffi::c_int;

use crate::error::{Error, Result};
use crate::raw::ffi;
use crate::stack::{Frame, Scope, Stack, ValueView};
use crate::value::Function;

/// The native call's frame (`Lua::Call`): the arguments at `1..=argument_count()` and the
/// stack above them, where results are pushed.
pub struct Call<'c> {
    stack: Stack<'c>,
    initial_top: c_int,
}

impl<'c> Call<'c> {
    /// # Safety
    /// `state` is the live thread Luau passed to the enclosing C function.
    pub(crate) unsafe fn from_raw(state: *mut ffi::lua_State) -> Call<'c> {
        // SAFETY: forwarded from the C entry point; a native call is never host level.
        let stack = unsafe { Stack::from_raw(state, false) };
        let initial_top = stack.top();
        Call { stack, initial_top }
    }

    /// A call whose argument count and thread record were read by `l3i_native_enter`.
    ///
    /// # Safety
    /// As `from_raw`.
    #[inline]
    pub(crate) unsafe fn from_parts(
        state: *mut ffi::lua_State,
        initial_top: c_int,
        record: *const crate::runtime::shared::ThreadRecord,
    ) -> Call<'c> {
        Call { stack: unsafe { Stack::from_raw_recorded(state, record) }, initial_top }
    }

    /// The call-level stack.
    #[inline(always)]
    pub fn stack(&self) -> &Stack<'c> {
        &self.stack
    }

    /// Number of arguments the caller passed.
    #[inline(always)]
    pub fn argument_count(&self) -> c_int {
        self.initial_top
    }

    /// The stack height when the call began.
    pub fn initial_top(&self) -> c_int {
        self.initial_top
    }

    /// Argument `index` (1-based). Beyond `argument_count` the view reads as none.
    #[inline(always)]
    pub fn arg(&self, index: c_int) -> ValueView<'_> {
        if index >= 1 && index <= self.initial_top {
            return ValueView::within(self.stack.state(), index, self.initial_top);
        }
        self.stack.at(index)
    }

    /// Upvalue `index` of the running C closure (1-based; the binder owns upvalue 1).
    pub fn upvalue(&self, index: c_int) -> ValueView<'_> {
        self.stack.at(ffi::lua_upvalueindex(index))
    }

    /// Results pushed so far, i.e. values above the arguments.
    pub fn result_count(&self) -> c_int {
        self.stack.top() - self.initial_top
    }

    /// Calls `function` with the arguments from `first_argument` to the last one, forwarding
    /// every result. On failure the error object is left on top and `LuaErrorOnStack` is
    /// returned, so the native entry re-raises it unchanged (`Call::invoke`).
    pub fn forward_to(&self, function: &Function, first_argument: c_int) -> Result<c_int> {
        if !function.value().belongs_to(self.stack.state()) {
            return Err(Error::logic("Lua callback belongs to a different Lua state"));
        }
        let argument_count = self.initial_top - first_argument + 1;
        if argument_count < 0 {
            return Err(Error::logic("Invalid Lua callback argument index"));
        }
        let state = self.stack.state();
        // SAFETY: trims anything above the arguments, pushes the function and copies of the
        // arguments, then pcalls; on failure the error object is the only thing left above the
        // arguments, exactly where `LuaErrorOnStack` expects it.
        unsafe {
            ffi::lua_settop(state, self.initial_top);
            ffi::lua_checkstack(state, argument_count + 1);
            ffi::lua_getref(state, function.value().reference_id());
            for offset in 0..argument_count {
                ffi::lua_pushvalue(state, first_argument + offset);
            }
            let lua_call = crate::runtime::shared::LuaCall::enter(state);
            let status = ffi::lua_pcall(state, argument_count, ffi::LUA_MULTRET, 0);
            drop(lua_call);
            if status != ffi::LUA_OK {
                return Err(Error::LuaErrorOnStack);
            }
        }
        Ok(self.result_count())
    }
}

impl<'c> Scope for Call<'c> {
    fn state(&self) -> *mut ffi::lua_State {
        Scope::state(&self.stack)
    }
    fn at(&self, index: c_int) -> ValueView<'_> {
        self.stack.at(index)
    }
    fn frame(&self) -> Frame<'_> {
        self.stack.frame()
    }
}

impl crate::stack::sealed::Sealed for Call<'_> {}
