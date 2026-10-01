//! The integration tests, linked as one binary: with cross-language LTO every test binary
//! pays a full link-time codegen, so one link instead of seventeen keeps `cargo test` fast.
//! Run one file's tests with `cargo test <file>::`.

// The shared CI runs `-W clippy::pedantic -D warnings`; tests compare floats exactly on purpose
// and read like the C++ contracts they port.
#![allow(
    clippy::float_cmp,
    clippy::doc_markdown,
    clippy::must_use_candidate,
    clippy::missing_errors_doc,
    clippy::missing_panics_doc,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss,
    clippy::cast_precision_loss,
    clippy::cast_lossless,
    clippy::borrow_as_ptr,
    clippy::ref_as_ptr,
    clippy::similar_names,
    clippy::too_many_lines,
    clippy::unreadable_literal,
    clippy::items_after_statements,
    clippy::needless_pass_by_value
)]

#[cfg(feature = "analysis")]
mod analysis;
#[cfg(feature = "bytes")]
mod bytes;
mod call;
mod debug;
mod direct;
mod extension;
#[cfg(feature = "fs")]
mod fs;
#[cfg(feature = "intern")]
mod intern;
mod iterator;
mod libraries;
mod memory;
mod metatable;
mod module;
#[cfg(feature = "jit")]
mod native_code;
mod net;
mod primitives;
#[cfg(feature = "process")]
mod process;
mod quat;
mod raster;
mod require;
mod sandbox;
#[cfg(feature = "soft-render")]
mod soft_render;
#[cfg(feature = "syntax")]
mod syntax;
mod tagged;
mod thread;
#[cfg(all(feature = "analysis", feature = "soft-render"))]
mod typed_definitions;
mod untagged;
mod vector_writer;
mod watchdog;
