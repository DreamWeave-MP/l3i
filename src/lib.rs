//! Luau binder for DreamWeave component crates.
//!
//! The host application owns the [`runtime::Runtime`]; component crates register bindings
//! into it through the shared contract in this crate. See the README for the usage tiers and the
//! ownership models.

pub mod error;
mod raw;
pub mod runtime;
pub mod source;
pub mod stack;

pub use error::{Error, Result};

/// Number of userdata tags the linked Luau VM was compiled with (`LUA_UTAG_LIMIT`).
///
/// Comes from the `LUAU_CXXFLAGS` the host built Luau with. Valid runtime tags are
/// `1..TAG_LIMIT`; tag 0 is Luau's untagged default and is never registered.
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
