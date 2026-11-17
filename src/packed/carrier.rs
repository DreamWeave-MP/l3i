//! The Luau value a packed scalar's 64 bits ride in: the one place that knows.
//!
//! [`super::Packed`], [`crate::raster::Color16`] and the other wide scalars read and push through
//! `read`, `read_raw` and `push` and never name a Luau type themselves. `build.rs` selects
//! exactly one implementation as `cfg(l3i_carrier = "...")`:
//!
//! - **light**, on x86-64 and AArch64 only (an allowlist, not a pointer-width test): a tagged
//!   light userdata. The pointer word holds the 64 bits and `extra` the light userdata tag. No
//!   allocation and nothing for the GC. The bits are copied into the slot by
//!   `l3i_push_light_bits` and read back as a `u64` through [`RawValue`]; nothing converts them to
//!   a pointer and nothing dereferences them. The VM only copies the word and compares it.
//! - **boxed**, everywhere else: an untagged full userdata of sixteen bytes (the bits, then the
//!   light tag) whose metatable is its kind's, interned through a weak-valued registry table keyed
//!   by tag and bits, so equal values are one object and `==`, `rawequal` and table keys behave as
//!   on the light carrier. A metatable carries `__type`, so `typeof` answers the kind's name.
//! - **integer**, on Luau 0.746 only, selected with `L3I_CARRIER=integer`: the Luau `integer` the
//!   scalars rode in before Luau 0.747 removed the type, kept as the oracle the others are
//!   measured against.
//!
//! **The nibble is the kind.** A read here checks the Luau type alone (light userdata, a boxed
//! scalar, an integer) and returns the bits; the caller checks the kind nibble in them, exactly as
//! lowered code does. The light tag is written on every push so that `typeof`, equality and table
//! keys are right, but no check reads it: in native code the nibble check is free and an exact tag
//! check is not, and two paths that checked different things would disagree. A host light userdata
//! whose pointer has a non-zero top nibble would read as a packed scalar, so the host push refuses
//! one ([`crate::memory::LightUserdata`]).

use std::ffi::c_int;

use crate::convert::RawValue;
use crate::error::Result;
use crate::raw::ffi;
use crate::stack::{Scope, ValueView};

/// A light userdata tag in l3i's band, `128..160`.
pub type LightTag = u8;

/// The light tag of nibble kind `k` is `NIBBLE_TAG_BASE + k` (`129..=143`).
pub const NIBBLE_TAG_BASE: LightTag = 128;
/// The first light tag of the wide kinds, which have no nibble.
pub const WIDE_TAG_FIRST: LightTag = 144;
/// One past l3i's last light tag (`ffi::LUA_LUTAG_LIMIT`).
pub const TAG_END: LightTag = 160;
/// [`crate::raster::Color16`]'s light tag.
pub const COLOR16_TAG: LightTag = 144;
/// [`crate::convert::Bits64`]'s light tag.
pub const BITS64_TAG: LightTag = 145;

const _: () = assert!(TAG_END as c_int == ffi::LUA_LUTAG_LIMIT);
const _: () = assert!(NIBBLE_TAG_BASE as c_int == ffi::HOST_LUTAG_LIMIT);
const _: () = assert!(NIBBLE_TAG_BASE + super::LAST_KIND < WIDE_TAG_FIRST);

/// The light tag of nibble kind `kind`.
#[inline(always)]
#[must_use]
pub const fn nibble_tag(kind: u8) -> LightTag {
    NIBBLE_TAG_BASE + kind
}

/// Which carrier this build uses, for messages and documentation.
#[cfg(l3i_carrier = "light")]
pub const CARRIER: &str = "light userdata";
/// Which carrier this build uses, for messages and documentation.
#[cfg(l3i_carrier = "boxed")]
pub const CARRIER: &str = "boxed userdata";
/// Which carrier this build uses, for messages and documentation.
#[cfg(l3i_carrier = "integer")]
pub const CARRIER: &str = "integer";

