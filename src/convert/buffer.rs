use std::marker::PhantomData;

use super::{FromView, Vector3};
use crate::error::{Error, Result};
use crate::raw::ffi;
use crate::stack::{Scope, Type, ValueView};

/// A borrowed view of a Luau `buffer`: raw bytes owned by Luau, mutable from scripts and from
/// the host.
///
/// Access goes through bounds-checked copies rather than a `&mut [u8]`, so several views of one
/// buffer (or a view held across a call back into Lua that also writes the buffer) never form
/// aliasing Rust references. Bounds failures use the buffer library's own wording so a host
/// method and `buffer.writef32` fail identically.
#[derive(Clone, Copy, Debug)]
pub struct BufferView<'v> {
    data: *mut u8,
    len: usize,
    _slot: PhantomData<&'v ()>,
}

impl<'v> BufferView<'v> {
    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// `isoutofbounds(offset, len, access)` from lbuflib.cpp, with the library's error text.
    fn check(&self, offset: usize, size: usize) -> Result<()> {
        if offset.checked_add(size).is_none_or(|end| end > self.len) {
            return Err(Error::runtime("buffer access out of bounds"));
        }
        Ok(())
    }

    /// Copies `dst.len()` bytes starting at `offset`.
    pub fn read(&self, offset: usize, dst: &mut [u8]) -> Result<()> {
        self.check(offset, dst.len())?;
        // SAFETY: bounds verified; the buffer lives while the slot does, and we copy rather
        // than hold a reference into it.
        unsafe { std::ptr::copy_nonoverlapping(self.data.add(offset), dst.as_mut_ptr(), dst.len()) };
        Ok(())
    }

    /// Writes `src` starting at `offset`.
    pub fn write(&self, offset: usize, src: &[u8]) -> Result<()> {
        self.check(offset, src.len())?;
        unsafe { std::ptr::copy_nonoverlapping(src.as_ptr(), self.data.add(offset), src.len()) };
        Ok(())
    }

    /// A copy of the whole buffer.
    pub fn to_vec(&self) -> Vec<u8> {
        let mut bytes = vec![0u8; self.len];
        self.read(0, &mut bytes).expect("whole-buffer read is in bounds");
        bytes
    }

    pub fn read_f32(&self, offset: usize) -> Result<f32> {
        let mut bytes = [0u8; 4];
        self.read(offset, &mut bytes)?;
        Ok(f32::from_ne_bytes(bytes))
    }

    pub fn write_f32(&self, offset: usize, value: f32) -> Result<()> {
        self.write(offset, &value.to_ne_bytes())
    }

    /// Three consecutive host-order f32 writes, the semantics of OpenMW's `vector:writef32x3`.
    pub fn write_f32x3(&self, offset: usize, vector: Vector3) -> Result<()> {
        self.check(offset, 12)?;
        let mut bytes = [0u8; 12];
        bytes[0..4].copy_from_slice(&vector.x.to_ne_bytes());
        bytes[4..8].copy_from_slice(&vector.y.to_ne_bytes());
        bytes[8..12].copy_from_slice(&vector.z.to_ne_bytes());
        self.write(offset, &bytes)
    }

    pub fn read_f32x3(&self, offset: usize) -> Result<Vector3> {
        let mut bytes = [0u8; 12];
        self.read(offset, &mut bytes)?;
        let component = |i: usize| f32::from_ne_bytes(bytes[i * 4..i * 4 + 4].try_into().expect("4 bytes"));
        Ok(Vector3::new(component(0), component(1), component(2)))
    }

    pub fn read_u8(&self, offset: usize) -> Result<u8> {
        let mut byte = [0u8; 1];
        self.read(offset, &mut byte)?;
        Ok(byte[0])
    }

    pub fn write_u8(&self, offset: usize, value: u8) -> Result<()> {
        self.write(offset, &[value])
    }

    pub fn read_i32(&self, offset: usize) -> Result<i32> {
        let mut bytes = [0u8; 4];
        self.read(offset, &mut bytes)?;
        Ok(i32::from_ne_bytes(bytes))
    }

    pub fn write_i32(&self, offset: usize, value: i32) -> Result<()> {
        self.write(offset, &value.to_ne_bytes())
    }

    pub fn read_f64(&self, offset: usize) -> Result<f64> {
        let mut bytes = [0u8; 8];
        self.read(offset, &mut bytes)?;
        Ok(f64::from_ne_bytes(bytes))
    }

    pub fn write_f64(&self, offset: usize, value: f64) -> Result<()> {
        self.write(offset, &value.to_ne_bytes())
    }
}

impl<'v> FromView<'v> for BufferView<'v> {
    const EXPECTED: &'static str = "buffer";

    fn from_view(view: ValueView<'v>) -> Result<Self> {
        if !view.is_buffer() {
            return Err(view.type_error(Type::Buffer));
        }
        let mut len = 0usize;
        // SAFETY: the slot holds a buffer; its storage is stable while the slot keeps it alive.
        let data = unsafe { ffi::lua_tobuffer(view.state(), view.index(), &mut len) };
        if data.is_null() {
            return Err(view.type_error(Type::Buffer));
        }
        Ok(BufferView { data: data.cast(), len, _slot: PhantomData })
    }

    fn matches(view: ValueView<'v>) -> bool {
        view.is_buffer()
    }
}

/// Creates a zero-filled buffer of `len` bytes on `scope`.
pub fn new_buffer<'s, S: Scope>(scope: &'s S, len: usize) -> Result<BufferView<'s>> {
    // SAFETY: lua_newbuffer raises only for out of memory (fatal at host level, propagated in
    // native calls) and pushes the buffer.
    let data = unsafe { ffi::lua_newbuffer(scope.state(), len) };
    Ok(BufferView { data: data.cast(), len, _slot: PhantomData })
}
