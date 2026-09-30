//! Untagged userdata: the long-tail path (`untaggedregistration.hpp`, `untaggeduserdataaccess.hpp`).
//!
//! No runtime tag is consumed. Each type has exactly one metatable, published in three places
//! (the registry under its canonical name, a private catalogue table under that name, and a
//! per-type slot keyed by the Rust type) so that duplicates fail in any of them and the typed
//! slot keeps working even if a host overwrites the named entries. Checks compare exact
//! metatable identity and require the metatable to be read-only.

use std::ffi::{CString, c_int, c_void};
use std::ptr;

use super::metatable::MetatableBuilder;
use super::{StableRef, Storage, Userdata, assert_userdata_layout, type_key};
use crate::error::{Error, Result};
use crate::raw::ffi;
use crate::runtime::Runtime;
use crate::runtime::shared::{UntaggedIdentity, shared_of};
use crate::stack::{Frame, Scope, ValueView};

static PRIVATE_REGISTRY_KEY: u8 = 0;

fn private_registry_key() -> *mut c_void {
    (&PRIVATE_REGISTRY_KEY as *const u8).cast_mut().cast()
}

/// Runs the storage's `Drop` when Luau frees the userdata (owned payloads drop; borrowed
/// storage drops only the pointer).
unsafe extern "C" fn destroy_storage<T: Userdata>(_: *mut ffi::lua_State, userdata: *mut c_void) {
    // SAFETY: only this module creates userdata with this destructor, always fully written.
    let outcome = std::panic::catch_unwind(|| unsafe { ptr::drop_in_place(userdata.cast::<Storage<T>>()) });
    if outcome.is_err() {
        std::process::abort();
    }
}

/// Pushes the private catalogue table, creating it on demand. Returns its absolute index.
///
/// # Safety
/// `state` is live with stack room for two values.
unsafe fn push_private_registry(state: *mut ffi::lua_State, create: bool) -> Result<c_int> {
    unsafe {
        let kind = ffi::lua_rawgetp(state, ffi::LUA_REGISTRYINDEX, private_registry_key());
        if kind == ffi::LUA_TTABLE {
            return Ok(ffi::lua_gettop(state));
        }
        if kind != ffi::LUA_TNIL {
            ffi::lua_pop(state, 1);
            return Err(Error::logic("Untagged userdata registry is corrupted"));
        }
        if !create {
            return Ok(ffi::lua_gettop(state));
        }
        ffi::lua_pop(state, 1);
        ffi::lua_newtable(state);
        ffi::lua_pushvalue(state, -1);
        ffi::lua_rawsetp(state, ffi::LUA_REGISTRYINDEX, private_registry_key());
        Ok(ffi::lua_gettop(state))
    }
}

