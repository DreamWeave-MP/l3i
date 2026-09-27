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

pub mod bind;
pub mod call;
pub mod convert;
pub mod debug;
pub mod debug_name;
pub mod diagnostics;
pub mod direct;
pub mod error;
pub mod flags;
pub mod module;
pub mod native;
#[cfg(feature = "jit")]
pub mod native_code;
mod raw;
pub mod readonly;
pub mod runtime;
pub mod sandbox;
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
/// dream-binder builds Luau with 254, the most Luau can address, and `build.rs` is the single
/// owner of that define. Valid runtime tags are `1..TAG_LIMIT`; tag 0 is Luau's untagged
/// default and is never registered.
pub const TAG_LIMIT: u8 = parse_tag_limit(env!("DREAM_BINDER_TAG_LIMIT"));

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
