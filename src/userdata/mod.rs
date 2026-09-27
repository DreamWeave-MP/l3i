//! Userdata: Rust values owned by Luau.
//!
//! Two deliberately different paths, as in the C++ binder:
//! - [`tagged`]: a dedicated Luau runtime tag per type, one destructor and one metatable per
//!   tag, payload allocated inline, O(1) `lua_touserdatatagged` checks. Scarce; for hot types.
//! - `untagged` (later phase): exact-metatable identity and per-instance destructors for the
//!   long tail, consuming no tag.

pub mod metatable;
pub mod tagged;

/// A Luau runtime userdata tag. Valid registrations use `1..TAG_LIMIT`; tag 0 is Luau's
/// untagged default. Tag numbers are protocol identifiers chosen by the host, never by the
/// binder.
pub type RuntimeTag = u8;

/// Luau places userdata payloads at 8-byte alignment, or 16-byte alignment once the payload is
/// at least 16 bytes (`lobject.h`, `Udata::data`).
pub const fn userdata_alignment(size: usize) -> usize {
    if size >= 16 { 16 } else { 8 }
}

/// Compile-time check that `T` fits Luau's alignment guarantee.
pub(crate) const fn assert_userdata_layout<T>() {
    assert!(
        std::mem::align_of::<T>() <= userdata_alignment(std::mem::size_of::<T>()),
        "userdata payload alignment exceeds what Luau guarantees (8, or 16 from 16 bytes up)"
    );
}

/// A Rust type stored inline in tagged Luau userdata.
///
/// # Safety
/// Implementors promise that `Drop` never calls into the Lua API, never panics, and does not
/// depend on the VM being in a consistent state: Luau runs it while sweeping GC memory. `TAG`
/// and `NAME` are declared by the host and must be unique within a VM; the binder verifies
/// conflicts at registration but cannot see two crates that agree on a number by accident.
pub unsafe trait TaggedUserdata: Sized + 'static {
    /// The Luau runtime tag, `1..TAG_LIMIT`.
    const TAG: RuntimeTag;
    /// Script-visible `__type` and the root of the type's debug names, e.g. `openmw.util.Vector3`.
    const NAME: &'static str;
}