/// Registers `T`'s metatable under its canonical name, configured by `configure`.
///
/// Transactional: any duplicate (public name, private catalogue, typed slot) or configure
/// failure leaves every registry entry as it was, including foreign entries installed
/// meanwhile.
pub fn register<T: Userdata>(
    runtime: &Runtime,
    configure: impl FnOnce(&mut MetatableBuilder<'_>) -> Result<()>,
) -> Result<()> {
    const { assert_userdata_layout::<Storage<T>>() };
    if let Some(tag) = runtime.shared().tag_of_type(std::any::TypeId::of::<T>()) {
        return Err(Error::logic(format!("'{}' is already tag {tag} in this runtime; use userdata::tagged", T::NAME)));
    }
    if T::NAME.is_empty() {
        return Err(Error::logic("Untagged userdata name cannot be empty"));
    }
    crate::debug_name::require_valid_debug_name(T::NAME, runtime.debug_roots())?;
    let name = CString::new(T::NAME).map_err(|_| Error::logic("Userdata type name cannot contain NUL"))?;
    let key = type_key::<T>();

    let stack = runtime.stack();
    stack.with_frame(|frame| {
        let state = frame.state();
        // SAFETY: live main thread; indexes are relative to values pushed here and the frame
        // restores the height on every path.
        unsafe {
            ffi::lua_getfield(state, ffi::LUA_REGISTRYINDEX, name.as_ptr());
            let public_exists = !ffi::lua_isnil(state, -1);
            ffi::lua_pop(state, 1);
            if public_exists {
                return Err(Error::logic(format!("Duplicate untagged userdata metatable: {}", T::NAME)));
            }

            let registry = push_private_registry(state, true)?;
            ffi::lua_getfield(state, registry, name.as_ptr());
            let private_exists = !ffi::lua_isnil(state, -1);
            ffi::lua_pop(state, 1);
            if private_exists {
                return Err(Error::logic(format!("Duplicate private untagged userdata registration: {}", T::NAME)));
            }

            ffi::lua_rawgetp(state, ffi::LUA_REGISTRYINDEX, key);
            let typed_exists = !ffi::lua_isnil(state, -1);
            ffi::lua_pop(state, 1);
            if typed_exists {
                return Err(Error::logic(format!("Duplicate typed untagged userdata registration: {}", T::NAME)));
            }

            ffi::lua_newtable(state);
            let metatable = ffi::lua_gettop(state);

            let rollback = |state: *mut ffi::lua_State| {
                // Remove only entries that still point at our table.
                ffi::lua_getfield(state, registry, name.as_ptr());
                let private_matches = ffi::lua_rawequal(state, -1, metatable) != 0;
                ffi::lua_pop(state, 1);
                if private_matches {
                    ffi::lua_pushnil(state);
                    ffi::lua_setfield(state, registry, name.as_ptr());
                }
                ffi::lua_getfield(state, ffi::LUA_REGISTRYINDEX, name.as_ptr());
                let public_matches = ffi::lua_rawequal(state, -1, metatable) != 0;
                ffi::lua_pop(state, 1);
                if public_matches {
                    ffi::lua_pushnil(state);
                    ffi::lua_setfield(state, ffi::LUA_REGISTRYINDEX, name.as_ptr());
                }
                ffi::lua_rawgetp(state, ffi::LUA_REGISTRYINDEX, key);
                let typed_matches = ffi::lua_rawequal(state, -1, metatable) != 0;
                ffi::lua_pop(state, 1);
                if typed_matches {
                    ffi::lua_pushnil(state);
                    ffi::lua_rawsetp(state, ffi::LUA_REGISTRYINDEX, key);
                }
            };

            let configured = (|| {
                let mut builder = MetatableBuilder::new(frame, metatable, runtime.debug_roots())?;
                builder.set_receiver_type::<T>(false);
                builder.set_type(T::NAME)?;
                configure(&mut builder)?;
                builder.finish()?;
                // Protection is restored even if configure removed it.
                ffi::lua_rawgetfield(state, metatable, c"__metatable".as_ptr());
                let is_protected = !ffi::lua_isnil(state, -1);
                ffi::lua_pop(state, 1);
                if !is_protected {
                    ffi::lua_pushboolean(state, 0);
                    ffi::lua_rawsetfield(state, metatable, c"__metatable".as_ptr());
                }
                ffi::lua_setreadonly(state, metatable, 1);

                ffi::lua_pushvalue(state, metatable);
                ffi::lua_rawsetp(state, ffi::LUA_REGISTRYINDEX, key);
                ffi::lua_pushvalue(state, metatable);
                ffi::lua_setfield(state, registry, name.as_ptr());
                ffi::lua_pushvalue(state, metatable);
                ffi::lua_setfield(state, ffi::LUA_REGISTRYINDEX, name.as_ptr());
                // The hot-path identity and the push reference, read through `identity_of`.
                ffi::lua_pushvalue(state, metatable);
                let reference = ffi::lua_ref(state, -1);
                ffi::lua_pop(state, 1);
                let pointer = ffi::lua_topointer(state, metatable);
                runtime
                    .shared()
                    .set_untagged_identity(std::any::TypeId::of::<T>(), UntaggedIdentity { pointer, reference });
                Ok(())
            })();
            if let Err(error) = configured {
                rollback(state);
                return Err(error);
            }
            Ok(())
        }
    })
}

/// `T`'s registered metatable in the VM that owns `state`, from the runtime's per-VM cache.
/// One callbacks read and one hash lookup; registration is the only writer.
#[inline]
fn identity_of<T: Userdata>(state: *mut ffi::lua_State) -> Option<UntaggedIdentity> {
    // SAFETY: the state is live; a VM without a runtime block has no untagged registrations.
    let shared = unsafe { shared_of(state) };
    if shared.is_null() {
        return None;
    }
    unsafe { (*shared).untagged_identity(std::any::TypeId::of::<T>()) }
}

/// Allocates storage, writes it, attaches `T`'s metatable, returns the view.
fn push_storage<'s, T: Userdata>(scope: &'s impl Scope, storage: Storage<T>) -> Result<ValueView<'s>> {
    const { assert_userdata_layout::<Storage<T>>() };
    let state = scope.state();
    let identity = identity_of::<T>(state)
        .ok_or_else(|| Error::logic(format!("Unknown or writable untagged userdata metatable: {}", T::NAME)))?;
    // SAFETY: allocate, write immediately (Luau owns the destructor from allocation on), then
    // attach the registered metatable. Out of memory is the only raise.
    unsafe {
        let raw = ffi::lua_newuserdatadtor(state, std::mem::size_of::<Storage<T>>(), destroy_storage::<T>);
        if raw.is_null() {
            return Err(Error::runtime("Unable to allocate untagged userdata"));
        }
        ptr::write(raw.cast::<Storage<T>>(), storage);
        ffi::lua_getref(state, identity.reference);
        if ffi::lua_setmetatable(state, -2) == 0 {
            return Err(Error::logic("Unable to attach untagged userdata metatable"));
        }
    }
    Ok(scope.top_value())
}

