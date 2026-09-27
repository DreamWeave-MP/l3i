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

    fn from_view(view: ValueView<'v>) -> crate::error::Result<Vector3> {
        if !view.is_vector() {
            return Err(view.type_error(Type::Vector));
        }
        // SAFETY: the slot holds a vector; lua_tovector points at LUA_VECTOR_SIZE floats that
        // live inside the TValue for as long as the slot does.
        unsafe {
            let components = ffi::lua_tovector(view.state(), view.index());
            Ok(Vector3 { x: *components, y: *components.add(1), z: *components.add(2) })
        }
    }

    fn matches(view: ValueView<'v>) -> bool {
        view.is_vector()
    }
}

impl Push for Vector3 {
    fn push<'s, S: Scope>(&self, scope: &'s S) -> crate::error::Result<ValueView<'s>> {
        unsafe { ffi::lua_pushvector(scope.state(), self.x, self.y, self.z) };
        Ok(scope.top_value())
    }
}
