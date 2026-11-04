//! Typed f32 computations compiled once and run against any compatible inputs. Isolated research.
//!
//! - [`graph`]: expression nodes (structure and shapes, no data).
//! - [`program`]: validated DAG, stage plan and execution entry points.
//! - [`kernels`]: the monomorphic bulk loops that do the arithmetic.
//! - [`storage`]: immutable owners, checked views, the payload budget.
//! - [`reference`]: the per-element interpreter, kept as an oracle and a baseline.
//! - [`lowering`]: the same validated plan as generated Luau and as a Jess shader program.
#![allow(clippy::missing_errors_doc, clippy::missing_panics_doc)]
pub mod graph;
pub mod kernels;
pub mod lowering;
pub mod program;
pub mod reference;
pub mod storage;

#[cfg(test)]
mod tests;

pub use graph::{Domain, Expr, Instruction, Step};
pub use kernels::{CHUNK, Isa};
pub use program::{Numeric, Program};
pub use storage::{Budget, Shape, Storage, Tensor, View};

pub(crate) const MAX_ELEMENTS: usize = 1_048_576;

pub(crate) fn error(message: &str) -> l3i::Error {
    l3i::Error::runtime(message)
}

/// Experimental allocation-free row reduction kernels.
pub mod reduction;
