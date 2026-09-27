//! Tagged userdata: the scarce hot path (`components/luau/taggeduserdata.hpp`).
//!
//! Registration order is the contract: build and freeze the metatable first, then publish the
//! destructor and metatable for the tag last, because the Luau setters cannot report failure.
//! Instances are allocated with `lua_newuserdatataggedwithmetatable` and initialised
//! immediately, so Luau never owns a destructor over uninitialised Rust memory.

use std::ffi::{CStr, CString, c_int, c_void};
use std::ptr;

use super::metatable::MetatableBuilder;
use super::{RuntimeTag, Userdata, assert_userdata_layout};
use crate::TAG_LIMIT;
use crate::error::{Error, Result};
use crate::raw::ffi;
use crate::runtime::Runtime;
use crate::stack::{Scope, ValueView};

/// The tag `T` declares; a compile error for an untagged type.
const fn tag_of<T: Userdata>() -> RuntimeTag {
    match T::TAG {
        Some(tag) => tag,
        None => panic!("this Userdata type is untagged; use userdata::untagged"),
    }
}

/// Runs the payload's `Drop` when Luau frees the userdata.
///
/// Luau calls this while sweeping; the payload contract forbids Lua access and panics, so a
/// panic here is a contract violation and aborts rather than unwinding into the collector.
unsafe extern "C" fn destroy<T: Userdata>(_: *mut ffi::lua_State, userdata: *mut c_void) {
    // SAFETY: Luau only calls the destructor registered for T's tag on userdata created by
    // `push::<T>`, which wrote a valid T at this address; nothing reads it afterwards.
    let outcome = std::panic::catch_unwind(|| unsafe { ptr::drop_in_place(userdata.cast::<T>()) });
    if outcome.is_err() {
        std::process::abort();
    }
}

fn require_tag_in_range(tag: RuntimeTag) -> Result<()> {
    if tag == 0 || tag >= TAG_LIMIT {
        return Err(Error::logic(format!(
            "Luau userdata tag {tag} is outside the usable range 1..{TAG_LIMIT}"
        )));
    }
    Ok(())
}

