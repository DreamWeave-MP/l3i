//! The audited unsafe substrate. Everything here states the invariant it relies on; nothing
//! outside this module touches `mlua::ffi` for anything the safe layers cannot express.

pub(crate) mod ffi_extra;
pub(crate) mod trampoline;

pub(crate) use mlua::ffi;
