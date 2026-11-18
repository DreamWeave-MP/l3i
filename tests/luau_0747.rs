//! What moving to Luau 0.747 took away, pinned: the integer type does not parse, type check or
//! exist as a library, and bytecode with an integer constant, which Luau 0.746 wrote, does not load.

use l3i::Runtime;
use l3i::source::LoadScope;

fn fixture(name: &str) -> Vec<u8> {
    std::fs::read(std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/bytecode").join(name)).unwrap()
}

#[test]
fn integer_literals_and_the_library_are_gone() {
    let runtime = Runtime::new().unwrap();
    let error = runtime.exec("local x = 42i").unwrap_err().to_string();
    assert!(error.contains("Malformed number"), "{error}");
    runtime.exec("assert(integer == nil and buffer.readinteger == nil)").unwrap();
}

/// Bytecode compiled by the Luau 0.746 build, version 14 as 0.747 writes: the chunk with an integer
/// constant, which 0.747's loader would trap on or misread, is refused by l3i before Luau sees it;
/// one without loads and runs.
#[test]
fn bytecode_from_luau_0_746_with_an_integer_constant_is_refused() {
    let runtime = Runtime::new().unwrap();
    let stack = runtime.stack();
    let error = stack.load_bytecode("=old", &fixture("luau-0.746-integer-constant.luauc")).unwrap_err().to_string();
    assert_eq!(
        error,
        "old: bytecode holds a 64-bit integer constant, which Luau 0.747 removed; it was compiled by an older build and must be compiled again"
    );
    let plain = stack.load_bytecode("=old", &fixture("luau-0.746-plain.luauc")).unwrap();
    assert_eq!(plain.invoke::<f64, _>(&stack, ()).unwrap(), 6.0);
}

/// Under analysis `integer` is an unknown type.
#[cfg(feature = "analysis")]
#[test]
fn integer_is_an_unknown_type_to_the_checker() {
    use l3i::analysis::{Analysis, AnalysisOptions, Mode, ModuleConfig, SourceCode, SourceProvider};
    struct One;
    impl SourceProvider for One {
        fn read_source(&self, _: &str) -> Option<SourceCode> {
            Some(SourceCode { text: "--!strict\nlocal x: integer = 1\nprint(x)\n".to_owned(), is_script: true })
        }
        fn resolve_module(&self, _: &str, _: &str) -> Option<String> {
            None
        }
        fn module_config(&self, _: &str) -> ModuleConfig {
            ModuleConfig { mode: Mode::Strict, ..ModuleConfig::default() }
        }
        fn human_name(&self, name: &str) -> Option<String> {
            Some(name.to_owned())
        }
    }
    let analysis = Analysis::new(One, AnalysisOptions::default()).unwrap();
    let text: Vec<String> = analysis.check("script", false).diagnostics.iter().map(|d| d.text.clone()).collect();
    assert!(text.iter().any(|t| t.contains("Unknown type 'integer'")), "{text:?}");
}

/// The 3.0.0 documentation's own examples (`builtin-extensions.md`, `safety.md`, the home page):
/// what they claim about `Bits64` and a packed kind passed where another is expected.
#[test]
fn the_documented_examples_hold() {
    use l3i::extension::{RuntimePlan, RuntimePolicy};
    use l3i::quat::QuatExtension;
    let plan = RuntimePlan::builder()
        .policy(RuntimePolicy::new().compat_global("@dream/quat", "quat"))
        .extension(QuatExtension)
        .finalize()
        .unwrap();
    let runtime = Runtime::from_plan(&plan).unwrap();
    runtime
        .exec(
            r#"
            local bits64 = require("@dream/bits64")
            local id = bits64.fromHex("0x9e3779b97f4a7c15")
            assert(bits64.hex(id) == "9e3779b97f4a7c15" and typeof(id) == "Bits64")
            local hi, lo = bits64.parts(id)
            assert(bits64.fromParts(hi, lo) == id)
            local seen = { [id] = true }
            assert(seen[bits64.fromHex("9E3779B97F4A7C15")])
            assert(not pcall(bits64.toNumber, id))

            local small = bits64.fromNumber(42)
            assert(small == bits64.fromNumber(42) and small ~= 42)
            assert(typeof(small) == "Bits64")
            assert(not pcall(function() return small < bits64.fromNumber(43) end))
            "#,
        )
        .unwrap();
    let error = runtime
        .exec("local spin = quat.axisAngle(vector.create(0, 0, 1), 1) quat.mul(spin, quat.key(spin, 5))")
        .unwrap_err()
        .to_string();
    assert!(error.contains("expected a packed Quaternion, got kind 2"), "{error}");
}