/// Registers `T` under its tag with a metatable configured by `configure`.
///
/// Registering the same `T` twice under the same name is a no-op. Any other collision (a
/// different type or name on the tag, or the name already naming a registry metatable) is a
/// logic error, and a failure inside `configure` unpublishes the half-built metatable.
pub fn register<T: Userdata>(
    runtime: &Runtime,
    configure: impl FnOnce(&mut MetatableBuilder<'_>) -> Result<()>,
) -> Result<()> {
    const { assert_userdata_layout::<T>() };
    let tag_value = const { tag_of::<T>() };
    require_tag_in_range(tag_value)?;
    crate::debug_name::require_valid_debug_name(T::NAME, runtime.debug_roots())?;
    let name = CString::new(T::NAME).map_err(|_| Error::logic("Userdata type name cannot contain NUL"))?;
    let tag = c_int::from(tag_value);
    let destructor: ffi::lua_Destructor = destroy::<T>;

    let stack = runtime.stack();
    stack.with_frame(|frame| {
        let state = frame.state();
        // SAFETY: live main thread; every index below is relative to values pushed here and
        // the frame restores the height on every path.
        unsafe {
            ffi::lua_getuserdatametatable(state, tag);
            let already_registered = !ffi::lua_isnil(state, -1);
            ffi::lua_pop(state, 1);
            if already_registered {
                let registered_name = CStr::from_ptr(ffi::lua_getuserdataname(state, tag));
                let same_destructor =
                    ffi::lua_getuserdatadtor(state, tag).is_some_and(|f| ptr::fn_addr_eq(f, destructor));
                if registered_name.to_bytes() != T::NAME.as_bytes() || !same_destructor {
                    return Err(Error::logic(format!(
                        "Conflicting Luau userdata tag registration: tag {} is already '{}'",
                        tag_value,
                        registered_name.to_string_lossy()
                    )));
                }
                return Ok(());
            }

            if ffi::luaL_newmetatable(state, name.as_ptr()) == 0 {
                return Err(Error::logic(format!("Conflicting Luau userdata name registration: '{}'", T::NAME)));
            }
            let metatable = ffi::lua_gettop(state);
            let published = ffi::lua_topointer(state, metatable);

            let configured = (|| {
                let mut builder = MetatableBuilder::new(frame, metatable, runtime.debug_roots())?;
                builder.set_type(T::NAME)?;
                configure(&mut builder)?;
                builder.finish()
            })();
            if let Err(error) = configured {
                // Unpublish the registry name only if it still names our table.
                ffi::lua_getfield(state, ffi::LUA_REGISTRYINDEX, name.as_ptr());
                let still_ours = ffi::lua_topointer(state, -1) == published;
                ffi::lua_pop(state, 1);
                if still_ours {
                    ffi::lua_pushnil(state);
                    ffi::lua_setfield(state, ffi::LUA_REGISTRYINDEX, name.as_ptr());
                }
                return Err(error);
            }
            ffi::lua_setreadonly(state, metatable, 1);

            // Publication is last: neither setter reports failure.
            ffi::lua_setuserdatadtor(state, tag, Some(destructor));
            ffi::lua_pushvalue(state, metatable);
            ffi::lua_setuserdatametatable(state, tag);
            Ok(())
        }
    })
}

/// True when `T`'s tag currently carries `T`'s destructor, i.e. `register::<T>` ran on this VM.
/// An untagged type or a tag outside `1..TAG_LIMIT` is never registered.
pub fn is_registered<T: Userdata>(scope: &impl Scope) -> bool {
    let Some(tag) = T::TAG else { return false };
    if require_tag_in_range(tag).is_err() {
        return false;
    }
    // SAFETY: live state; the tag is within Luau's destructor table.
    let registered = unsafe { ffi::lua_getuserdatadtor(scope.state(), c_int::from(tag)) };
    registered.is_some_and(|f| ptr::fn_addr_eq(f, destroy::<T> as ffi::lua_Destructor))
}

fn require_registered<T: Userdata>(scope: &impl Scope) -> Result<()> {
    if is_registered::<T>(scope) {
        return Ok(());
    }
    Err(Error::logic(format!("Luau tagged userdata type '{}' is not registered", T::NAME)))
}

/// Pushes `value` as a new tagged userdata onto `scope` and returns a view of it.
///
/// One allocation, one write, no intermediate state: the metatable attached by Luau is the
/// one `register::<T>` published.
pub fn push<'s, T: Userdata>(scope: &'s impl Scope, value: T) -> Result<ValueView<'s>> {
    const { assert_userdata_layout::<T>() };
    let tag = const { tag_of::<T>() };
    require_registered::<T>(scope)?;
    // SAFETY: registration verified the tag has T's metatable and destructor. The allocation
    // is fully initialised by `ptr::write` before anything else can observe it. Luau raises
    // only for out of memory, before the destructor could see the slot.
    unsafe {
        let storage = ffi::lua_newuserdatataggedwithmetatable(scope.state(), std::mem::size_of::<T>(), c_int::from(tag));
        if storage.is_null() {
            return Err(Error::runtime("Unable to allocate tagged userdata"));
        }
        ptr::write(storage.cast::<T>(), value);
    }
    Ok(scope.top_value())
}

/// The payload when `value` is a `T`, else `None`. One tag comparison, no metatable walk.
pub fn test<'v, T: Userdata>(value: ValueView<'v>) -> Option<&'v T> {
    let tag = T::TAG?;
    if !value.exists() || require_tag_in_range(tag).is_err() {
        return None;
    }
    // SAFETY: lua_touserdatatagged returns the payload only when the userdata carries T's
    // tag, and only `push::<T>` creates userdata with that tag on a VM where T is registered.
    // The view's lifetime keeps the slot, and so the userdata, reachable.
    unsafe {
        let data = ffi::lua_touserdatatagged(value.state(), value.index(), c_int::from(tag));
        data.cast::<T>().as_ref()
    }
}

/// Mutable access to the payload.
///
/// # Safety
/// No other reference to the same userdata's payload may be live: Luau lets the same value
/// appear at several stack slots, and the binder cannot see aliasing through them.
pub unsafe fn test_mut<'v, T: Userdata>(value: ValueView<'v>) -> Option<&'v mut T> {
    let tag = T::TAG?;
    if !value.exists() || require_tag_in_range(tag).is_err() {
        return None;
    }
    unsafe {
        let data = ffi::lua_touserdatatagged(value.state(), value.index(), c_int::from(tag));
        data.cast::<T>().as_mut()
    }
}

/// [`test`] or a Luau-style type error for argument `value`.
pub fn check<'v, T: Userdata>(value: ValueView<'v>) -> Result<&'v T> {
    test::<T>(value).ok_or_else(|| crate::diagnostics::type_error(value, T::NAME))
}
