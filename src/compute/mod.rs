//! Typed f32 computations compiled once and run against any compatible inputs.
//!
//! The `compute` feature is the engine. `compute-testing` adds what only its tests and benches
//! need: the per-element oracle, generated-code comparators and the workloads they share.
//!
//! - [`graph`]: expression nodes (structure and shapes, no data).
//! - [`program`]: validated DAG, stage plan and execution entry points.
//! - [`kernels`]: the monomorphic bulk loops that do the arithmetic.
//! - [`storage`]: immutable owners, checked views, the payload budget.
//! - `reference`: the per-element interpreter, kept as an oracle and a baseline.
//! - `lowering`: the same validated plan as generated Luau and as a Jess shader program.
//! - `forward`: a synthetic forward pass, the composition workload tests and benches share.
#![allow(clippy::missing_errors_doc, clippy::missing_panics_doc)]
#[cfg(feature = "compute-testing")]
pub mod forward;
pub mod graph;
pub mod kernels;
#[cfg(feature = "compute-testing")]
pub mod lowering;
pub mod program;
#[cfg(feature = "compute-testing")]
pub mod reference;
pub mod storage;

#[cfg(all(test, feature = "compute-testing"))]
mod adversarial;
#[cfg(all(test, feature = "compute-testing"))]
mod audit;
#[cfg(all(test, feature = "compute-testing"))]
mod parallel;
#[cfg(all(test, feature = "compute-testing"))]
mod tests;

pub use graph::{Domain, Expr, Instruction, Step};
pub use kernels::{CHUNK, Isa};
pub use program::{Numeric, Program};
pub use storage::{Budget, Shape, Storage, Tensor, View};

/// The most elements any shape can have: what the address space and this crate's byte arithmetic can
/// represent, leaving room for a packing's padding (at most 8x) and four bytes each without overflow.
/// This is a mechanical limit. What a host allows is [`Limits`], and is much smaller by default.
pub(crate) const ADDRESSABLE_ELEMENTS: usize = isize::MAX.unsigned_abs() / 64;

/// What a host allows one graph and one tensor. Nothing here is a limit of the engine: these are
/// policy, conservative by default for untrusted scripts, and every check still uses checked
/// arithmetic whatever they are set to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    /// Elements in one tensor or graph value, and in each of its dimensions.
    pub elements: usize,
    /// Scalar operations one run of a program may perform, as its plan counts them.
    pub work: usize,
}
impl Default for Limits {
    fn default() -> Self {
        Self { elements: 1 << 20, work: 1 << 26 }
    }
}

/// A host's workers, lent to a run. The engine keeps no threads of its own: scheduling, priorities and
/// how many workers a call may have are the host's, and a host that lends none gets one thread.
pub trait Executor: Send + Sync + std::fmt::Debug {
    /// How many tasks can usefully run at once. A run asks for no more than this.
    fn workers(&self) -> usize;
    /// Calls `task(i)` once for each `i` in `0..tasks`, on whatever threads it likes, and returns when
    /// every call has returned. Tasks run no Lua and don't block on each other. Breaking this can't
    /// break memory safety and never passes for a result: a task skipped is reported as an error. But
    /// the tasks that did run have written their shares by then, so that error is the one a run can
    /// give with its output partly written. Copying every output to guard against it would cost every
    /// host that keeps this contract.
    fn run(&self, tasks: usize, task: &(dyn Fn(usize) + Sync));
}

pub(crate) fn error(message: &str) -> crate::Error {
    crate::Error::runtime(message)
}

/// Asked before every fallible allocation: a test decides whether it fails and sees what is charged.
#[cfg(test)]
pub(crate) fn may_allocate(bytes: usize) -> bool {
    ALLOCATION_HOOK.with_borrow_mut(|hook| hook.as_mut().is_none_or(|hook| hook(bytes)))
}
#[cfg(not(test))]
#[inline(always)]
#[allow(clippy::inline_always)] // This must vanish from every kernel path.
pub(crate) fn may_allocate(_bytes: usize) -> bool {
    true
}
#[cfg(test)]
type AllocationHook = Box<dyn FnMut(usize) -> bool>;
#[cfg(test)]
thread_local! {
    static ALLOCATION_HOOK: std::cell::RefCell<Option<AllocationHook>> = const { std::cell::RefCell::new(None) };
}
/// `body`, with `hook` answering [`may_allocate`] on this thread. The hook may borrow locals because
/// it is gone again before this returns.
#[cfg(all(test, feature = "compute-testing"))]
pub(crate) fn with_allocation_hook<R>(mut hook: impl FnMut(usize) -> bool, body: impl FnOnce() -> R) -> R {
    let local: &mut dyn FnMut(usize) -> bool = &mut hook;
    // SAFETY: only the lifetime changes, and `Reset` clears the slot before `hook` can die, also on panic.
    let stored: *mut (dyn FnMut(usize) -> bool + 'static) = unsafe { std::mem::transmute(local) };
    struct Reset;
    impl Drop for Reset {
        fn drop(&mut self) {
            ALLOCATION_HOOK.with_borrow_mut(|slot| *slot = None);
        }
    }
    // SAFETY: as above; the box only forwards.
    ALLOCATION_HOOK.with_borrow_mut(|slot| *slot = Some(Box::new(move |bytes| unsafe { (*stored)(bytes) })));
    let _reset = Reset;
    body()
}

/// Experimental allocation-free row reduction kernels.
pub mod reduction;

/// The `@dream/compute` extension.
pub mod script;

/// Experimental exact-accumulation int8 primitives.
#[cfg(feature = "compute-testing")]
pub mod quantized;
