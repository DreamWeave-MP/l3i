//! The system allocator, counting calls and bytes, for the benches that report native
//! allocations and retention (`intl`, `tcp`, `dns_tls`). One binary has one global allocator,
//! so it lives here and is installed by `main.rs` only when one of those benches is compiled in;
//! the Criterion benches under the default features run on the plain system allocator.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

pub struct Counting;

/// Allocation calls, `alloc` and `realloc` alike.
pub static ALLOCATIONS: AtomicU64 = AtomicU64::new(0);
/// Bytes requested, growth only: a `realloc` counts what it added.
pub static ALLOCATED: AtomicU64 = AtomicU64::new(0);
/// Bytes currently held.
pub static LIVE: AtomicUsize = AtomicUsize::new(0);

// SAFETY: forwards to `System` unchanged; the counters are plain atomics.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        ALLOCATED.fetch_add(layout.size() as u64, Ordering::Relaxed);
        LIVE.fetch_add(layout.size(), Ordering::Relaxed);
        // SAFETY: forwarded.
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        LIVE.fetch_sub(layout.size(), Ordering::Relaxed);
        // SAFETY: forwarded.
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        ALLOCATED.fetch_add(new_size.saturating_sub(layout.size()) as u64, Ordering::Relaxed);
        LIVE.fetch_add(new_size, Ordering::Relaxed);
        LIVE.fetch_sub(layout.size(), Ordering::Relaxed);
        // SAFETY: forwarded.
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}
