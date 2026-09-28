//! Builds Luau from the `luau/` git submodule (pinned at the 0.740 release, the commit `OpenMW`
//! pins), the binder's own C additions (`csrc/extra.cpp`), and, per feature, the C++ shims over
//! Luau's code generator and analysis libraries. Everything is compiled with `cc`, so the crate
//! builds wherever a C++17 compiler and Cargo exist (MSVC, clang, GCC, the Android NDK, cross
//! sysroots) with no network access at build time and nothing outside the package.

// The shared CI runs `-W clippy::pedantic -D warnings`.
#![allow(clippy::doc_markdown)]

use std::env;
use std::path::{Path, PathBuf};

// Max stack slots a C function may use. Luau's default; OpenMW builds with it too, and
// LUA_REGISTRYINDEX in src/raw/ffi.rs is derived from this same number.
const MAX_CSTACK: usize = 8000;

// Luau's default is 128 userdata tags. The binder uses every tag Luau can address: Udata::tag
// is a uint8_t and Luau keeps LUA_UTAG_LIMIT itself for LU_TAG_ITERATOR, so 254 is the ceiling
// and the value OpenMW builds with. l3i owns the Luau build, so it owns this ABI
// choice; src/lib.rs mirrors it as TAG_LIMIT. Hosts may append their own flags through
// LUAU_CXXFLAGS but cannot lower this one.
const TAG_LIMIT: u32 = 254;

// Three-component vectors, as OpenMW.
const VECTOR_SIZE: u32 = 3;

// The Luau release the `luau/` submodule is pinned at. Bumping the submodule means bumping this,
// re-auditing src/raw/ffi.rs against the new headers, and re-checking the flag policy.
const LUAU_VERSION: &str = "0.740";

struct Luau {
    root: PathBuf,
    base: cc::Build,
}

impl Luau {
    fn dir(&self, component: &str, part: &str) -> PathBuf {
        self.root.join(component).join(part)
    }

    /// One Luau component library: its own include and source directories plus `extra`
    /// include directories, compiled with the shared base configuration.
    fn library(&self, name: &str, component: &str, extra_includes: &[PathBuf], defines: &[(&str, &str)]) {
        let mut build = self.base.clone();
        build.include(self.dir(component, "include"));
        for include in extra_includes {
            build.include(include);
        }
        for (key, value) in defines {
            build.define(key, Some(*value));
        }
        for source in sources(&self.dir(component, "src")) {
            build.file(source);
        }
        build.compile(name);
    }
}

/// Every `.cpp` under `dir`, sorted for reproducible archives.
fn sources(dir: &Path) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = std::fs::read_dir(dir)
        .unwrap_or_else(|error| panic!("cannot read {}: {error}", dir.display()))
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "cpp"))
        .collect();
    files.sort();
    files
}

