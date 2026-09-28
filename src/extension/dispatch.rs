//! Generic direct-access callbacks for planned userdata types.
//!
//! The typed callbacks in [`crate::direct`] need a `DirectAccess` impl on the Rust type, which
//! a downstream crate cannot provide for the planner. These callbacks are the same for every
//! planned type: Luau hands them the receiver's tag and the member atom, the runtime's
//! [`DirectPlan`](crate::direct::plan::DirectPlan) turns that into a slot (validating Luau's
//! per-instruction cache first), and the slot names the bound member entry the planner stored
//! at registration. A hit runs the member directly on the callback's own stack; a miss falls
//! back to the metatable's own metamethod, which is the semantic oracle.

use std::ffi::{c_int, c_void};

use super::plan::ResolvedUserdata;
use crate::direct::plan::DirectPlan;
use crate::direct::registry::UNKNOWN_SLOT;
use crate::direct::{AccessKind, Atom};
use crate::error::{Error, Result};
use crate::raw::{ffi, trampoline};
use crate::runtime::Runtime;
use crate::runtime::shared::Shared;

/// The slot for `(tag, atom, kind)`, consulting and refreshing Luau's cache word.
///
/// # Safety
/// `slot` is Luau's cache word for the instruction or null.
#[inline(always)]
unsafe fn resolve(plan: &DirectPlan, slot: *mut u16, tag: c_int, atom: Atom, kind: AccessKind) -> u16 {
    unsafe {
        let cached = if slot.is_null() { UNKNOWN_SLOT } else { *slot };
        if cached != UNKNOWN_SLOT && plan.cached_slot_matches_tag(cached, tag, atom, kind) {
            return cached;
        }
        let resolved = plan.resolve_slot(tag, atom, kind);
        if !slot.is_null() {
            *slot = resolved;
        }
        resolved
    }
}

/// The VM's shared block and its direct plan, or `None` when the state has no runtime plan.
///
/// # Safety
/// `state` is a live thread.
#[inline(always)]
unsafe fn planned(state: *mut ffi::lua_State) -> Option<(&'static Shared, &'static DirectPlan)> {
    let shared = unsafe { crate::runtime::shared_for(state) }?;
    let plan = shared.direct_plan().get()?;
    Some((shared, plan))
}

/// Pushes the metatable metamethod `name` of userdata tag `tag`, or nil.
///
/// # Safety
/// `state` is live with room for two values.
unsafe fn push_metamethod(state: *mut ffi::lua_State, tag: c_int, name: &std::ffi::CStr) {
    unsafe {
        ffi::lua_getuserdatametatable(state, tag);
        if ffi::lua_istable(state, -1) {
            ffi::lua_rawgetfield(state, -1, name.as_ptr());
            ffi::lua_remove(state, -2);
        }
    }
}

/// Calls the metatable's own `__namecall` with the frame's arguments; every result forwarded.
unsafe fn namecall_fallback(state: *mut ffi::lua_State, tag: c_int, top: c_int) -> Result<c_int> {
    unsafe {
        push_metamethod(state, tag, c"__namecall");
        if !ffi::lua_isfunction(state, -1) {
            ffi::lua_pop(state, 1);
            return Err(Error::runtime("attempt to call a method on a userdata without __namecall"));
        }
        ffi::lua_insert(state, 1);
        let lua_call = crate::runtime::shared::LuaCall::enter(state);
        let status = ffi::lua_pcall(state, top, ffi::LUA_MULTRET, 0);
        drop(lua_call);
        if status != ffi::LUA_OK {
            return Err(Error::LuaErrorOnStack);
        }
        Ok(ffi::lua_gettop(state))
    }
}

/// `__index` fallback: a methods table is raw-indexed, a function is called with `(ud, key)`.
unsafe fn index_fallback(state: *mut ffi::lua_State, tag: c_int) -> Result<c_int> {
    unsafe {
        push_metamethod(state, tag, c"__index");
        if ffi::lua_istable(state, -1) {
            ffi::lua_pushvalue(state, 2);
            ffi::lua_rawget(state, -2);
            ffi::lua_remove(state, -2);
            return Ok(1);
        }
        if !ffi::lua_isfunction(state, -1) {
            ffi::lua_pop(state, 1);
            ffi::lua_pushnil(state);
            return Ok(1);
        }
        ffi::lua_insert(state, 1);
        let lua_call = crate::runtime::shared::LuaCall::enter(state);
        let status = ffi::lua_pcall(state, 2, 1, 0);
        drop(lua_call);
        if status != ffi::LUA_OK {
            return Err(Error::LuaErrorOnStack);
        }
        Ok(1)
    }
}

