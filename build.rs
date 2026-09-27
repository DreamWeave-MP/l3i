use std::env;

// luau0-src appends LUAU_CXXFLAGS to every Luau compile. Read the same variable so the
// Rust-side tag validation matches the limit the VM was actually built with.
fn main() {
    println!("cargo:rerun-if-env-changed=LUAU_CXXFLAGS");
    let mut limit: u32 = 128;
    if let Ok(flags) = env::var("LUAU_CXXFLAGS") {
        for flag in flags.split_whitespace() {
            if let Some(value) = flag.strip_prefix("-DLUA_UTAG_LIMIT=") {
                limit = value
                    .parse()
                    .unwrap_or_else(|_| panic!("LUAU_CXXFLAGS has an unparseable LUA_UTAG_LIMIT: {value}"));
            }
        }
    }
    // Udata::tag is a uint8_t and Luau reserves LUA_UTAG_LIMIT itself for LU_TAG_ITERATOR.
    assert!(
        (2..=254).contains(&limit),
        "LUA_UTAG_LIMIT must be between 2 and 254, got {limit}"
    );
    println!("cargo:rustc-env=DREAM_BINDER_TAG_LIMIT={limit}");
}