// A build script is one linear recipe; splitting it into functions would only scatter the order.
#[allow(clippy::too_many_lines)]
fn main() {
    println!("cargo:rerun-if-env-changed=LUAU_CXXFLAGS");
    println!("cargo:rerun-if-changed=luau");
    println!("cargo:rerun-if-changed=csrc");
    println!("cargo:rustc-env=L3I_TAG_LIMIT={TAG_LIMIT}");

    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("luau");
    assert!(
        root.join("VM/include/lua.h").is_file(),
        "the Luau submodule is not checked out; run `git submodule update --init` in {}",
        root.parent().unwrap().display()
    );
    println!("cargo:rustc-env=LUAU_VERSION={LUAU_VERSION}");

    let target = env::var("TARGET").unwrap_or_default();
    // Luau's internal assertions follow Rust's debug assertions, not the debuginfo setting: a
    // release build with symbols (what gets profiled) must not carry an asserting VM.
    let debug = env::var_os("CARGO_CFG_DEBUG_ASSERTIONS").is_some();
    let jit = env::var_os("CARGO_FEATURE_JIT").is_some();
    let analysis = env::var_os("CARGO_FEATURE_ANALYSIS").is_some();

    let mut base = cc::Build::new();
    base.cpp(true).std("c++17").warnings(false);
    toolchain_policy(&mut base);
    base.define("LUAI_MAXCSTACK", Some(MAX_CSTACK.to_string().as_str()));
    base.define("LUA_VECTOR_SIZE", Some(VECTOR_SIZE.to_string().as_str()));
    base.define("LUA_UTAG_LIMIT", Some(TAG_LIMIT.to_string().as_str()));
    base.define("LUA_API", Some("extern \"C\""));
    base.define("LUALIB_API", Some("extern \"C\""));
    if debug {
        // Luau's internal api_check assertions, as OpenMW's debug builds have them.
        base.define("LUAU_ENABLE_ASSERT", None);
    } else {
        // Lets the compiler lower sqrt() to one instruction.
        base.flag_if_supported("-fno-math-errno");
    }
    if target.ends_with("emscripten") {
        // cc adds -fno-exceptions for wasm32; Luau needs exceptions, in the ABI Rust uses.
        base.flag_if_supported("-fexceptions");
        base.flag_if_supported("-fwasm-exceptions");
    }
    if target.ends_with("-msvc") {
        // cc passes no /EH flag, and clang-cl refuses try and throw without one (cl only warns).
        // Luau raises errors as C++ exceptions, out of its extern "C" API too, so /EHs without
        // the c that would let the compiler assume extern "C" functions never throw.
        base.flag("/EHs");
    }
    if let Ok(extra) = env::var("LUAU_CXXFLAGS") {
        assert!(
            !extra.contains("LUA_UTAG_LIMIT"),
            "LUA_UTAG_LIMIT is fixed at {TAG_LIMIT} by l3i; remove it from LUAU_CXXFLAGS"
        );
        for flag in extra.split_whitespace() {
            base.flag(flag);
        }
    }
    base.include(root.join("Common/include"));

    let luau = Luau { root: root.clone(), base };
    let vm_include = luau.dir("VM", "include");
    let vm_src = luau.dir("VM", "src");
    let ast_include = luau.dir("Ast", "include");
    let bytecode_include = luau.dir("Bytecode", "include");
    let compiler_include = luau.dir("Compiler", "include");
    let config_include = luau.dir("Config", "include");
    let codegen_include = luau.dir("CodeGen", "include");

    luau.library("luaucommon", "Common", &[], &[]);
    luau.library("luauast", "Ast", &[], &[]);
    luau.library("luaubytecode", "Bytecode", &[], &[]);
    luau.library(
        "luauinliner",
        "Inliner",
        &[bytecode_include.clone(), luau.dir("Bytecode", "src"), vm_include.clone(), vm_src.clone()],
        &[("LUAJITINLINER_API", "extern \"C\"")],
    );
    luau.library(
        "luaucompiler",
        "Compiler",
        &[bytecode_include.clone(), ast_include.clone()],
        &[("LUACODE_API", "extern \"C\"")],
    );
    luau.library("luauconfig", "Config", &[ast_include.clone(), compiler_include.clone(), vm_include.clone()], &[]);
    luau.library(
        "luaurequire",
        "Require",
        &[ast_include.clone(), config_include.clone(), vm_include.clone()],
        &[("LUAREQUIRE_API", "extern \"C\"")],
    );
    luau.library("luauvm", "VM", &[], &[]);

    // The binder's own additions to the C API.
    let mut extra = luau.base.clone();
    extra.include(&vm_include).include(&vm_src).file("csrc/extra.cpp").compile("l3iextra");

    if jit {
        assert!(!target.ends_with("emscripten"), "native code generation (jit) is not supported on emscripten");
        luau.library(
            "luaucodegen",
            "CodeGen",
            &[vm_include.clone(), vm_src.clone()],
            &[("LUACODEGEN_API", "extern \"C\"")],
        );
        let mut shim = luau.base.clone();
        shim.include(&codegen_include).include(&vm_include).include(&vm_src).file("csrc/codegen.cpp");
        shim.compile("l3icodegen");
        generate_ir_enums(&codegen_include);
    }

    if analysis {
        // Analysis runs user-defined type functions on a Luau VM, so it sees the VM, bytecode,
        // and compiler headers too.
        luau.library(
            "luauanalysis",
            "Analysis",
            &[
                ast_include.clone(),
                config_include.clone(),
                compiler_include.clone(),
                bytecode_include.clone(),
                vm_include.clone(),
            ],
            &[],
        );
        let mut shim = luau.base.clone();
        shim.include(luau.dir("Analysis", "include"))
            .include(&ast_include)
            .include(&config_include)
            .include(&compiler_include)
            .include(&bytecode_include)
            .include(&vm_include)
            .file("csrc/analysis.cpp");
        shim.compile("l3ianalysis");
    }
}