/// Pushes `value` as a new Luau-owned userdata of type `T`.
pub fn push<'s, T: Userdata>(scope: &'s impl Scope, value: T) -> Result<ValueView<'s>> {
    push_storage(scope, Storage::Owned(value))
}

/// Pushes a userdata that borrows an engine-owned `T`; see [`StableRef`] for the contract.
/// Only the read-only receiver path can see it.
pub fn push_borrowed<'s, T: Userdata>(scope: &'s impl Scope, borrowed: StableRef<T>) -> Result<ValueView<'s>> {
    push_storage(scope, Storage::Borrowed(borrowed))
}

/// The storage when `value` is a `T` whose metatable is `T`'s registered read-only one.
fn storage<'v, T: Userdata>(value: ValueView<'v>) -> Option<&'v Storage<T>> {
    if !value.is_userdata() {
        return None;
    }
    let state = value.state();
    let expected = identity_of::<T>(state)?.pointer;
    // SAFETY: the view proved the slot; lua_getmetatablepointer is a read with no pushes, and a
    // matching identity means only `push_storage::<T>` could have created this userdata.
    unsafe {
        if ffi::lua_getmetatablepointer(state, value.index()) != expected {
            return None;
        }
        ffi::lua_touserdata(state, value.index()).cast::<Storage<T>>().as_ref()
    }
}

/// The payload (owned or borrowed) when `value` is a `T`.
pub fn test<'v, T: Userdata>(value: ValueView<'v>) -> Option<&'v T> {
    storage::<T>(value).map(Storage::get)
}

/// The payload only when Luau owns it: borrowed storage is not a mutable receiver.
pub fn test_owned<'v, T: Userdata>(value: ValueView<'v>) -> Option<&'v T> {
    storage::<T>(value).and_then(Storage::owned)
}

/// [`test()`] or a Luau-style type error.
pub fn check<'v, T: Userdata>(value: ValueView<'v>) -> Result<&'v T> {
    test::<T>(value).ok_or_else(|| crate::diagnostics::type_error(value, T::NAME))
}

/// True when this VM has registered `T`'s metatable.
pub fn is_registered<T: Userdata>(frame: &Frame<'_>) -> bool {
    identity_of::<T>(frame.state()).is_some()
}
