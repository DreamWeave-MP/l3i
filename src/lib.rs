//! Luau binder for DreamWeave component crates.
//!
//! The host application owns the [`mlua::Lua`]; component crates register bindings into it
//! through the shared contract in this crate. See the README for the usage tiers and the
//! ownership models.

pub mod error;
mod raw;
pub mod stack;

pub use error::{Error, Result};

/// Number of userdata tags the linked Luau VM was compiled with (`LUA_UTAG_LIMIT`).
///
/// Comes from the `LUAU_CXXFLAGS` the host built Luau with, not from `mlua_sys`, whose
/// constant is hard-coded to 128. Valid runtime tags are `2..TAG_LIMIT`; tags 0 and 1 are
/// reserved (`mlua` stamps tag 1 on userdata it has moved a value out of).
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

#[cfg(test)]
mod tests {
    #[test]
    fn tag_limit_matches_the_configured_build() {
        assert_eq!(super::TAG_LIMIT, 254);
    }
}
