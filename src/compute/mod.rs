//! Typed f32 computations compiled once and run against any compatible inputs.
//!
//! The `compute` feature is the engine. `compute-testing` adds what only its tests and benches
//! need: the per-element oracle, generated-code comparators and the workloads they share.
//!
//! - [`graph`]: expression nodes (structure and shapes, no data).
//! - [`program`]: validated DAG, stage plan and execution entry points.
//! - [`kernels`]: the monomorphic bulk loops that do the arithmetic.
//! - [`storage`]: immutable owners, checked views, the payload budget.
//! - [`reference`]: the per-element interpreter, kept as an oracle and a baseline.
//! - [`lowering`]: the same validated plan as generated Luau and as a Jess shader program.
//! - [`forward`]: a synthetic forward pass, the composition workload tests and benches share.
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
mod tests;

pub use graph::{Domain, Expr, Instruction, Step};
pub use kernels::{CHUNK, Isa};
pub use program::{Numeric, Program};
pub use storage::{Budget, Shape, Storage, Tensor, View};

pub(crate) const MAX_ELEMENTS: usize = 1_048_576;

pub(crate) fn error(message: &str) -> crate::Error {
    crate::Error::runtime(message)
}

/// Experimental allocation-free row reduction kernels.
pub mod reduction;

/// Experimental exact-accumulation int8 primitives.
#[cfg(feature = "compute-testing")]
pub mod quantized;
