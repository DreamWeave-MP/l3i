//! Per-VM state reachable from Luau callbacks (`Lua::State` in luastate.cpp), and the
//! callbacks themselves: the interrupt watchdog and allocation activity.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::ffi::{CStr, c_int, c_void};
use std::time::{Duration, Instant};

use crate::raw::ffi;

/// A Luau memory category (0..256). Ids and their meanings are host data; OpenMW uses 0 for
/// shared, 1 global, 2 menu, 3 player, and so on.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct MemoryCategory(pub u8);

/// Watchdog limits. Zero disables a limit.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Limits {
    /// Wall-clock budget per outermost script call.
    pub execution_time: Duration,
    /// Heap ceiling in bytes across all categories.
    pub memory_bytes: usize,
}

/// One active call on the scope stack.
#[derive(Clone, Copy, Debug)]
pub(crate) struct ActiveCall {
    pub context: u64,
    pub category: MemoryCategory,
    pub nested_ms: f64,
    pub allocated_bytes: u64,
}

/// A watchdog deadline: when the outermost call started and when it must end.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Deadline {
    pub start: Instant,
    pub end: Instant,
}

/// Accumulated profiler statistics.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct CallStats {
    /// Total measured self time of script calls, in milliseconds.
    pub total_script_ms: f64,
    /// Self time per host context id.
    pub time_by_context_ms: HashMap<u64, f64>,
    /// Allocation activity (bytes grown) per host context id.
    pub allocation_by_context: HashMap<u64, u64>,
    /// Timed calls, and the overhead samples taken every 64th call.
    pub timed_calls: u64,
    pub overhead_samples: u64,
    pub overhead_total_ms: f64,
}

pub(crate) const SAFEPOINTS_PER_WATCHDOG_POLL: u32 = 64;
const TIMED_CALLS_PER_OVERHEAD_SAMPLE: u64 = 64;

pub(crate) struct Shared {
    limits: Cell<Limits>,
    profiler: bool,
    active_calls: RefCell<Vec<ActiveCall>>,
    deadline: Cell<Option<Deadline>>,
    safepoints_until_poll: Cell<u32>,
    stats: RefCell<CallStats>,
}

impl Shared {
    pub(crate) fn new(limits: Limits, profiler: bool) -> Shared {
        Shared {
            limits: Cell::new(limits),
            profiler,
            active_calls: RefCell::new(Vec::new()),
            deadline: Cell::new(None),
            safepoints_until_poll: Cell::new(0),
            stats: RefCell::new(CallStats::default()),
        }
    }

    pub(crate) fn profiler_enabled(&self) -> bool {
        self.profiler
    }

    pub(crate) fn limits(&self) -> Limits {
        self.limits.get()
    }

    pub(crate) fn set_limits(&self, limits: Limits) {
        self.limits.set(limits);
    }

    pub(crate) fn needs_interrupt(&self) -> bool {
        let limits = self.limits.get();
        !limits.execution_time.is_zero() || limits.memory_bytes != 0
    }

    pub(crate) fn stats(&self) -> CallStats {
        self.stats.borrow().clone()
    }

    pub(crate) fn active_calls(&self) -> &RefCell<Vec<ActiveCall>> {
        &self.active_calls
    }

    pub(crate) fn deadline(&self) -> &Cell<Option<Deadline>> {
        &self.deadline
    }

    pub(crate) fn reset_watchdog_poll(&self) {
        self.safepoints_until_poll.set(0);
    }

    /// Records a measured call; returns true when this call should sample its own overhead.
    pub(crate) fn add_call_time(&self, context: u64, self_ms: f64) -> bool {
        let mut stats = self.stats.borrow_mut();
        stats.total_script_ms += self_ms;
        *stats.time_by_context_ms.entry(context).or_insert(0.0) += self_ms;
        stats.timed_calls += 1;
        stats.timed_calls.is_multiple_of(TIMED_CALLS_PER_OVERHEAD_SAMPLE)
    }

    pub(crate) fn add_overhead_sample(&self, elapsed: Duration) {
        let mut stats = self.stats.borrow_mut();
        stats.overhead_samples += 1;
        stats.overhead_total_ms += elapsed.as_secs_f64() * 1000.0;
    }

    pub(crate) fn add_allocation_activity(&self, context: u64, bytes: u64) {
        *self.stats.borrow_mut().allocation_by_context.entry(context).or_insert(0) += bytes;
    }
}