/// `__newindex` fallback: the metamethod called with `(ud, key, value)`.
unsafe fn newindex_fallback(state: *mut ffi::lua_State, tag: c_int) -> Result<c_int> {
    unsafe {
        push_metamethod(state, tag, c"__newindex");
        if !ffi::lua_isfunction(state, -1) {
            ffi::lua_pop(state, 1);
            return Err(Error::runtime("attempt to assign a field of a userdata without __newindex"));
        }
        ffi::lua_insert(state, 1);
        let lua_call = crate::runtime::shared::LuaCall::enter(state);
        let status = ffi::lua_pcall(state, 3, 0, 0);
        drop(lua_call);
        if status != ffi::LUA_OK {
            return Err(Error::LuaErrorOnStack);
        }
        Ok(0)
    }
}

/// `lua_UserdataDirectNamecall` for every planned type. Stack: `[ud, args...]`.
unsafe extern "C-unwind" fn namecall(
    state: *mut ffi::lua_State,
    _data: *mut c_void,
    atom: c_int,
    slot: *mut u16,
    utag: c_int,
) -> c_int {
    unsafe {
        trampoline::enter(state, || {
            let top = ffi::lua_gettop(state);
            let Some((shared, plan)) = planned(state) else { return namecall_fallback(state, utag, top) };
            let resolved = resolve(plan, slot, utag, atom as Atom, AccessKind::Namecall);
            match shared.direct_entry(resolved) {
                Some(entry) if resolved != UNKNOWN_SLOT => Ok(entry.call(state, top)),
                _ => namecall_fallback(state, utag, top),
            }
        })
    }
}

/// `lua_UserdataDirectAccess` for `__index`. Stack: `[ud, key]`; exactly one result.
unsafe extern "C-unwind" fn index(
    state: *mut ffi::lua_State,
    _data: *mut c_void,
    atom: c_int,
    slot: *mut u16,
    utag: c_int,
) {
    unsafe {
        trampoline::enter(state, || {
            let Some((shared, plan)) = planned(state) else { return index_fallback(state, utag) };
            let resolved = resolve(plan, slot, utag, atom as Atom, AccessKind::Index);
            match shared.direct_entry(resolved) {
                Some(entry) if resolved != UNKNOWN_SLOT => {
                    // The getter takes the receiver alone; the key leaves the stack so result
                    // counting sees only what the getter pushes.
                    ffi::lua_remove(state, 2);
                    let pushed = entry.call(state, 1);
                    if pushed == 0 {
                        ffi::lua_pushnil(state);
                    } else if pushed > 1 {
                        ffi::lua_settop(state, 2);
                    }
                    Ok(1)
                }
                _ => index_fallback(state, utag),
            }
        });
    }
}

/// `lua_UserdataDirectAccess` for `__newindex`. Stack: `[ud, key, value]`.
unsafe extern "C-unwind" fn newindex(
    state: *mut ffi::lua_State,
    _data: *mut c_void,
    atom: c_int,
    slot: *mut u16,
    utag: c_int,
) {
    unsafe {
        trampoline::enter(state, || {
            let Some((shared, plan)) = planned(state) else { return newindex_fallback(state, utag) };
            let resolved = resolve(plan, slot, utag, atom as Atom, AccessKind::NewIndex);
            match shared.direct_entry(resolved) {
                Some(entry) if resolved != UNKNOWN_SLOT => {
                    ffi::lua_remove(state, 2);
                    entry.call(state, 2);
                    Ok(0)
                }
                _ => newindex_fallback(state, utag),
            }
        });
    }
}

/// Registers the generic callbacks for `resolved`'s tag, for the access kinds its direct slots
/// use. The metatable must already exist and be read-only.
pub(crate) fn register(runtime: &Runtime, resolved: &ResolvedUserdata) -> Result<()> {
    let Some(tag) = resolved.tag else {
        return Err(Error::logic(format!("'{}' has direct slots but no tag", resolved.key)));
    };
    let kinds: Vec<AccessKind> =
        resolved.members.iter().filter(|m| m.slot.is_some()).map(super::plan::ResolvedMember::access_kind).collect();
    let has = |kind: AccessKind| kinds.contains(&kind);
    let stack = runtime.stack();
    stack.with_frame(|frame| {
        let state = frame.state();
        // SAFETY: balanced pushes within the frame; the metatable was registered read-only by
        // the planner just before this call.
        unsafe {
            ffi::lua_getuserdatametatable(state, c_int::from(tag));
            if !ffi::lua_istable(state, -1) {
                return Err(Error::logic(format!("'{}' has no metatable for tag {tag}", resolved.key)));
            }
            if ffi::lua_getreadonly(state, -1) == 0 {
                return Err(Error::logic(format!(
                    "'{}' metatable must be read-only before direct dispatch",
                    resolved.key
                )));
            }
            let registered = ffi::lua_registeruserdatadirectaccess(
                state,
                c_int::from(tag),
                has(AccessKind::Index).then_some(index as ffi::lua_UserdataDirectAccess),
                has(AccessKind::NewIndex).then_some(newindex as ffi::lua_UserdataDirectAccess),
                has(AccessKind::Namecall).then_some(namecall as ffi::lua_UserdataDirectNamecall),
            );
            if registered == 0 {
                return Err(Error::logic(format!("direct access registration failed for '{}'", resolved.key)));
            }
        }
        Ok(())
    })
}
