//! The VM's allocator: the system heap behind a running byte count that enforces the memory
//! limit at the allocation itself.
//!
//! Every block Luau allocates (objects, GC pages, stacks, strings and buffers) goes through
//! [`allocate`]. A growth that would take the total past the limit gets `NULL`, which Luau turns
//! into `LUA_ERRMEM` at the allocation, catchable by `pcall`; so a script can't take a gigabyte
//! with one `buffer.create` between two polls. Frees and shrinks always succeed, so collection,
//! finalizers and `lua_close` work at any total.
//!
//! What the count does not see: buffers routed through a `memory::BufferCage`, and memory a
//! native extension allocates from Rust (`@dream/bytes`, `@dream/data`, `@dream/fs` readers).

use std::cell::Cell;
use std::ffi::c_void;

/// The VM's byte count and limit, the allocator's userdata. Lives in the runtime's shared block,
/// which outlives `lua_close`.
#[derive(Debug, Default)]
pub(crate) struct MemoryAccount {
    /// Zero is unlimited.
    limit: Cell<usize>,
    total: Cell<usize>,
    /// Set when the allocator refused a growth, until the next host transition: an
    /// out-of-memory failure with it set is the limit, not the system.
    refused: Cell<bool>,
}

impl MemoryAccount {
    pub(crate) fn new(limit: usize) -> MemoryAccount {
        MemoryAccount { limit: Cell::new(limit), total: Cell::new(0), refused: Cell::new(false) }
    }

    pub(crate) fn set_limit(&self, limit: usize) {
        self.limit.set(limit);
    }

    /// Whether a growth was refused since [`MemoryAccount::forget_refusal`].
    pub(crate) fn refused(&self) -> bool {
        self.refused.get()
    }

    pub(crate) fn forget_refusal(&self) {
        self.refused.set(false);
    }
}

unsafe extern "C" {
    fn realloc(block: *mut c_void, size: usize) -> *mut c_void;
    fn free(block: *mut c_void);
}

/// `lua_Alloc` over the C heap, as `luaL_newstate`'s own allocator, with the count and limit of
/// the [`MemoryAccount`] `ud` points at. Never panics and never unwinds.
///
/// # Safety
/// `ud` is the account the VM was created with; `block` and `old_size` describe a block this
/// allocator returned (or null and zero for a new one), as Luau's `frealloc` contract says.
pub(crate) unsafe extern "C" fn allocate(
    ud: *mut c_void,
    block: *mut c_void,
    old_size: usize,
    new_size: usize,
) -> *mut c_void {
    // SAFETY: forwarded contract; the account outlives the VM.
    let account = unsafe { &*ud.cast::<MemoryAccount>() };
    let old_size = if block.is_null() { 0 } else { old_size };
    let rest = account.total.get().saturating_sub(old_size);
    if new_size == 0 {
        // SAFETY: a block this allocator returned, or null.
        unsafe { free(block) };
        account.total.set(rest);
        return std::ptr::null_mut();
    }
    let limit = account.limit.get();
    if new_size > old_size && limit != 0 && rest.saturating_add(new_size) > limit {
        account.refused.set(true);
        return std::ptr::null_mut();
    }
    // SAFETY: a block this allocator returned, or null; a failed realloc leaves it intact.
    let grown = unsafe { realloc(block, new_size) };
    if !grown.is_null() {
        account.total.set(rest.saturating_add(new_size));
    }
    grown
}
