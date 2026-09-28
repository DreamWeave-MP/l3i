use super::{FromView, Push};
use crate::raw::ffi;
use crate::stack::{Scope, Type, ValueView};

/// A native Luau `vector`: three `f32` components (the VM is built 3-wide). Converting costs one
/// tag check and a 12-byte copy; no userdata, no allocation.
#[derive(Clone, Copy, Debug, PartialEq, Default)]
#[repr(C)]
pub struct Vector3 {
    pub x: f32,
    pub y: f32,
    pub z: f32,
}

impl Vector3 {
    pub const fn new(x: f32, y: f32, z: f32) -> Vector3 {
        Vector3 { x, y, z }
    }

    pub const fn to_array(self) -> [f32; 3] {
        [self.x, self.y, self.z]
    }
}

impl From<[f32; 3]> for Vector3 {
    fn from([x, y, z]: [f32; 3]) -> Vector3 {
        Vector3 { x, y, z }
    }
}

impl From<Vector3> for [f32; 3] {
    fn from(v: Vector3) -> [f32; 3] {
        v.to_array()
    }
}

impl<'v> FromView<'v> for Vector3 {
    const EXPECTED: &'static str = "vector";

    #[inline]
    fn from_view(view: ValueView<'v>) -> crate::error::Result<Vector3> {
        if !view.exists() {
            return Err(view.type_error(Type::Vector));
        }
        let mut components = [0f32; 3];
        // SAFETY: the slot exists; the helper writes three floats only for a vector.
        if unsafe { ffi::l3i_read_vector(view.state(), view.index(), components.as_mut_ptr()) } == 0 {
            return Err(view.type_error(Type::Vector));
        }
        Ok(Vector3 { x: components[0], y: components[1], z: components[2] })
    }

    #[inline(always)]
    fn from_raw_arg(raw: &super::RawValue, view: impl FnOnce() -> ValueView<'v>) -> crate::error::Result<Vector3> {
        if raw.tag() == ffi::LUA_TVECTOR {
            let [x, y, z] = raw.vector();
            Ok(Vector3 { x, y, z })
        } else {
            Err(view().type_error(Type::Vector))
        }
    }

    #[inline]
    fn matches(view: ValueView<'v>) -> bool {
        view.is_vector()
    }
}

impl Push for Vector3 {
    #[inline]
    fn push_into<'s, S: Scope>(&self, scope: &'s S) -> crate::error::Result<ValueView<'s>> {
        unsafe { ffi::lua_pushvector(scope.state(), self.x, self.y, self.z) };
        Ok(scope.top_value())
    }
}
