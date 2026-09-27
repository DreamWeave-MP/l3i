//! Direct primitive fields: `lua_registeruserdatadirectfieldget` (Luau 0.740).
//!
//! A registered getter runs from `GETTABLEKS` with no Lua frame, receiving only the userdata
//! payload and a result slot. It must not touch the Lua API and must not fail. Only the value
//! kinds Luau provides setters for can be produced.

use std::ffi::{CString, c_int, c_void};

use crate::convert::Vector3;
use crate::error::{Error, Result};
use crate::raw::ffi;
use crate::runtime::Runtime;
use crate::userdata::Userdata;

/// A value a direct field getter can produce.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum FieldValue {
    Nil,
    Boolean(bool),
    Number(f64),
    Integer(i64),
    Vector(Vector3),
}

/// A direct field getter for `T`: a unit type, so the getter is a plain function pointer with
/// no context.
pub trait DirectField<T: Userdata>: 'static {
    fn get(value: &T) -> FieldValue;
}

/// The Luau callback for `(T, H)`.
unsafe extern "C" fn field_thunk<T: Userdata, H: DirectField<T>>(userdata: *mut c_void, result: *mut c_void) {
    let _guard = crate::raw::trampoline::AbortOnPanic;
    // SAFETY: Luau calls this only for userdata of the tag it was registered on, so the
    // payload is a T; `result` is the destination TValue Luau's setters write.
    unsafe {
        let value = &*userdata.cast::<T>();
        match H::get(value) {
            FieldValue::Nil => ffi::lua_userdatadirectfield_setnil(result),
            FieldValue::Boolean(b) => ffi::lua_userdatadirectfield_setboolean(result, c_int::from(b)),
            FieldValue::Number(n) => ffi::lua_userdatadirectfield_setnumber(result, n),
            FieldValue::Integer(i) => ffi::lua_userdatadirectfield_setinteger64(result, i),
            FieldValue::Vector(v) => ffi::lua_userdatadirectfield_setvector(result, v.x, v.y, v.z),
        }
    }
}

/// Registers `H` as the direct getter for `field` on `T`. `T` must be a registered tagged type
/// with a read-only metatable. Luau offers no query, replacement, or removal for direct
/// fields, so registering a field twice is a logic error here.
pub fn register<T: Userdata, H: DirectField<T>>(runtime: &Runtime, field: &str) -> Result<()> {
    let Some(tag) = T::TAG else {
        return Err(Error::logic(format!("'{}' is untagged; direct fields need a runtime tag", T::NAME)));
    };
    super::require_tag(tag)?;
    if field.is_empty() || field.contains('\0') {
        return Err(Error::logic("Direct userdata field name must be a non-empty C string"));
    }
    let name = CString::new(field).expect("checked for NUL above");
    let stack = runtime.stack();
    stack.with_frame(|frame| {
        let state = frame.state();
        // SAFETY: the metatable check is a balanced push/pop; registration copies the field
        // name into an interned, pinned Luau string.
        unsafe {
            ffi::lua_getuserdatametatable(state, c_int::from(tag));
            if !ffi::lua_istable(state, -1) {
                return Err(Error::logic("Luau userdata tag has no registered metatable"));
            }
            if ffi::lua_getreadonly(state, -1) == 0 {
                return Err(Error::logic("Luau userdata metatable must be read-only"));
            }
            ffi::lua_pop(state, 1);
            ffi::lua_registeruserdatadirectfieldget(state, c_int::from(tag), name.as_ptr(), field_thunk::<T, H>);
        }
        Ok(())
    })
}
