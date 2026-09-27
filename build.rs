use std::env;

// Max stack slots a C function may use. Luau's default; OpenMW builds with it too, and
// LUA_REGISTRYINDEX in src/raw/ffi.rs is derived from this same number.
const MAX_CSTACK: usize = 8000;

// Luau's default is 128 userdata tags. The binder uses every tag Luau can address: Udata::tag
// is a uint8_t and Luau keeps LUA_UTAG_LIMIT itself for LU_TAG_ITERATOR, so 254 is the ceiling
// and the value OpenMW builds with. dream-binder owns the Luau build, so it owns this ABI
// choice; src/lib.rs mirrors it as TAG_LIMIT. Hosts may append their own flags through
// LUAU_CXXFLAGS but cannot lower this one.
const TAG_LIMIT: u32 = 254;

fn main() {
    println!("cargo:rerun-if-env-changed=LUAU_CXXFLAGS");
    println!("cargo:rustc-env=DREAM_BINDER_TAG_LIMIT={TAG_LIMIT}");

    // luau0-src reads LUAU_CXXFLAGS from this process's environment while compiling Luau.
    let mut flags = format!("-DLUA_UTAG_LIMIT={TAG_LIMIT}");
    if let Ok(extra) = env::var("LUAU_CXXFLAGS") {
        assert!(
            !extra.contains("LUA_UTAG_LIMIT"),
            "LUA_UTAG_LIMIT is fixed at {TAG_LIMIT} by dream-binder; remove it from LUAU_CXXFLAGS"
        );
        flags.push(' ');
        flags.push_str(&extra);
    }
    // SAFETY: build scripts are single-threaded at this point; no other thread reads the
    // environment concurrently.
    unsafe { env::set_var("LUAU_CXXFLAGS", &flags) };

    let artifacts = luau0_src::Build::new()
        .set_max_cstack_size(MAX_CSTACK)
        .set_vector_size(3)
        .enable_codegen(env::var_os("CARGO_FEATURE_JIT").is_some())
        .build();
    artifacts.print_cargo_metadata();
}
