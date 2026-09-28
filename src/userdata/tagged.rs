//! Tagged userdata: the scarce hot path (`components/luau/taggeduserdata.hpp`).
//!
//! Tags are assigned by the host per VM: [`register`] takes the tag, records `tag -> type` in
//! the runtime's tag plan, and every check afterwards is `lua_userdatatag` + one `TypeId`
//! compare against that plan. A Rust type carries no tag of its own.
//!
//! Registration order is the contract: build and freeze the metatable first, then publish the
//! destructor and metatable for the tag last, because the Luau setters cannot report failure.
//! Instances are allocated with `lua_newuserdatataggedwithmetatable` and initialised
//! immediately, so Luau never owns a destructor over uninitialised Rust memory.

use std::any::TypeId;
use std::ffi::{CStr, CString, c_int, c_void};
use std::ptr;

use super::metatable::MetatableBuilder;
use super::{RuntimeTag, Userdata, assert_userdata_layout};
use crate::TAG_LIMIT;
use crate::error::{Error, Result};
use crate::raw::ffi;
use crate::runtime::Runtime;
use crate::stack::{Scope, ValueView};

/// The tag this VM assigned to `T`, if `T` is registered tagged here.
pub fn tag_of<T: Userdata>(scope: &impl Scope) -> Option<RuntimeTag> {
    // SAFETY: a scope proves its thread is live.
    unsafe { planned_tag::<T>(scope.state()) }
}

/// [`tag_of`] on a raw thread.
///
/// # Safety
/// `state` is a live thread.
pub(crate) unsafe fn planned_tag<T: Userdata>(state: *mut ffi::lua_State) -> Option<RuntimeTag> {
    unsafe { crate::runtime::shared_for(state) }.and_then(|shared| shared.tag_of_type(TypeId::of::<T>()))
}

/// True when `utag` is the tag this VM assigned to `T`.
///
/// # Safety
/// `state` is a live thread.
#[inline]
pub(crate) unsafe fn type_matches_tag<T: Userdata>(state: *mut ffi::lua_State, utag: c_int) -> bool {
    unsafe { crate::runtime::shared_for(state) }.and_then(|shared| shared.type_of_tag(utag)) == Some(TypeId::of::<T>())
}

/// The payload pointer when the userdata at `index` is a `T` of this VM, else null. No pushes.
///
/// # Safety
/// `state` is a live thread and `index` names an existing slot (or an acceptable index for
/// `lua_userdatatag`, which reports -1 for non-userdata).
#[inline]
pub(crate) unsafe fn payload_ptr<T: Userdata>(state: *mut ffi::lua_State, index: c_int) -> *mut T {
    unsafe {
        let mut tag: c_int = 0;
        let data = ffi::l3i_touserdata_tag(state, index, &mut tag);
        if data.is_null() || tag <= 0 || !type_matches_tag::<T>(state, tag) {
            return ptr::null_mut();
        }
        data.cast::<T>()
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
        return Err(Error::logic(format!("Luau userdata tag {tag} is outside the usable range 1..{TAG_LIMIT}")));
    }
    Ok(())
}

/// Registers `T` under `tag` in this VM with a metatable configured by `configure`.
///
/// Registering the same `T` twice under the same tag is a no-op. Any other collision (another
/// type on the tag, `T` already on another tag, the name already naming a registry metatable)
/// is a logic error, and a failure inside `configure` unpublishes the half-built metatable.
pub fn register<T: Userdata>(
    runtime: &Runtime,
    tag_value: RuntimeTag,
    configure: impl FnOnce(&mut MetatableBuilder<'_>) -> Result<()>,
) -> Result<()> {
    const { assert_userdata_layout::<T>() };
    require_tag_in_range(tag_value)?;
    let shared = runtime.shared();
    let identity = TypeId::of::<T>();
    if let Some(existing) = shared.tag_of_type(identity)
        && existing != tag_value
    {
        return Err(Error::logic(format!(
            "Conflicting Luau userdata tag registration: '{}' is already tag {existing} in this runtime",
            T::NAME
        )));
    }
    if let Some(other) = shared.type_of_tag(c_int::from(tag_value))
        && other != identity
    {
        return Err(Error::logic(format!(
            "Conflicting Luau userdata tag registration: tag {tag_value} is already '{}'",
            registered_name(runtime, tag_value)
        )));
    }
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
                builder.set_receiver_type::<T>(true);
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
            shared.assign_tag(identity, tag_value);
            Ok(())
        }
    })
}

