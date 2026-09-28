//! Luau binder for DreamWeave component crates: a Rust port of the OpenMW Luau binder that
//! owns its own Luau 0.740 build (no intermediate Lua binding crate).
//!
//! The host application owns the [`runtime::Runtime`]; component crates register bindings
//! into it through the shared contract in this crate.
//!
//! Three usage tiers, as in the C++ binder:
//! - hot path: the borrowed [`stack`] layer (`Stack`, `Frame`, `ValueView`, `TableView`), no
//!   registry pins, lifetimes tied to the frame that owns the slot;
//! - middle tier: owned registry pins (`lua_ref`), for values that outlive a call;
//! - cold tier: convenience lookups that may push temporaries and pin.
//!
//! Error model: Luau raises C++ exceptions. Inside a native call they unwind through the
//! binding's Rust frames to Luau's `pcall`; a Rust panic inside a native call aborts. Host-level
//! code never sees a Luau raise: operations that can raise run under `lua_pcall` there.

// Pedantic Clippy policy. The shared CI runs `-W clippy::pedantic -D warnings`; the groups
// below are deliberate in an FFI binder, each for the stated reason. Everything else pedantic
// flags is fixed or justified at the site.
#![allow(
    // Every `Result` is a Luau error or a host logic error; the module docs say which raises.
    clippy::missing_errors_doc,
    clippy::missing_panics_doc,
    // A binder API is mostly accessors; `#[must_use]` on each would be noise.
    clippy::must_use_candidate,
    clippy::return_self_not_must_use,
    // OpenMW, Luau, and C++ identifiers appear in prose.
    clippy::doc_markdown,
    // The hot paths in BENCHMARKS.md were measured with these; see PERF commits.
    clippy::inline_always,
    // FFI call sites take `&x as *const T` everywhere.
    clippy::borrow_as_ptr,
    clippy::ref_as_ptr,
    // Luau's C API is `c_int`/`size_t`-shaped; conversions are bounded where it matters.
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss,
    clippy::cast_precision_loss,
    clippy::cast_lossless,
    // View and frame lifetimes are spelled out on purpose.
    clippy::needless_lifetimes,
    clippy::elidable_lifetime_names,
    // Argument tuples and pinned values are moved into calls by design.
    clippy::needless_pass_by_value,
    // Exact float round-trips are part of the conversion contract (and its tests).
    clippy::float_cmp,
    // Inner `ffi` modules mirror C headers with `use super::*`.
    clippy::wildcard_imports,
    // Style choices: local callbacks next to their use, explicit match arms per Luau type.
    clippy::items_after_statements,
    clippy::single_match_else,
    clippy::match_same_arms,
    clippy::similar_names,
    clippy::struct_excessive_bools
)]

#[cfg(feature = "analysis")]
pub mod analysis;
pub mod bind;
pub mod call;
pub mod convert;
pub mod debug;
pub mod debug_name;
pub mod diagnostics;
pub mod direct;
pub mod error;
pub mod extension;
pub mod flags;
pub mod libraries;
pub mod memory;
pub mod module;
pub mod native;
#[cfg(feature = "jit")]
pub mod native_code;
pub mod net;
pub mod options;
pub mod packed;
pub mod quat;
pub mod raster;
mod raw;
pub mod readonly;
pub mod require;
pub mod runtime;
pub mod sandbox;
pub mod sequence;
pub mod source;
pub mod stack;
pub mod thread;
pub mod userdata;
pub mod value;
pub mod vector_writer;

pub use error::{Error, Result};
pub use runtime::Runtime;

/// The raw Luau C API for hand-written native functions. Everything here is `unsafe`; prefer
/// the safe layers.
pub mod ffi {
    pub use crate::raw::ffi::*;
}

/// Number of userdata tags the linked Luau VM was compiled with (`LUA_UTAG_LIMIT`).
///
/// l3i builds Luau with 254, the most Luau can address, and `build.rs` is the single
/// owner of that define. Valid runtime tags are `1..TAG_LIMIT`; tag 0 is Luau's untagged
/// default and is never registered.
pub const TAG_LIMIT: u8 = parse_tag_limit(env!("L3I_TAG_LIMIT"));

const fn parse_tag_limit(text: &str) -> u8 {
    let bytes = text.as_bytes();
    let mut value: u32 = 0;
    let mut i = 0;
    while i < bytes.len() {
        value = value * 10 + (bytes[i] - b'0') as u32;
        i += 1;
    }
    assert!(value >= 2 && value <= 254);
    value as u8
}

/// The Luau release the linked VM was built from, e.g. `"0.740"`: the release OpenMW pins.
pub const LUAU_VERSION: &str = env!("LUAU_VERSION");

#[cfg(test)]
mod tests {
    #[test]
    fn tag_limit_matches_the_configured_build() {
        assert_eq!(super::TAG_LIMIT, 254);
    }

    #[test]
    fn luau_is_the_release_openmw_pins() {
        assert_eq!(super::LUAU_VERSION, "0.740");
    }
}
