use std::env;
use std::path::{Path, PathBuf};

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

    let jit = env::var_os("CARGO_FEATURE_JIT").is_some();
    if jit {
        build_native_code_shim(&flags);
    }

    let artifacts =
        luau0_src::Build::new().set_max_cstack_size(MAX_CSTACK).set_vector_size(3).enable_codegen(jit).build();
    artifacts.print_cargo_metadata();
}

/// The `luau/` source tree luau0-src compiled, for the C++ headers the code generation shim
/// includes. luau0-src does not export it, so it is located the way Cargo lays it out:
/// `LUAU0_SRC_DIR` overrides; otherwise the registry checkout matching the pinned version,
/// then a `vendor/` directory next to this manifest.
fn luau_source_dir() -> PathBuf {
    println!("cargo:rerun-if-env-changed=LUAU0_SRC_DIR");
    if let Some(dir) = env::var_os("LUAU0_SRC_DIR") {
        let dir = PathBuf::from(dir);
        assert!(dir.join("VM/include/lua.h").is_file(), "LUAU0_SRC_DIR does not contain a Luau source tree");
        return dir;
    }
    let mut roots = Vec::new();
    if let Some(cargo_home) = env::var_os("CARGO_HOME")
        .map(PathBuf::from)
        .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".cargo")))
    {
        let registry = cargo_home.join("registry").join("src");
        if let Ok(entries) = registry.read_dir() {
            roots.extend(entries.flatten().map(|entry| entry.path()));
        }
    }
    roots.push(Path::new(env!("CARGO_MANIFEST_DIR")).join("vendor"));
    for root in roots {
        let Ok(entries) = root.read_dir() else { continue };
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name.starts_with(&format!("luau0-src-{LUAU0_SRC_VERSION}")) {
                let candidate = entry.path().join("luau");
                if candidate.join("VM/include/lua.h").is_file()
                    && candidate.join("CodeGen/include/Luau/CodeGen.h").is_file()
                {
                    return candidate;
                }
            }
        }
    }
    panic!(
        "cannot find the luau0-src {LUAU0_SRC_VERSION} source tree for the native code shim; set LUAU0_SRC_DIR to its `luau/` directory"
    );
}

/// The luau0-src version pinned in Cargo.toml; the registry directory is named after it.
const LUAU0_SRC_VERSION: &str = "0.22.0";

/// Compiles `csrc/codegen.cpp` against Luau's CodeGen headers and generates the Rust mirror of
/// `IrCmd`, so lowering hooks can be written in Rust against exactly this Luau's IR.
fn build_native_code_shim(luau_flags: &str) {
    let source = luau_source_dir();
    println!("cargo:rerun-if-changed=csrc/codegen.cpp");
    let mut build = cc::Build::new();
    build
        .cpp(true)
        .std("c++17")
        .warnings(false)
        .file("csrc/codegen.cpp")
        .include(source.join("CodeGen/include"))
        .include(source.join("VM/include"))
        .include(source.join("VM/src"))
        .include(source.join("Common/include"));
    for flag in luau_flags.split_whitespace() {
        build.flag(flag);
    }
    build.compile("dreambindercodegen");
    generate_ir_enums(&source);
}

/// Parses the `enum class` bodies the Rust IR layer mirrors and writes them to `OUT_DIR`.
fn generate_ir_enums(source: &Path) {
    let ir_data = std::fs::read_to_string(source.join("CodeGen/include/Luau/IrData.h")).expect("IrData.h");
    let options =
        std::fs::read_to_string(source.join("CodeGen/include/Luau/CodeGenOptions.h")).expect("CodeGenOptions.h");
    let mut out = String::new();
    out.push_str(&mirror_enum(&ir_data, "IrCmd", "u8", "IrCmd"));
    out.push_str(&mirror_enum(&ir_data, "IrCondition", "u8", "IrCondition"));
    out.push_str(&mirror_enum(&ir_data, "IrBlockKind", "u8", "IrBlockKind"));
    out.push_str(&mirror_enum(&options, "HostMetamethod", "i32", "HostMetamethod"));
    let path = PathBuf::from(env::var_os("OUT_DIR").unwrap()).join("ir_enums.rs");
    std::fs::write(&path, out).expect("write ir_enums.rs");
}

/// Mirrors `enum class <name> ... { A, B, ... }` as a Rust `#[repr]` enum with the same order
/// and values (implicit consecutive numbering; Luau's IR enums use no explicit values except a
/// trailing `Count`).
fn mirror_enum(header: &str, name: &str, repr: &str, rust_name: &str) -> String {
    let start = header.find(&format!("enum class {name}")).unwrap_or_else(|| panic!("{name} not found"));
    let body_start = header[start..].find('{').unwrap() + start + 1;
    let body_end = header[body_start..].find("};").unwrap() + body_start;
    let body = &header[body_start..body_end];
    let mut variants = Vec::new();
    for raw_line in body.lines() {
        let line = raw_line.split("//").next().unwrap().trim().trim_end_matches(',').trim();
        if line.is_empty() {
            continue;
        }
        assert!(!line.contains('='), "{name}::{line} has an explicit value; extend the mirror");
        variants.push(line.to_owned());
    }
    let mut out = format!(
        "/// Mirror of Luau's `Luau::CodeGen::{name}` for this exact Luau build, generated by build.rs.
         #[allow(non_camel_case_types, clippy::upper_case_acronyms)]
         #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[repr({repr})]
pub enum {rust_name} {{
"
    );
    for (index, variant) in variants.iter().enumerate() {
        out.push_str(&format!(
            "    {variant} = {index},
"
        ));
    }
    out.push_str(
        "}

",
    );
    out
}