/// The bits of the packed scalar in `view`, of any kind, or `None` when it holds no packed scalar.
#[inline]
pub(crate) fn read(view: ValueView<'_>) -> Option<u64> {
    // SAFETY: the view names a live slot of a live thread; the mirror is proven at creation.
    let slot = unsafe { ffi::l3i_stack_slot(view.state(), view.index()).as_ref() }?;
    read_raw(slot, || view.state())
}

/// The bits of the packed scalar in `raw`, of any kind, or `None`. `state` is only called by the
/// boxed carrier, which needs the VM's metatables to recognise its own values.
#[inline(always)]
pub(crate) fn read_raw(raw: &RawValue, state: impl FnOnce() -> *mut ffi::lua_State) -> Option<u64> {
    imp::read_raw(raw, state)
}

/// Pushes `bits` as a packed scalar under light tag `tag`. Fails only where the carrier allocates
/// (the boxed one), with the VM's memory error.
#[inline(always)]
pub(crate) fn push<S: Scope>(scope: &S, bits: u64, tag: LightTag) -> Result<()> {
    debug_assert!((NIBBLE_TAG_BASE + 1..TAG_END).contains(&tag), "light tag {tag} is outside l3i's band");
    debug_assert!(
        tag >= WIDE_TAG_FIRST || (bits >> 60) as u8 == tag - NIBBLE_TAG_BASE,
        "a nibble kind's light tag and kind nibble must agree (tag {tag}, bits {bits:#018x})"
    );
    imp::push(scope, bits, tag)
}

/// Names light tag `tag` in this VM, once: `typeof` answers `name` for every value pushed under
/// it. Runs at runtime creation and when a kind is registered, at host level and protected.
///
/// # Safety
/// `state` is a live main thread at host level.
pub(crate) unsafe fn name_tag(state: *mut ffi::lua_State, tag: LightTag, name: &'static str) -> Result<()> {
    let name = std::ffi::CString::new(name).map_err(|_| crate::error::Error::logic("packed kind name contains NUL"))?;
    // SAFETY: forwarded contract.
    unsafe { imp::name_tag(state, tag, &name) }
}

/// Proves at runtime creation that this platform and VM keep a packed scalar whole: every single
/// bit, all ones, the patterns that are not canonical pointers, through a push, a raw read,
/// equality, inequality under another tag, and a table key.
///
/// # Safety
/// `state` is a live main thread at host level with the standard library open.
pub(crate) unsafe fn self_test(state: *mut ffi::lua_State) -> Result<()> {
    // SAFETY: forwarded contract.
    unsafe { imp::self_test(state) }
}

/// Bit patterns every carrier must carry exactly.
fn self_test_patterns() -> impl Iterator<Item = u64> {
    (0..64).map(|bit| 1u64 << bit).chain([
        0,
        u64::MAX,
        0x8000_0000_0000_0000,
        0x0000_8000_0000_0000,
        0xFFFF_7FFF_FFFF_FFFF,
        0x1234_5678_9ABC_DEF0,
    ])
}

#[cfg(l3i_carrier = "light")]
mod imp {
    use super::{LightTag, Result, Scope, self_test_patterns};
    use crate::convert::RawValue;
    use crate::error::Error;
    use crate::raw::ffi;

    #[inline(always)]
    pub(super) fn read_raw(raw: &RawValue, _state: impl FnOnce() -> *mut ffi::lua_State) -> Option<u64> {
        (raw.tag() == ffi::LUA_TLIGHTUSERDATA).then(|| raw.pointer_bits())
    }

    #[inline(always)]
    pub(super) fn push<S: Scope>(scope: &S, bits: u64, tag: LightTag) -> Result<()> {
        // SAFETY: writes one slot after push_fixed makes room for it; the tag is in l3i's band,
        // below LUA_LUTAG_LIMIT.
        unsafe {
            crate::stack::push_fixed(scope.state(), |state| {
                ffi::l3i_push_light_bits(state, bits, std::ffi::c_int::from(tag));
            })
        }
    }

