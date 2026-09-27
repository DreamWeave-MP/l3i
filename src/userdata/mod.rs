//! Userdata: Rust values owned (or stably borrowed) by Luau.
//!
//! Two deliberately different paths, as in the C++ binder:
//! - [`tagged`]: a dedicated Luau runtime tag per type, one destructor and one metatable per
//!   tag, payload allocated inline, O(1) `lua_touserdatatagged` checks. Scarce; for hot types.
//! - [`untagged`]: exact-metatable identity and per-instance destructors for the long tail,
//!   consuming no tag. Storage may be owned or a stable borrow of an engine-owned object.
//!
//! Both are declared through one [`Userdata`] trait; `TAG` decides the path at compile time.

pub mod dispatch;
pub mod iterator;
pub mod metatable;
pub mod tagged;
pub mod untagged;

use std::any::TypeId;
use std::collections::HashMap;
use std::ffi::c_void;
use std::ptr::NonNull;
use std::sync::{PoisonError, RwLock};

use crate::error::Result;
use crate::stack::ValueView;

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

/// A Rust type exposed to Luau as userdata.
///
/// `TAG = Some(n)` selects the tagged hot path (payload inline, tag `n`); `None` selects the
/// untagged long-tail path (exact metatable identity, [`Storage`] wrapper).
///
/// # Safety
/// Implementors promise that `Drop` never calls into the Lua API, never panics, and does not
/// depend on the VM being in a consistent state: Luau runs it while sweeping GC memory. `TAG`
/// and `NAME` are declared by the host and must be unique within a VM; the binder verifies
/// conflicts at registration but cannot see two crates that agree on a number by accident.
pub unsafe trait Userdata: Sized + 'static {
    /// Script-visible `__type` and the root of the type's debug names, e.g. `openmw.util.Vector3`.
    const NAME: &'static str;
    /// The Luau runtime tag for the tagged path, or `None` for the untagged path.
    const TAG: Option<RuntimeTag>;
}

/// A pointer to an engine-owned object that Luau may observe but never owns.
///
/// The host guarantees the pointee, and its address, stay valid whenever Lua can reach the
/// userdata, and that the pointee is destroyed only after Lua execution has stopped. Dropping
/// the userdata never dereferences the pointer. Prefer `Arc<T>` payloads or handle/id payloads
/// wherever the engine object's lifetime is not already this strong.
#[derive(Clone, Copy, Debug)]
pub struct StableRef<T>(NonNull<T>);

impl<T> StableRef<T> {
    /// # Safety
    /// See the type documentation: the pointee must outlive every possible Lua observation of
    /// the userdata that will hold this reference.
    pub unsafe fn new(pointer: NonNull<T>) -> StableRef<T> {
        StableRef(pointer)
    }

    pub fn as_ptr(&self) -> *const T {
        self.0.as_ptr()
    }
}

/// Untagged userdata storage (`UserdataStorage<T>`): the payload is either owned by Luau or a
/// stable borrow of an engine object.
#[repr(C)]
pub enum Storage<T> {
    Owned(T),
    Borrowed(StableRef<T>),
}

impl<T> Storage<T> {
    /// The payload, owned or borrowed.
    pub fn get(&self) -> &T {
        match self {
            Storage::Owned(value) => value,
            // SAFETY: the StableRef contract keeps the pointee alive while this storage exists.
            Storage::Borrowed(borrowed) => unsafe { &*borrowed.as_ptr() },
        }
    }

    /// The payload only when Luau owns it.
    pub fn owned(&self) -> Option<&T> {
        match self {
            Storage::Owned(value) => Some(value),
            Storage::Borrowed(_) => None,
        }
    }
}

/// A per-type registry key: a light-userdata identity that is unique per Rust type.
///
/// Rust statics inside generic functions are shared across instantiations, so the key cannot
/// be a per-`T` static. Instead the first request for a type leaks one byte and records its
/// address against the type's `TypeId` in a process-wide map. `TypeId` equality is exact, so two
/// types can never share a key; a hash of the id (the previous scheme) could in principle
/// collide and let `test::<T>()` accept another type's storage.
pub(crate) fn type_key<T: 'static>() -> *mut c_void {
    static KEYS: RwLock<Option<HashMap<TypeId, usize>>> = RwLock::new(None);
    let id = TypeId::of::<T>();
    if let Some(key) = KEYS.read().unwrap_or_else(PoisonError::into_inner).as_ref().and_then(|keys| keys.get(&id)) {
        return *key as *mut c_void;
    }
    let mut keys = KEYS.write().unwrap_or_else(PoisonError::into_inner);
    let key = *keys.get_or_insert_with(HashMap::new).entry(id).or_insert_with(|| {
        // Leaked on purpose: one byte per Rust type ever used as a registry key, for the life
        // of the process, so the address can never be reused by another allocation.
        Box::leak(Box::new(0u8)) as *mut u8 as usize
    });
    key as *mut c_void
}

/// The payload when `value` is a `T` of either path; one tag compare for tagged types, one
/// metatable identity compare for untagged ones.
pub fn receiver<'v, T: Userdata>(value: ValueView<'v>) -> Option<&'v T> {
    if T::TAG.is_some() { tagged::test::<T>(value) } else { untagged::test::<T>(value) }
}

/// [`receiver`] or the Luau-style type error naming `T::NAME`.
pub fn check_receiver<'v, T: Userdata>(value: ValueView<'v>) -> Result<&'v T> {
    receiver::<T>(value).ok_or_else(|| crate::diagnostics::type_error(value, T::NAME))
}

#[cfg(test)]
mod type_key_tests {
    use super::type_key;

    struct A;
    struct B;

    #[test]
    fn keys_are_stable_per_type_and_distinct_between_types() {
        assert_eq!(type_key::<A>(), type_key::<A>());
        assert_ne!(type_key::<A>(), type_key::<B>());
        assert!(!type_key::<A>().is_null());
        let threads: Vec<_> = (0..8).map(|_| std::thread::spawn(|| type_key::<A>() as usize)).collect();
        for thread in threads {
            assert_eq!(thread.join().unwrap(), type_key::<A>() as usize);
        }
    }
}