/// The `Shared` block of the VM that owns `state`, or null before the runtime is set up.
///
/// # Safety
/// `state` is a live thread whose VM was created by [`crate::Runtime`].
unsafe fn shared_of(state: *mut ffi::lua_State) -> *const Shared {
    unsafe { (*ffi::lua_callbacks(state)).userdata.cast_const().cast::<Shared>() }
}

/// The interrupt hook (`State::interruptHook`): runs at VM safepoints (`gc == -1`) and during
/// GC (any other value, ignored so collection never charges a script's budget). Every 64th
/// safepoint it polls the deadline and the memory limit and raises when either is exceeded.
pub(crate) unsafe extern "C-unwind" fn interrupt(state: *mut ffi::lua_State, gc: c_int) {
    if gc != -1 {
        return;
    }
    // SAFETY: userdata is the Shared block installed at build time and outlives the VM.
    let shared = unsafe { shared_of(state) };
    if shared.is_null() {
        return;
    }
    let shared = unsafe { &*shared };
    if shared.active_calls.borrow().is_empty() {
        return;
    }
    let remaining = shared.safepoints_until_poll.get();
    if remaining != 0 {
        shared.safepoints_until_poll.set(remaining - 1);
        return;
    }
    shared.safepoints_until_poll.set(SAFEPOINTS_PER_WATCHDOG_POLL - 1);

    if let Some(deadline) = shared.deadline.get() {
        let now = Instant::now();
        if now >= deadline.end {
            let elapsed = now.duration_since(deadline.start).as_millis();
            let message = format!("Lua execution time limit exceeded after {elapsed} ms");
            // SAFETY: raising from the interrupt is how Luau's watchdog contract works; no
            // Rust value that must drop is live here once `message` is pushed.
            unsafe {
                ffi::lua_pushlstring(state, message.as_ptr().cast(), message.len());
                drop(message);
                ffi::lua_error(state);
            }
        }
    }
    let memory_limit = shared.limits.get().memory_bytes;
    // SAFETY: lua_totalbytes(-1) is a plain read.
    if memory_limit != 0 && unsafe { ffi::lua_totalbytes(state, -1) } > memory_limit {
        unsafe {
            ffi::lua_pushlstring(state, c"Lua memory limit exceeded".as_ptr(), 25);
            ffi::lua_error(state);
        }
    }
}

/// Allocation activity (`State::onAllocate`): growth is charged to the innermost active call.
/// Luau does not report every fixed GC allocation here, so this is activity, not ownership.
pub(crate) unsafe extern "C" fn on_allocate(
    state: *mut ffi::lua_State,
    _block: *mut c_void,
    old_size: usize,
    new_size: usize,
    _category: u8,
    _tt: c_int,
    _tag: c_int,
) {
    if new_size <= old_size {
        return;
    }
    let shared = unsafe { shared_of(state) };
    if shared.is_null() {
        return;
    }
    let shared = unsafe { &*shared };
    if let Ok(mut calls) = shared.active_calls.try_borrow_mut()
        && let Some(innermost) = calls.last_mut()
    {
        innermost.allocated_bytes += (new_size - old_size) as u64;
    }
}

/// `source:line` of the innermost Lua frame on `state`, or empty.
pub(crate) fn caller_location(state: *mut ffi::lua_State) -> String {
    let mut level = 0;
    loop {
        let mut ar = std::mem::MaybeUninit::<ffi::lua_Debug>::zeroed();
        // SAFETY: lua_getinfo fills the "sl" fields; pointers reference ar.ssbuf or interned
        // strings that outlive the read.
        unsafe {
            if ffi::lua_getinfo(state, level, c"sl".as_ptr(), ar.as_mut_ptr()) == 0 {
                return String::new();
            }
            let ar = ar.assume_init_ref();
            if !ar.what.is_null() && CStr::from_ptr(ar.what).to_bytes() == b"Lua" {
                let source = if ar.source.is_null() { ar.short_src } else { ar.source };
                let mut source = CStr::from_ptr(source).to_string_lossy().into_owned();
                if source.starts_with('@') || source.starts_with('=') {
                    source.remove(0);
                }
                return format!("{source}:{}", ar.currentline);
            }
        }
        level += 1;
    }
}
