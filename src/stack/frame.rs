use std::ffi::c_int;

use super::Stack;
use crate::raw::ffi;

/// Restores the stack height recorded at construction when dropped.
///
/// Dereferences to the underlying [`Stack`], so views read through the frame cannot outlive it.
pub struct StackFrame<'s, 'vm> {
    stack: Option<&'s Stack<'vm>>,
    top: c_int,
}

impl<'s, 'vm> StackFrame<'s, 'vm> {
    pub(crate) fn new(stack: &'s Stack<'vm>) -> Self {
        StackFrame { stack: Some(stack), top: stack.top() }
    }

    /// The recorded entry height.
    pub fn entry_top(&self) -> c_int {
        self.top
    }

    /// Disarms the frame: the stack is left as it is when the frame drops.
    pub fn release(&mut self) {
        self.stack = None;
    }

    /// Keeps the current top value (normally a Luau error object) and discards every other
    /// temporary above the entry height, then disarms the frame.
    pub fn preserve_top_and_release(&mut self) {
        let Some(stack) = self.stack.take() else { return };
        let state = stack.state();
        // SAFETY: state is live; indexes are derived from the current top.
        unsafe {
            let current = ffi::lua_gettop(state);
            if current <= self.top {
                return;
            }
            if current > self.top + 1 {
                ffi::lua_replace(state, self.top + 1);
            }
            ffi::lua_settop(state, self.top + 1);
        }
    }
}

impl<'s, 'vm> std::ops::Deref for StackFrame<'s, 'vm> {
    type Target = Stack<'vm>;

    fn deref(&self) -> &Stack<'vm> {
        self.stack.expect("a released StackFrame no longer names a stack")
    }
}

impl Drop for StackFrame<'_, '_> {
    fn drop(&mut self) {
        if let Some(stack) = self.stack {
            // SAFETY: restoring to a height not above the current one is always valid.
            unsafe { ffi::lua_settop(stack.state(), self.top) };
        }
    }
}
