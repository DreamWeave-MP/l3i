//! Borrowed views over a live Luau stack: the hot-path tier.
//!
//! Nothing here pins a registry reference. A [`ValueView`] names one stack slot and is valid
//! exactly as long as the raw Lua index it names. Rust proves that the way the C++ binder only
//! asserted it:
//!
//! - [`Stack`] is the exclusive handle to a thread's stack. It has no `pop`; only frames pop.
//! - Temporaries are pushed through a [`Frame`], and the view returned borrows that frame
//!   object. Dropping the frame while such a view is alive is a compile error.
//! - [`Stack::with_frame`] / [`Frame::with_frame`] take a higher-ranked closure, so a view
//!   cannot be returned out of the frame that made it.
//! - Slots below every frame's floor (a native call's arguments, values the host pushed before
//!   opening a frame) are read through [`Stack::at`] and borrow the stack instead.
//!
//! Frame topology is strictly nested. A stack or a frame has at most one open child frame:
//! opening a second sibling panics at the opening line (it would let one drop pop the other's
//! slots), pushing into a scope while its child is open is refused, and popping or releasing a
//! frame needs `&mut`, so no live view of that frame can survive the pop. Together these make a
//! view of a reused slot unrepresentable rather than merely detectable. Two runtime guards
//! remain as belt and braces: a frame's drop restores its floor only downwards, and every view
//! access is bounded by the live top.

mod frame;
mod table;
mod view;

#[cfg(test)]
mod tests;

use std::cell::Cell;
use std::ffi::{c_char, c_int};
use std::marker::PhantomData;

use crate::error::{Error, Result};
use crate::raw::ffi;

pub use frame::Frame;
pub use table::TableView;
pub use view::{Type, ValueView};

mod sealed {
    pub trait Sealed {}
    impl Sealed for super::Stack<'_> {}
    impl Sealed for super::Frame<'_> {}
}

/// Somewhere values can be pushed: the call-level [`Stack`] or a temporary [`Frame`].
/// Helpers that create values (userdata, functions) are generic over it so a result can be
/// pushed at the call level while a temporary goes through a frame.
pub trait Scope: sealed::Sealed {
    #[doc(hidden)]
    fn state(&self) -> *mut ffi::lua_State;
    /// A view of slot `index` bound to this scope.
    fn at(&self, index: c_int) -> ValueView<'_>;
    /// Opens a temporary frame on this scope.
    fn frame(&self) -> Frame<'_>;
    /// The most recently pushed value.
    fn top_value(&self) -> ValueView<'_> {
        self.at(-1)
    }
    /// Runs `body` inside a temporary frame; the result cannot borrow the frame.
    fn with_frame<R>(&self, body: impl FnOnce(&Frame<'_>) -> Result<R>) -> Result<R> {
        let frame = self.frame();
        body(&frame)
    }
    /// Pushes a Rust value through its [`crate::convert::Push`] conversion.
    fn push<T: crate::convert::Push + ?Sized>(&self, value: &T) -> Result<ValueView<'_>>
    where
        Self: Sized,
    {
        value.push(self)
    }
}

impl Scope for Stack<'_> {
    fn state(&self) -> *mut ffi::lua_State {
        self.state
    }
    fn at(&self, index: c_int) -> ValueView<'_> {
        Stack::at(self, index)
    }
    fn frame(&self) -> Frame<'_> {
        Stack::frame(self)
    }
}

impl Scope for Frame<'_> {
    fn state(&self) -> *mut ffi::lua_State {
        Frame::state(self)
    }
    fn at(&self, index: c_int) -> ValueView<'_> {
        Frame::at(self, index)
    }
    fn frame(&self) -> Frame<'_> {
        Frame::frame(self)
    }
}

/// The exclusive handle to one Lua thread's stack.
///
/// `'vm` is the lifetime for which the caller guarantees the `lua_State` stays alive: inside a
/// native callback that is the callback's frame, host side it is the borrow of the owning
/// [`crate::runtime::Runtime`]. Not `Clone`: two handles would let one pop what the other views.
pub struct Stack<'vm> {
    state: *mut ffi::lua_State,
    /// True outside any Lua call. Operations that can raise a Luau error then run under
    /// [`crate::raw::protect::protected_call`], because no `pcall` above us would catch them.
    host_level: bool,
    /// Whether a frame opened directly on this stack is currently alive.
    child_open: Cell<bool>,
    _vm: PhantomData<&'vm mut ()>,
}

impl<'vm> Stack<'vm> {
    /// # Safety
    /// `state` must be a live Luau thread that outlives `'vm`, and no other `Stack` may exist
    /// for it while this one does (Luau VMs are single-threaded; a native callback is the
    /// only code running on its thread).
    pub(crate) unsafe fn from_raw(state: *mut ffi::lua_State, host_level: bool) -> Self {
        debug_assert!(!state.is_null(), "Stack requires a live Lua state");
        Stack { state, host_level, child_open: Cell::new(false), _vm: PhantomData }
    }