/// The `__type` name Luau reports for `tag`, for diagnostics.
fn registered_name(runtime: &Runtime, tag: RuntimeTag) -> String {
    let stack = runtime.stack();
    // SAFETY: lua_getuserdataname reads the tag's metatable; the tag is in range.
    unsafe {
        CStr::from_ptr(ffi::lua_getuserdataname(stack.state_ptr(), c_int::from(tag))).to_string_lossy().into_owned()
    }
}

/// True when this VM assigned `T` a tag that carries `T`'s destructor, i.e. `register::<T>`
/// ran here.
pub fn is_registered<T: Userdata>(scope: &impl Scope) -> bool {
    let Some(tag) = tag_of::<T>(scope) else { return false };
    // SAFETY: live state; the tag is within Luau's destructor table.
    let registered = unsafe { ffi::lua_getuserdatadtor(scope.state(), c_int::from(tag)) };
    registered.is_some_and(|f| ptr::fn_addr_eq(f, destroy::<T> as ffi::lua_Destructor))
}

/// Pushes `value` as a new tagged userdata onto `scope` and returns a view of it.
///
/// One allocation, one write, no intermediate state: the metatable attached by Luau is the
/// one `register::<T>` published for the tag this VM gave `T`.
pub fn push<'s, T: Userdata>(scope: &'s impl Scope, value: T) -> Result<ValueView<'s>> {
    const { assert_userdata_layout::<T>() };
    let Some(tag) = tag_of::<T>(scope) else {
        return Err(Error::logic(format!("Luau tagged userdata type '{}' is not registered", T::NAME)));
    };
    // SAFETY: registration verified the tag has T's metatable and destructor. The allocation
    // is fully initialised by `ptr::write` before anything else can observe it. Luau raises
    // only for out of memory, before the destructor could see the slot.
    unsafe {
        let storage =
            ffi::lua_newuserdatataggedwithmetatable(scope.state(), std::mem::size_of::<T>(), c_int::from(tag));
        if storage.is_null() {
            return Err(Error::runtime("Unable to allocate tagged userdata"));
        }
        ptr::write(storage.cast::<T>(), value);
    }
    Ok(scope.top_value())
}

/// The payload when `value` is a `T`, else `None`. One tag read and one `TypeId` compare
/// against this VM's plan; no metatable walk.
pub fn test<'v, T: Userdata>(value: ValueView<'v>) -> Option<&'v T> {
    if !value.exists() {
        return None;
    }
    // SAFETY: the plan maps a tag to T only after `register::<T>` published T's metatable and
    // destructor for it, and only `push::<T>` creates userdata with that tag. The view's
    // lifetime keeps the slot, and so the userdata, reachable.
    unsafe { payload_ptr::<T>(value.state(), value.index()).as_ref() }
}

/// Mutable access to the payload.
///
/// # Safety
/// No other reference to the same userdata's payload may be live: Luau lets the same value
/// appear at several stack slots, and the binder cannot see aliasing through them.
pub unsafe fn test_mut<'v, T: Userdata>(value: ValueView<'v>) -> Option<&'v mut T> {
    if !value.exists() {
        return None;
    }
    unsafe { payload_ptr::<T>(value.state(), value.index()).as_mut() }
}

/// [`test`] or a Luau-style type error for argument `value`.
pub fn check<'v, T: Userdata>(value: ValueView<'v>) -> Result<&'v T> {
    test::<T>(value).ok_or_else(|| crate::diagnostics::type_error(value, T::NAME))
}