    pub(super) unsafe fn name_tag(state: *mut ffi::lua_State, tag: LightTag, name: &std::ffi::CStr) -> Result<()> {
        let tag = std::ffi::c_int::from(tag);
        // SAFETY: forwarded contract; Luau refuses to rename a tag, so a named one is left alone.
        unsafe {
            if !ffi::lua_getlightuserdataname(state, tag).is_null() {
                return Ok(());
            }
            crate::raw::protect::protected_call(state, 0, 0, |state| {
                ffi::lua_setlightuserdataname(state, tag, name.as_ptr());
                0
            })
        }
    }

    pub(super) unsafe fn self_test(state: *mut ffi::lua_State) -> Result<()> {
        const TAG: std::ffi::c_int = super::BITS64_TAG as std::ffi::c_int;
        const OTHER: std::ffi::c_int = super::COLOR16_TAG as std::ffi::c_int;
        // SAFETY: forwarded contract. Each round pushes four values and a table and settles the
        // stack back to `top`; the table allocation is protected.
        unsafe {
            let top = ffi::lua_gettop(state);
            if ffi::lua_checkstack(state, 6) == 0 {
                return Err(Error::runtime("Lua error: stack overflow"));
            }
            for bits in self_test_patterns() {
                ffi::l3i_push_light_bits(state, bits, TAG);
                ffi::l3i_push_light_bits(state, bits, TAG);
                ffi::l3i_push_light_bits(state, bits, OTHER);
                let slot = |offset: i32| ffi::l3i_stack_slot(state, top + offset).as_ref();
                let read = slot(1).is_some_and(|v| {
                    v.tag() == ffi::LUA_TLIGHTUSERDATA && v.pointer_bits() == bits && v.extra() == TAG
                });
                let equal = ffi::lua_rawequal(state, top + 1, top + 2) != 0;
                let unequal = ffi::lua_rawequal(state, top + 1, top + 3) == 0;
                let mut keyed = false;
                // The three values move into the protected frame as its arguments 1, 2 and 3.
                crate::raw::protect::protected_call(state, 3, 0, |state| {
                    ffi::lua_createtable(state, 0, 1);
                    ffi::lua_pushvalue(state, 1);
                    ffi::lua_pushboolean(state, 1);
                    ffi::lua_rawset(state, -3);
                    ffi::lua_pushvalue(state, 2);
                    let hit = ffi::lua_rawget(state, -2) == ffi::LUA_TBOOLEAN;
                    ffi::lua_pushvalue(state, 3);
                    let miss = ffi::lua_rawget(state, -3) == ffi::LUA_TNIL;
                    keyed = hit && miss;
                    0
                })?;
                ffi::lua_settop(state, top);
                if !(read && equal && unequal && keyed) {
                    return Err(Error::logic(format!(
                        "the light userdata carrier does not keep packed scalar {bits:#018x} whole on this platform \
                         (read {read}, equal {equal}, unequal under another tag {unequal}, table key {keyed})"
                    )));
                }
            }
            Ok(())
        }
    }
}

#[cfg(l3i_carrier = "boxed")]
mod imp {
    use std::ffi::{c_int, c_void};

    use super::{LightTag, NIBBLE_TAG_BASE, Result, Scope, TAG_END, self_test_patterns};
    use crate::convert::RawValue;
    use crate::error::Error;
    use crate::raw::ffi;

    /// The payload: the bits, then the light tag. Its size is part of the recognition check.
    const PAYLOAD: usize = 16;

    #[inline]
    pub(super) fn read_raw(raw: &RawValue, state: impl FnOnce() -> *mut ffi::lua_State) -> Option<u64> {
        if raw.tag() != ffi::LUA_TUSERDATA {
            return None;
        }
        // SAFETY: a full userdata slot holds a live Udata.
        let (tag, payload) = unsafe { raw.userdata() };
        if tag != 0 {
            return None;
        }
        // SAFETY: as above; len and metatable are in the header.
        let (len, metatable) = unsafe { (raw.userdata_len(), raw.userdata_metatable()) };
        if len != PAYLOAD || metatable.is_null() {
            return None;
        }
        // SAFETY: the payload is sixteen bytes.
        let light = unsafe { payload.cast::<u8>().add(8).read() };
        if !(NIBBLE_TAG_BASE..TAG_END).contains(&light) {
            return None;
        }
        // SAFETY: the state is live; only l3i sets these metatables, which are read-only.
        let shared = unsafe { crate::runtime::shared_for(state()) }?;
        (shared.boxed_metatable(light) == metatable)
            // SAFETY: sixteen bytes, the first eight the bits.
            .then(|| unsafe { payload.cast::<u64>().read_unaligned() })
    }

