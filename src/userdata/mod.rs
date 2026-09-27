//! Userdata: Rust values owned (or stably borrowed) by Luau.
//!
//! Two deliberately different paths, as in the C++ binder:
//! - [`tagged`]: a dedicated Luau runtime tag per type, one destructor and one metatable per
//!   tag, payload allocated inline, O(1) `lua_touserdatatagged` checks. Scarce; for hot types.
//! - [`untagged`]: exact-metatable identity and per-instance destructors for the long tail,
//!   consuming no tag. Storage may be owned or a stable borrow of an engine-owned object.
//!
//! Both are declared through one [`Userdata`] trait, which gives a type its identity and script
//! name only. Whether a type is tagged, and which tag it gets, is decided by the host per VM at
//! registration (`tagged::register` takes the tag); the same Rust type may be tagged 8 in one
//! runtime, 17 in another, and untagged in a third.

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

/// A Rust type exposed to Luau as userdata: a stable type identity plus a script name.
///
/// The path is chosen at registration, not here: [`tagged::register`] takes the tag the host
/// assigns in that VM (payload inline, O(1) checks); [`untagged::register`] uses exact metatable
/// identity and the [`Storage`] wrapper.
///
/// # Safety
/// Implementors promise that `Drop` never calls into the Lua API, never panics, and does not
/// depend on the VM being in a consistent state: Luau runs it while sweeping GC memory. `NAME`
/// must be unique within a VM; the binder verifies conflicts at registration.
pub unsafe trait Userdata: Sized + 'static {
    /// Script-visible `__type` and the root of the type's debug names, e.g. `openmw.util.Vector3`.
    const NAME: &'static str;
}

/// A pointer to an engine-owned object that Luau may observe but never owns.
///
/// The host guarantees the pointee, and its address, stay valid whenever Lua can reach the
/// userdata, and that the pointee is destroyed only after Lua execution has stopped. Dropping
/// the userdata never dereferences the pointer. Prefer `Arc<T>` payloads or handle/id payloads
/// wherever the engine object's lifetime is not already this strong.
#[derive(Debug)]
pub struct StableRef<T>(NonNull<T>);

impl<T> Clone for StableRef<T> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<T> Copy for StableRef<T> {}

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

/// The payload when `value` is a `T` of either path: one tag-to-type compare for tagged types
/// (which also rejects untagged userdata in one read), then one metatable identity compare.
pub fn receiver<'v, T: Userdata>(value: ValueView<'v>) -> Option<&'v T> {
    tagged::test::<T>(value).or_else(|| untagged::test::<T>(value))
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

/// A userdata result: moves `value` into a new Lua-owned instance of its registered type
/// (tagged or untagged, as this VM registered it). Returned from bound functions as
/// `Owned(value)`.
pub struct Owned<T: Userdata>(pub T);

impl<T: Userdata> crate::bind::Return for Owned<T> {
    fn push_results(self, call: &crate::bind::Call<'_>) -> Result<std::ffi::c_int> {
        push_owned(call, self.0)?;
        Ok(1)
    }
}

/// Pushes `value` as a new Lua-owned userdata of whichever path this VM registered `T` on.
pub fn push_owned<'s, T: Userdata>(scope: &'s impl crate::stack::Scope, value: T) -> Result<ValueView<'s>> {
    match tagged::tag_of::<T>(scope) {
        Some(_) => tagged::push(scope, value),
        None => untagged::push(scope, value),
    }
}

/// A stable borrow of an engine object as a userdata value (untagged types only: tagged
/// payloads are always owned). Pushable as an argument and returnable from bound functions.
#[derive(Debug)]
pub struct Borrowed<T: Userdata>(pub StableRef<T>);

impl<T: Userdata> Clone for Borrowed<T> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<T: Userdata> Copy for Borrowed<T> {}

impl<T: Userdata> crate::convert::Push for Borrowed<T> {
    fn push_into<'s, S: crate::stack::Scope>(&self, scope: &'s S) -> Result<ValueView<'s>> {
        if tagged::tag_of::<T>(scope).is_some() {
            return Err(crate::error::Error::logic(format!(
                "'{}' is tagged in this runtime; tagged userdata always owns its payload",
                T::NAME
            )));
        }
        untagged::push_borrowed(scope, self.0)
    }
}

impl<T: Userdata> crate::bind::Return for Borrowed<T> {
    fn push_results(self, call: &crate::bind::Call<'_>) -> Result<std::ffi::c_int> {
        crate::convert::Push::push_into(&self, call)?;
        Ok(1)
    }
}
