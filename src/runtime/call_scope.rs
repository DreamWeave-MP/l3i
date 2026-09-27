//! RAII call scopes (`Detail::ScriptCallScope` in luastate.hpp).
//!
//! A scope wraps one script call from the host: it pushes an active-call context so the
//! interrupt and allocation callbacks know a script is running, switches the memory category
//! when the profiler is on, arms the watchdog deadline (the outermost or earliest one wins),
//! and on drop records self time (elapsed minus nested), allocation activity, and restores the
//! previous category and deadline. A scope is a no-op when neither the profiler nor a limit
//! needs it.

use std::time::Instant;

use super::Runtime;
use super::shared::{ActiveCall, Deadline, MemoryCategory, Shared};

/// What kind of call a scope wraps; only the ordinary script call is timed and time-limited.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CallKind {
    /// A script entry point: timed, limited.
    ScriptCall,
    /// Sandbox or template setup: never time-limited, not timed.
    Initialization,
    /// Host code calling into Lua for its own purposes: limited but not timed.
    HostInterface,
}

/// Identifies who is running for accounting: a host-defined context id and the memory
/// category allocations are attributed to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CallContext {
    pub id: u64,
    pub category: MemoryCategory,
}

/// An active script call; see the module docs.
pub struct CallScope<'r> {
    runtime: &'r Runtime,
    active: bool,
    context: CallContext,
    started: Option<Instant>,
    previous_deadline: Option<Deadline>,
    previous_category: MemoryCategory,
}

impl Runtime {
    /// Opens a call scope for `context`. Scopes must nest strictly.
    pub fn call_scope(&self, context: CallContext, kind: CallKind) -> CallScope<'_> {
        let shared: &Shared = self.shared();
        let limits = shared.limits();
        let profiler = shared.profiler_enabled();
        let watchdog_needed =
            limits.memory_bytes != 0 || (kind != CallKind::Initialization && !limits.execution_time.is_zero());
        if !profiler && !watchdog_needed {
            return CallScope {
                runtime: self,
                active: false,
                context,
                started: None,
                previous_deadline: None,
                previous_category: MemoryCategory(0),
            };
        }

        let mut calls = shared.active_calls().borrow_mut();
        let previous_category = calls.last().map_or(MemoryCategory(0), |call| call.category);
        if profiler {
            self.set_memory_category(context.category);
        }
        let was_outermost = calls.is_empty();
        calls.push(ActiveCall { context: context.id, category: context.category, nested_ms: 0.0, allocated_bytes: 0 });
        drop(calls);

        let started = (kind == CallKind::ScriptCall && profiler).then(Instant::now);
        let previous_deadline = shared.deadline().get();
        if kind != CallKind::Initialization && !limits.execution_time.is_zero() {
            let start = Instant::now();
            let candidate = Deadline { start, end: start + limits.execution_time };
            if previous_deadline.is_none_or(|previous| candidate.end < previous.end) {
                shared.deadline().set(Some(candidate));
                shared.reset_watchdog_poll();
            }
        }
        if was_outermost && watchdog_needed {
            shared.reset_watchdog_poll();
        }
        CallScope { runtime: self, active: true, context, started, previous_deadline, previous_category }
    }
}

impl Drop for CallScope<'_> {
    fn drop(&mut self) {
        if !self.active {
            return;
        }
        let shared = self.runtime.shared();
        let (context, nested_ms, allocated) = {
            let calls = shared.active_calls().borrow();
            let innermost = calls.last().expect("a scope is open");
            debug_assert_eq!(innermost.context, self.context.id, "call scopes must nest strictly");
            (innermost.context, innermost.nested_ms, innermost.allocated_bytes)
        };
        // Unmeasured scopes pass their nested time through, so it is never counted twice.
        let mut elapsed_ms = nested_ms;
        if let Some(started) = self.started {
            let end = Instant::now();
            elapsed_ms = end.duration_since(started).as_secs_f64() * 1000.0;
            let sample_overhead = shared.add_call_time(context, (elapsed_ms - nested_ms).max(0.0));
            if sample_overhead {
                shared.add_overhead_sample(Instant::now().duration_since(end));
            }
        }
        {
            let mut calls = shared.active_calls().borrow_mut();
            calls.pop();
            if let Some(parent) = calls.last_mut() {
                parent.nested_ms += elapsed_ms;
            }
        }
        if allocated != 0 {
            shared.add_allocation_activity(context, allocated);
        }
        if shared.profiler_enabled() {
            self.runtime.set_memory_category(self.previous_category);
        }
        shared.deadline().set(self.previous_deadline);
    }
}
