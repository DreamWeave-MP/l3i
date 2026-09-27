//! The audited unsafe substrate. Everything here states the invariant it relies on; nothing
//! outside this module touches the C API for anything the safe layers cannot express.

pub(crate) mod ffi;
pub(crate) mod protect;
pub(crate) mod trampoline;
