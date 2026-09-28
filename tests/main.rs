//! The integration tests, linked as one binary: with cross-language LTO every test binary
//! pays a full link-time codegen, so one link instead of seventeen keeps `cargo test` fast.
//! Run one file's tests with `cargo test <file>::`.

#[cfg(feature = "analysis")]
mod analysis;
mod call;
mod debug;
mod direct;
mod iterator;
mod libraries;
mod memory;
mod metatable;
mod module;
#[cfg(feature = "jit")]
mod native_code;
mod require;
mod sandbox;
mod tagged;
mod thread;
mod untagged;
mod vector_writer;
mod watchdog;
