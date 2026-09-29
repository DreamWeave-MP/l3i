//! Entry point for hand-written `lua_CFunction`s that use the binder's stack layer.

use std::ffi::c_int;

use crate::error::Result;
use crate::raw::{ffi, trampoline};
use crate::stack::Stack;

/// Runs `body` as the entire implementation of a `lua_CFunction`, with a [`Stack`] over the
/// calling thread. An `Err` is raised as a Lua error after every Rust value has dropped; a panic
/// aborts the process, since it cannot cross Luau's frames.
///
/// # Safety
/// `state` is the `lua_State*` Luau passed to the enclosing C function, and the caller is that
/// function's frame.
pub unsafe fn enter(state: *mut ffi::lua_State, body: impl FnOnce(&Stack<'_>) -> Result<c_int>) -> c_int {
    unsafe {
        trampoline::enter(state, || {
            let stack = Stack::from_raw(state, false);
            body(&stack)
        })
    }
}
