use std::marker::PhantomData;

use super::{FromView, Push, Vector3};
use crate::error::{Error, Result};
use crate::raw::ffi;
use crate::stack::{Scope, Type, ValueView};

/// A borrowed view of a Luau `buffer`: raw bytes owned by Luau, mutable from scripts and from
/// the host.
///
/// Safe access goes through bounds-checked copies, never a Rust slice: a script can pass one
/// buffer to two parameters, so two views of the same storage are ordinary, and safe code
/// could otherwise hold a `&[u8]` while writing the bytes through the other view or through
/// a call back into Lua. The slice forms exist for trusted code that can prove neither
/// happens and are `unsafe` ([`Self::bytes_unchecked`], [`Self::bytes_mut_unchecked`]).
/// Bounds failures use the buffer library's own wording so a host method and `buffer.writef32`
/// fail identically.
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

    /// The bounds check alone, so a packed write fails before its encoder runs.
    pub(crate) fn check_packed(&self, offset: usize, size: usize) -> Result<()> {
        self.check(offset, size)
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

    /// Sets `len` bytes from `offset` to `value`.
    pub fn fill(&self, offset: usize, len: usize, value: u8) -> Result<()> {
        self.check(offset, len)?;
        // SAFETY: bounds verified; a byte store into Luau's writable storage, no reference held.
        unsafe { std::ptr::write_bytes(self.data.add(offset), value, len) };
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

    #[inline(always)]
    fn from_raw_arg(raw: &super::RawValue, view: impl FnOnce() -> ValueView<'v>) -> Result<Self> {
        if raw.tag() == ffi::LUA_TBUFFER {
            // SAFETY: the tag says buffer; the argument slot keeps it alive for the call ('v).
            let (data, len) = unsafe { raw.buffer() };
            return Ok(BufferView { data, len, _slot: PhantomData });
        }
        Self::from_view(view())
    }

    fn matches(view: ValueView<'v>) -> bool {
        view.is_buffer()
    }
}

impl BufferView<'_> {
    /// The buffer's bytes as a slice, without copying.
    ///
    /// # Safety
    ///
    /// While the slice lives, nothing may write the buffer: no call back into Lua (a script
    /// could write it), no write through this or any other view of the same storage (a script
    /// can pass one buffer to several parameters). Those are the rules `lua_tobuffer` imposes
    /// on C; the caller proves them for the slice's whole lifetime.
    #[inline]
    #[must_use]
    pub unsafe fn bytes_unchecked(&self) -> &[u8] {
        // SAFETY: the buffer's storage is stable and at least `len` bytes while the slot that
        // produced this view keeps it alive; the caller upholds the no-write contract above.
        unsafe { std::slice::from_raw_parts(self.data.cast_const(), self.len) }
    }

    /// The buffer's bytes as a mutable slice, without copying.
    ///
    /// # Safety
    ///
    /// As [`Self::bytes_unchecked`], and additionally nothing may read the buffer through
    /// another view while the slice lives.
    #[inline]
    #[must_use]
    pub unsafe fn bytes_mut_unchecked(&mut self) -> &mut [u8] {
        // SAFETY: as `bytes_unchecked`; Luau buffers are writable byte storage.
        unsafe { std::slice::from_raw_parts_mut(self.data, self.len) }
    }

    /// A bounds-checked sub-range as a new view over the same storage.
    pub fn range(&self, offset: usize, len: usize) -> Result<BufferView<'_>> {
        self.check(offset, len)?;
        // SAFETY: the range lies inside the buffer.
        Ok(BufferView { data: unsafe { self.data.add(offset) }, len, _slot: PhantomData })
    }
}

/// Immutable bytes from either a Lua string or a Luau buffer, without normalising: APIs that
/// accept "some bytes" read them through this and never copy to decide.
#[derive(Clone, Copy, Debug)]
pub enum BytesView<'v> {
    String(&'v [u8]),
    Buffer(BufferView<'v>),
}

impl BytesView<'_> {
    pub fn len(&self) -> usize {
        match self {
            BytesView::String(bytes) => bytes.len(),
            BytesView::Buffer(buffer) => buffer.len(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Copies `dst.len()` bytes starting at `offset`.
    pub fn read(&self, offset: usize, dst: &mut [u8]) -> Result<()> {
        match self {
            BytesView::String(bytes) => {
                let end = offset.checked_add(dst.len()).filter(|end| *end <= bytes.len());
                let Some(end) = end else { return Err(Error::runtime("buffer access out of bounds")) };
                dst.copy_from_slice(&bytes[offset..end]);
                Ok(())
            }
            BytesView::Buffer(buffer) => buffer.read(offset, dst),
        }
    }

    /// The bytes as a slice, without copying.
    ///
    /// # Safety
    ///
    /// For a buffer, [`BufferView::bytes_unchecked`]'s contract; a string's bytes are immutable
    /// and carry no condition.
    #[inline]
    #[must_use]
    pub unsafe fn bytes_unchecked(&self) -> &[u8] {
        match self {
            BytesView::String(bytes) => bytes,
            // SAFETY: forwarded contract.
            BytesView::Buffer(buffer) => unsafe { buffer.bytes_unchecked() },
        }
    }

    pub fn to_vec(&self) -> Vec<u8> {
        match self {
            BytesView::String(bytes) => bytes.to_vec(),
            BytesView::Buffer(buffer) => buffer.to_vec(),
        }
    }
}

impl<'v> FromView<'v> for BytesView<'v> {
    const EXPECTED: &'static str = "string or buffer";

    fn from_view(view: ValueView<'v>) -> Result<Self> {
        if view.is_buffer() {
            return BufferView::from_view(view).map(BytesView::Buffer);
        }
        if view.type_of() == Type::String {
            return <&[u8]>::from_view(view).map(BytesView::String);
        }
        Err(view.type_error(Type::Buffer))
    }

    #[inline(always)]
    fn from_raw_arg(raw: &super::RawValue, view: impl FnOnce() -> ValueView<'v>) -> Result<Self> {
        match raw.tag() {
            ffi::LUA_TSTRING => <&[u8]>::from_raw_arg(raw, view).map(BytesView::String),
            ffi::LUA_TBUFFER => BufferView::from_raw_arg(raw, view).map(BytesView::Buffer),
            _ => Self::from_view(view()),
        }
    }

    fn matches(view: ValueView<'v>) -> bool {
        view.is_buffer() || view.type_of() == Type::String
    }
}

/// Creates a zero-filled buffer of `len` bytes on `scope`.
/// Bytes returned to a script as a new Luau `buffer` of exactly their length: the result type
/// for operations that produce bytes (a decompressed block, a digest, an encoded string).
/// Pushing allocates the buffer and copies once; a `Vec<u8>` pushes a string instead.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct NewBuffer(pub Vec<u8>);

impl Push for NewBuffer {
    fn push_into<'s, S: Scope>(&self, scope: &'s S) -> Result<ValueView<'s>> {
        let mut buffer = new_buffer(scope, self.0.len())?;
        // SAFETY: the buffer was created by this call and no other view of it exists yet.
        unsafe { buffer.bytes_mut_unchecked() }.copy_from_slice(&self.0);
        Ok(scope.top_value())
    }
}

pub fn new_buffer<'s, S: Scope>(scope: &'s S, len: usize) -> Result<BufferView<'s>> {
    // SAFETY: lua_newbuffer raises only for out of memory (fatal at host level, propagated in
    // native calls) and pushes the buffer.
    let data = unsafe { ffi::lua_newbuffer(scope.state(), len) };
    Ok(BufferView { data: data.cast(), len, _slot: PhantomData })
}