    /// The registry references a push of `tag` needs: the interning table and the kind's
    /// metatable.
    fn references(state: *mut ffi::lua_State, tag: LightTag) -> Result<(c_int, c_int)> {
        // SAFETY: the state is live.
        let shared = unsafe { crate::runtime::shared_for(state) }
            .ok_or_else(|| Error::logic("a packed scalar needs an l3i runtime"))?;
        let intern = shared.boxed_intern();
        let metatable = shared.boxed_metatable_ref(tag);
        if intern == ffi::LUA_NOREF || metatable == ffi::LUA_NOREF {
            return Err(Error::logic(format!("light tag {tag} has no boxed metatable in this runtime")));
        }
        Ok((intern, metatable))
    }

    /// Pushes the interned box for `(tag, bits)`, making it on a miss. Raises out of memory.
    ///
    /// # Safety
    /// `state` is live with room for one value, inside a protected call or a Lua call.
    unsafe fn push_in(state: *mut ffi::lua_State, bits: u64, tag: LightTag, intern: c_int, metatable: c_int) {
        let mut key = [0u8; 9];
        key[0] = tag;
        key[1..].copy_from_slice(&bits.to_le_bytes());
        // SAFETY: forwarded contract; the sequence balances to one pushed value.
        unsafe {
            ffi::luaL_checkstack(state, 5, c"packed scalar".as_ptr());
            ffi::lua_rawgeti(state, ffi::LUA_REGISTRYINDEX, intern); // [intern]
            ffi::lua_pushlstring(state, key.as_ptr().cast(), key.len()); // [intern, key]
            ffi::lua_pushvalue(state, -1);
            if ffi::lua_rawget(state, -3) == ffi::LUA_TUSERDATA {
                // [intern, key, value]: the weak entry is live, so this is the value.
                ffi::lua_replace(state, -3);
                ffi::lua_pop(state, 1);
                return;
            }
            ffi::lua_pop(state, 1);
            let payload = ffi::lua_newuserdatatagged(state, PAYLOAD, 0).cast::<u8>();
            payload.cast::<u64>().write_unaligned(bits);
            payload.add(8).write(tag);
            std::ptr::write_bytes(payload.add(9), 0, PAYLOAD - 9);
            ffi::lua_rawgeti(state, ffi::LUA_REGISTRYINDEX, metatable);
            ffi::lua_setmetatable(state, -2); // [intern, key, value]
            ffi::lua_pushvalue(state, -2);
            ffi::lua_pushvalue(state, -2);
            ffi::lua_rawset(state, -5);
            ffi::lua_replace(state, -3);
            ffi::lua_pop(state, 1);
        }
    }

    pub(super) fn push<S: Scope>(scope: &S, bits: u64, tag: LightTag) -> Result<()> {
        let (intern, metatable) = references(scope.state(), tag)?;
        // SAFETY: push_in balances to one value and may raise out of memory, which
        // push_allocating catches at host level and Luau's pcall catches inside a call.
        unsafe {
            crate::stack::push_allocating(scope.state(), scope.is_host_level(), |state| {
                push_in(state, bits, tag, intern, metatable);
            })
        }
    }