/// Parses the `enum class` bodies the Rust IR layer mirrors and writes them to `OUT_DIR`.
/// Enforces the verified toolchain: clang for the C++ side, cross-language thin LTO, and an LLVM
/// major shared by clang and rustc. TOOLCHAIN.md holds the measurements: the binder's hot paths
/// run 8 to 15 percent faster than under GCC only when the Rust thunks and Luau's API inline into
/// each other, and the same configuration also has the shortest clean build. Plain clang is
/// slower than GCC at runtime, so the LTO half is not optional. `L3I_UNVERIFIED_TOOLCHAIN=1`
/// downgrades the refusal to a warning for hosts that cannot meet the requirement; docs.rs
/// (which sets `DOCS_RS`) gets the same treatment, since it only renders documentation and
/// cannot be given a linker configuration.
fn toolchain_policy(base: &mut cc::Build) {
    println!("cargo:rerun-if-env-changed=L3I_UNVERIFIED_TOOLCHAIN");
    println!("cargo:rerun-if-env-changed=DOCS_RS");
    println!("cargo:rerun-if-env-changed=CXX");
    println!("cargo:rerun-if-env-changed=CARGO_ENCODED_RUSTFLAGS");
    let compiler = base.get_compiler();
    let rustflags = env::var("CARGO_ENCODED_RUSTFLAGS").unwrap_or_default().replace('\u{1f}', " ");
    let plugin_lto = rustflags.contains("linker-plugin-lto");
    // The linker must consume LLVM bitcode: clang driving lld (GNU and Apple targets), or
    // lld-link itself (MSVC targets, where clang-cl compiles and rustc links with lld-link).
    let linker = rustflag_value(&rustflags, "linker")
        .and_then(|linker| Path::new(linker).file_stem())
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    let linker_is_clang = linker.starts_with("clang");
    let linker_is_lld_link = linker == "lld-link";
    let uses_lld = linker_is_lld_link || rustflags.contains("-fuse-ld=lld");

    let problem = if !compiler.is_like_clang() {
        Some(format!("the C++ compiler is `{}`, not clang", compiler.path().display()))
    } else if !plugin_lto {
        Some("RUSTFLAGS lacks -Clinker-plugin-lto".to_string())
    } else if !(linker_is_clang || linker_is_lld_link) {
        Some("RUSTFLAGS lacks -Clinker=clang (or -Clinker=lld-link on MSVC targets); the final link must consume LLVM bitcode".to_string())
    } else if !uses_lld {
        Some("RUSTFLAGS lacks -Clink-arg=-fuse-ld=lld (ld.bfd cannot consume the LTO bitcode)".to_string())
    } else {
        match (llvm_major_of_clang(&compiler), llvm_major_of_rustc()) {
            (Some(clang), Some(rustc)) if clang == rustc => None,
            (Some(clang), Some(rustc)) => Some(format!(
                "clang is LLVM {clang} but rustc is LLVM {rustc}; cross-language LTO needs the same major"
            )),
            (None, _) => Some(format!("`{} --version` did not report an LLVM version", compiler.path().display())),
            (_, None) => Some("`rustc -vV` did not report an LLVM version".to_string()),
        }
    };

    match problem {
        None => {
            // Bitcode objects, so the linker's LTO sees Luau and the Rust thunks as one module.
            base.flag("-flto=thin");
        }
        Some(problem) if env::var_os("DOCS_RS").is_some() => {
            println!(
                "cargo:warning=l3i is building for docs.rs with an unverified toolchain ({problem}); documentation only"
            );
        }
        Some(problem) if env::var_os("L3I_UNVERIFIED_TOOLCHAIN").is_some() => {
            println!(
                "cargo:warning=l3i is building with an unverified toolchain ({problem}); expect slower binder hot paths, see TOOLCHAIN.md"
            );
        }
        Some(problem) => panic!(
            "l3i builds only with the verified toolchain ({problem}).\n\
             Required: clang++ as the C++ compiler (CXX=clang++), lld, and RUSTFLAGS containing\n\
             `-Clinker-plugin-lto -Clinker=clang -Clink-arg=-fuse-ld=lld`, with clang and rustc on the\n\
             same LLVM major. This repository's .cargo/config.toml sets them; a dependent crate adds the\n\
             same lines to its own .cargo/config.toml. Set L3I_UNVERIFIED_TOOLCHAIN=1 to build anyway\n\
             with a warning. TOOLCHAIN.md has the measurements behind this policy."
        ),
    }
}

