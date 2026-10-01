//! Luau's value layout, read directly.
//!
//! A Luau stack slot is a 16-byte `TValue`: an 8-byte value union, one 32-bit `extra` word, and
//! the 32-bit type tag. The binder reads argument slots through this mirror instead of through
//! Luau's index resolution, which turns each scalar read into a tag compare and a load. The
//! layout belongs to the pinned Luau build: `csrc/extra.cpp` checks every offset at compile time
//! and [`self_test`] checks the reads against the API on every runtime creation, so a Luau bump
//! that moves a byte fails at once rather than misreading.

use std::ffi::c_void;

use crate::raw::ffi;

const _: () = assert!(cfg!(target_endian = "little"), "the value mirror assumes little-endian layout");

/// One Luau stack slot, as laid out by this build (`lobject.h`: `Value value; int extra[1]; int tt`).
#[repr(C)]
pub struct RawValue {
    bits: u64,
    extra: i32,
    tt: i32,
}

const _: () = assert!(std::mem::size_of::<RawValue>() == 16);

impl RawValue {
    /// The type tag (`LUA_T*`).
    #[inline(always)]
    pub fn tag(&self) -> i32 {
        self.tt
    }

    /// The payload of a `LUA_TNUMBER` slot.
    #[inline(always)]
    pub fn number(&self) -> f64 {
        f64::from_bits(self.bits)
    }

    /// The payload of a `LUA_TINTEGER` slot.
    #[inline(always)]
    pub fn integer(&self) -> i64 {
        self.bits as i64
    }

    /// The payload of a `LUA_TBOOLEAN` slot (an `int` in the low word).
    #[inline(always)]
    pub fn boolean(&self) -> bool {
        (self.bits as u32) != 0
    }

    /// The payload of a `LUA_TVECTOR` slot: two floats in the value word, the third in `extra`.
    #[inline(always)]
    pub fn vector(&self) -> [f32; 3] {
        [f32::from_bits(self.bits as u32), f32::from_bits((self.bits >> 32) as u32), f32::from_bits(self.extra as u32)]
    }

    /// The tag byte and payload address of a `LUA_TUSERDATA` slot (`Udata`: the tag at offset
    /// 3, the payload at offset 16).
    ///
    /// # Safety
    /// The slot must hold a live full userdata (tag `LUA_TUSERDATA`).
    #[inline(always)]
    pub unsafe fn userdata(&self) -> (u8, *mut c_void) {
        let object = self.bits as usize as *mut u8;
        // SAFETY: the caller checked the tag; a live Udata is at least its header long.
        unsafe { (*object.add(3), object.add(16).cast::<c_void>()) }
    }

    /// The bytes of a `LUA_TSTRING` slot (`TString`: the length at offset 20, the bytes at 24).
    ///
    /// # Safety
    /// The slot must hold a live string, and the slice must not outlive the slot's hold on it.
    #[inline(always)]
    pub unsafe fn string<'a>(&self) -> &'a [u8] {
        let object = self.bits as usize as *const u8;
        // SAFETY: the caller checked the tag; a live TString holds `len` bytes after its header,
        // and strings are immutable.
        unsafe {
            let len = object.add(20).cast::<u32>().read();
            std::slice::from_raw_parts(object.add(24), len as usize)
        }
    }

    /// The storage and length of a `LUA_TBUFFER` slot (`Buffer`: the length at offset 4, the
    /// bytes at 8).
    ///
    /// # Safety
    /// The slot must hold a live buffer.
    #[inline(always)]
    pub unsafe fn buffer(&self) -> (*mut u8, usize) {
        let object = self.bits as usize as *mut u8;
        // SAFETY: the caller checked the tag; a live Buffer is at least its header long.
        unsafe { (object.add(8), object.add(4).cast::<u32>().read() as usize) }
    }
}

/// Reads known values through the mirror and compares with the API. Runs once per runtime.
///
/// # Safety
/// `state` is a live thread with the standard library available and stack room for a few values.
pub(crate) unsafe fn self_test(state: *mut ffi::lua_State) -> crate::error::Result<()> {
    use crate::error::Error;
    unsafe {
        let top = ffi::lua_gettop(state);
        ffi::lua_pushnumber(state, 2.5);
        ffi::lua_pushinteger64(state, 0x1234_5678_9ABC_DEF0u64 as i64);
        ffi::lua_pushboolean(state, 1);
        ffi::lua_pushvector(state, 1.5, -2.5, 3.5);
        ffi::lua_pushlstring(state, c"mirror".as_ptr(), 6);
        let buffer = ffi::lua_newbuffer(state, 5).cast::<u8>();
        let ok = {
            let slot = |offset: i32| ffi::l3i_stack_slot(state, top + offset).as_ref();
            let number = slot(1).is_some_and(|v| v.tag() == ffi::LUA_TNUMBER && v.number() == 2.5);
            let integer =
                slot(2).is_some_and(|v| v.tag() == ffi::LUA_TINTEGER && v.integer() == 0x1234_5678_9ABC_DEF0u64 as i64);
            let boolean = slot(3).is_some_and(|v| v.tag() == ffi::LUA_TBOOLEAN && v.boolean());
            let vector = slot(4).is_some_and(|v| v.tag() == ffi::LUA_TVECTOR && v.vector() == [1.5, -2.5, 3.5]);
            let string = slot(5).is_some_and(|v| v.tag() == ffi::LUA_TSTRING && v.string() == b"mirror");
            let buffer = slot(6).is_some_and(|v| v.tag() == ffi::LUA_TBUFFER && v.buffer() == (buffer, 5));
            number && integer && boolean && vector && string && buffer
        };
        ffi::lua_settop(state, top);
        if ok {
            Ok(())
        } else {
            Err(Error::logic("Luau's value layout does not match the mirror l3i reads arguments through"))
        }
    }
}