    pub(super) unsafe fn name_tag(state: *mut ffi::lua_State, tag: LightTag, name: &std::ffi::CStr) -> Result<()> {
        // SAFETY: forwarded contract.
        let shared = unsafe { crate::runtime::shared_for(state) }
            .ok_or_else(|| Error::logic("a packed scalar needs an l3i runtime"))?;
        if shared.boxed_metatable_ref(tag) != ffi::LUA_NOREF {
            return Ok(());
        }
        let mut intern = shared.boxed_intern();
        let mut reference = ffi::LUA_NOREF;
        let mut pointer: *const c_void = std::ptr::null();
        // SAFETY: allocation runs protected; the references keep both tables alive for the VM.
        unsafe {
            crate::raw::protect::protected_call(state, 0, 0, |state| {
                if intern == ffi::LUA_NOREF {
                    ffi::lua_createtable(state, 0, 0);
                    ffi::lua_createtable(state, 0, 1);
                    ffi::lua_pushlstring(state, c"v".as_ptr(), 1);
                    ffi::lua_setfield(state, -2, c"__mode".as_ptr());
                    ffi::lua_setmetatable(state, -2);
                    intern = ffi::lua_ref(state, -1);
                    ffi::lua_pop(state, 1);
                }
                ffi::lua_createtable(state, 0, 1);
                ffi::lua_pushstring(state, name.as_ptr());
                ffi::lua_setfield(state, -2, c"__type".as_ptr());
                ffi::lua_setreadonly(state, -1, 1);
                pointer = ffi::lua_topointer(state, -1);
                reference = ffi::lua_ref(state, -1);
                ffi::lua_pop(state, 1);
                0
            })?;
        }
        shared.set_boxed_intern(intern);
        shared.set_boxed_metatable(tag, reference, pointer);
        Ok(())
    }

    pub(super) unsafe fn self_test(state: *mut ffi::lua_State) -> Result<()> {
        let tag = super::BITS64_TAG;
        let other = super::COLOR16_TAG;
        let (intern, metatable) = references(state, tag)?;
        let (_, other_metatable) = references(state, other)?;
        let mut failed = None;
        // SAFETY: forwarded contract; everything allocating runs inside the protected call, which
        // settles the stack back to where it began.
        unsafe {
            crate::raw::protect::protected_call(state, 0, 0, |state| {
                let top = ffi::lua_gettop(state);
                for bits in self_test_patterns() {
                    push_in(state, bits, tag, intern, metatable);
                    push_in(state, bits, tag, intern, metatable);
                    push_in(state, bits, other, intern, other_metatable);
                    let read = ffi::l3i_stack_slot(state, top + 1)
                        .as_ref()
                        .and_then(|v| read_raw(v, || state))
                        .is_some_and(|read| read == bits);
                    let equal = ffi::lua_rawequal(state, top + 1, top + 2) != 0;
                    let unequal = ffi::lua_rawequal(state, top + 1, top + 3) == 0;
                    ffi::lua_settop(state, top);
                    if !(read && equal && unequal) {
                        failed = Some((bits, read, equal, unequal));
                        break;
                    }
                }
                0
            })?;
        }
        match failed {
            None => Ok(()),
            Some((bits, read, equal, unequal)) => Err(Error::logic(format!(
                "the boxed userdata carrier does not keep packed scalar {bits:#018x} whole \
                 (read {read}, interned {equal}, unequal under another tag {unequal})"
            ))),
        }
    }
}

#[cfg(l3i_carrier = "integer")]
mod imp {
    use super::{LightTag, Result, Scope};
    use crate::convert::RawValue;
    use crate::raw::ffi;

    #[inline(always)]
    pub(super) fn read_raw(raw: &RawValue, _state: impl FnOnce() -> *mut ffi::lua_State) -> Option<u64> {
        (raw.tag() == ffi::LUA_TINTEGER).then(|| raw.pointer_bits())
    }

    #[inline(always)]
    pub(super) fn push<S: Scope>(scope: &S, bits: u64, _tag: LightTag) -> Result<()> {
        // SAFETY: pushes one integer after push_fixed makes room for it.
        unsafe { crate::stack::push_fixed(scope.state(), |state| ffi::lua_pushinteger64(state, bits as i64)) }
    }

    #[allow(clippy::unnecessary_wraps, reason = "the signature every carrier shares; an integer has no tag to name")]
    pub(super) unsafe fn name_tag(_state: *mut ffi::lua_State, _tag: LightTag, _name: &std::ffi::CStr) -> Result<()> {
        Ok(())
    }

    #[allow(clippy::unnecessary_wraps, reason = "the signature every carrier shares; Luau's integer is its own type")]
    pub(super) unsafe fn self_test(_state: *mut ffi::lua_State) -> Result<()> {
        let _ = super::self_test_patterns;
        Ok(())
    }
}