    /// True when this stack is used from host code rather than inside a Lua call.
    pub fn is_host_level(&self) -> bool {
        self.host_level
    }

    #[inline]
    pub fn top(&self) -> c_int {
        // SAFETY: state is live for 'vm.
        unsafe { ffi::lua_gettop(self.state) }
    }

    /// A view of slot `index`, borrowing the stack. Negative indexes resolve against the current
    /// top; `0`, out-of-range indexes, and indexes above the top are views of [`Type::None`].
    pub fn at(&self, index: c_int) -> ValueView<'_> {
        ValueView::resolve(self.state, index)
    }

    /// Opens a temporary frame. Values pushed through it are popped when it drops.
    ///
    /// # Panics
    /// If a frame opened from this stack is still alive: sibling frames would let one drop pop
    /// the other's slots. Open the second frame from the first instead.
    pub fn frame(&self) -> Frame<'_> {
        Frame::open(self.state, self.host_level, &self.child_open)
    }

    /// Runs `body` inside a temporary frame. The closure is higher-ranked over the frame, so
    /// its result cannot borrow anything the frame pushed.
    pub fn with_frame<R>(&self, body: impl FnOnce(&Frame<'_>) -> Result<R>) -> Result<R> {
        let frame = self.frame();
        body(&frame)
    }

    /// Ensures `extra` free slots, as `lua_checkstack`.
    pub fn check(&self, extra: c_int) -> Result<()> {
        if unsafe { ffi::lua_checkstack(self.state, extra) } == 0 {
            return Err(Error::runtime("Lua error: stack overflow"));
        }
        Ok(())
    }

    fn assert_no_open_frame(&self) {
        debug_assert!(!self.child_open.get(), "push through the open Frame, not the Stack beneath it");
    }

    // Pushes at the call level: results of a native function, or host setup before any frame.
    // Nothing pops these within the current scope, so their views borrow the stack.

    pub fn push_nil(&self) -> ValueView<'_> {
        self.assert_no_open_frame();
        // SAFETY: live state; Luau guarantees LUA_MINSTACK free slots on entry and callers use
        // `check` before pushing more than that.
        unsafe { ffi::lua_pushnil(self.state) };
        self.at(-1)
    }

    pub fn push_boolean(&self, value: bool) -> ValueView<'_> {
        self.assert_no_open_frame();
        unsafe { ffi::lua_pushboolean(self.state, c_int::from(value)) };
        self.at(-1)
    }

    pub fn push_number(&self, value: f64) -> ValueView<'_> {
        self.assert_no_open_frame();
        unsafe { ffi::lua_pushnumber(self.state, value) };
        self.at(-1)
    }

    pub fn push_string(&self, value: &str) -> ValueView<'_> {
        self.assert_no_open_frame();
        unsafe { ffi::lua_pushlstring(self.state, value.as_ptr().cast(), value.len()) };
        self.at(-1)
    }

    /// Pushes a copy of `value`, which must belong to this stack.
    pub fn push_value(&self, value: ValueView<'_>) -> Result<ValueView<'_>> {
        self.assert_no_open_frame();
        push_copy(self.state, value)?;
        Ok(self.at(-1))
    }

    /// Pushes a C function with an already-retained debug name (see [`crate::debug_name`]).
    ///
    /// # Safety
    /// `debug_name` is null or a pointer that stays valid until the VM closes.
    pub unsafe fn push_c_function(&self, function: ffi::lua_CFunction, debug_name: *const c_char) -> ValueView<'_> {
        self.assert_no_open_frame();
        unsafe { ffi::lua_pushcfunction(self.state, function, debug_name) };
        self.at(-1)
    }
}

/// `lua_pushvalue` for a view of this state, or `lua_xpush` for a view of another thread of the
/// same VM. Nonexistent slots and other VMs are logic errors, as in the C++ `pushLuaValue`.
pub(crate) fn push_copy(state: *mut ffi::lua_State, value: ValueView<'_>) -> Result<()> {
    if value.type_of() == Type::None {
        return Err(Error::logic("Cannot push a nonexistent Lua stack value"));
    }
    // SAFETY: `value` proved its slot exists on its own live thread.
    unsafe {
        if value.state() == state {
            ffi::lua_pushvalue(state, value.index());
        } else if same_vm(state, value.state()) {
            ffi::lua_xpush(value.state(), state, value.index());
        } else {
            return Err(Error::logic("Lua value belongs to a different VM"));
        }
    }
    Ok(())
}

/// True when both threads belong to one VM.
pub(crate) fn same_vm(left: *mut ffi::lua_State, right: *mut ffi::lua_State) -> bool {
    !left.is_null() && !right.is_null() && unsafe { ffi::lua_mainthread(left) == ffi::lua_mainthread(right) }
}

pub(crate) fn checked_capacity(capacity: usize) -> Result<c_int> {
    c_int::try_from(capacity).map_err(|_| Error::logic("Lua table capacity exceeds the supported range"))
}