/// The value of `-C<name>=<value>` (or `-C <name>=<value>`) in space-separated rustflags.
/// Unrelated flags (`-Dwarnings`, `--cfg ...`) are skipped, not treated as the end.
fn rustflag_value<'a>(rustflags: &'a str, name: &str) -> Option<&'a str> {
    let mut words = rustflags.split_whitespace();
    while let Some(word) = words.next() {
        let option = if word == "-C" {
            match words.next() {
                Some(option) => option,
                None => break,
            }
        } else if let Some(option) = word.strip_prefix("-C") {
            option
        } else {
            continue;
        };
        if let Some(value) = option.strip_prefix(name).and_then(|rest| rest.strip_prefix('=')) {
            return Some(value);
        }
    }
    None
}

/// The LLVM major of the C++ compiler, from `clang++ --version`.
fn llvm_major_of_clang(compiler: &cc::Tool) -> Option<u32> {
    let output = compiler.to_command().arg("--version").output().ok()?;
    let text = String::from_utf8_lossy(&output.stdout);
    let after = text.split("clang version ").nth(1)?;
    after.split(|c: char| !c.is_ascii_digit()).next()?.parse().ok()
}

/// The LLVM major rustc was built against, from `rustc -vV`.
fn llvm_major_of_rustc() -> Option<u32> {
    let rustc = env::var_os("RUSTC").unwrap_or_else(|| "rustc".into());
    let output = std::process::Command::new(rustc).arg("-vV").output().ok()?;
    let text = String::from_utf8_lossy(&output.stdout);
    let after = text.split("LLVM version: ").nth(1)?;
    after.split('.').next()?.trim().parse().ok()
}

fn generate_ir_enums(codegen_include: &Path) {
    let ir_data = std::fs::read_to_string(codegen_include.join("Luau/IrData.h")).expect("IrData.h");
    let options = std::fs::read_to_string(codegen_include.join("Luau/CodeGenOptions.h")).expect("CodeGenOptions.h");
    let mut out = String::new();
    out.push_str(&mirror_enum(&ir_data, "IrCmd", "u8", "IrCmd"));
    out.push_str(&mirror_enum(&ir_data, "IrCondition", "u8", "IrCondition"));
    out.push_str(&mirror_enum(&ir_data, "IrBlockKind", "u8", "IrBlockKind"));
    out.push_str(&mirror_enum(&options, "HostMetamethod", "i32", "HostMetamethod"));
    let path = PathBuf::from(env::var_os("OUT_DIR").unwrap()).join("ir_enums.rs");
    std::fs::write(&path, out).expect("write ir_enums.rs");
}

/// Mirrors `enum class <name> ... { A, B, ... }` as a Rust `#[repr]` enum with the same order
/// and values (implicit consecutive numbering; Luau's IR enums use no explicit values).
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
        "/// Mirror of Luau's `Luau::CodeGen::{name}` for this exact Luau build, generated by build.rs.\n\
         #[allow(non_camel_case_types, clippy::upper_case_acronyms)]\n\
         #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]\n#[repr({repr})]\npub enum {rust_name} {{\n"
    );
    for (index, variant) in variants.iter().enumerate() {
        use std::fmt::Write;
        writeln!(out, "    {variant} = {index},").expect("writing to a String cannot fail");
    }
    out.push_str("}\n\n");
    out
}
