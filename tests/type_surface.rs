//! The packed kinds as types: each is a nominal type in the definitions, so a strict script cannot
//! hand one kind to another's parameter; the bytecode compiler erases their names to `any`, so a
//! native function annotated with one keeps its lowering instead of refusing its own arguments at
//! the entry guard; and the constants are module values the compiler resolves once.

#![allow(clippy::cast_possible_truncation)]

use std::rc::Rc;

use l3i::Runtime;
use l3i::extension::{RuntimePlan, RuntimePolicy};
use l3i::quat::QuatExtension;
use l3i::raster::RasterExtension;

fn plan(policy: RuntimePolicy) -> Rc<RuntimePlan> {
    RuntimePlan::builder()
        .policy(
            policy
                .compat_global("@dream/quat", "quat")
                .compat_global("@dream/raster", "raster")
                .compat_global("@dream/bits64", "bits64"),
        )
        .extension(QuatExtension)
        .extension(RasterExtension)
        .finalize()
        .unwrap()
}

#[test]
fn constants_are_values_of_their_kind() {
    let runtime = Runtime::from_plan(&plan(RuntimePolicy::new())).unwrap();
    runtime
        .exec(
            "assert(quat.IDENTITY == quat.fromXYZW(0, 0, 0, 1), 'identity') \
             assert(raster.WHITE == raster.rgba8(255, 255, 255, 255), 'white') \
             assert(raster.WHITE16 == raster.widen(raster.WHITE), 'white16') \
             assert(raster.CLIP_ALL == raster.clip(0, 0, raster.CLIP_MAX_COORD, raster.CLIP_MAX_COORD), 'clip all')",
        )
        .unwrap();
    // Under a safe environment a use of the constant is an import, resolved once at load: inside
    // a loop it costs what a folded constant did.
    let options = runtime.compile_options();
    let text = l3i::source::disassemble("local s for i = 1, 10 do s = quat.IDENTITY end return s", &options).unwrap();
    assert!(text.contains("GETIMPORT") && text.contains("quat.IDENTITY"), "{text}");
    assert!(!text.contains("GETTABLEKS"), "{text}");
}

/// A plan with a host userdata class or a type alias named like a packed kind does not finalize:
/// compiled code would erase the name.
#[test]
fn a_plan_refuses_a_type_named_like_a_packed_kind() {
    use l3i::extension::{Extension, ExtensionDescriptor};
    struct Alias;
    impl Extension for Alias {
        fn id(&self) -> &'static str {
            "test.alias"
        }
        fn describe(&self, d: &mut ExtensionDescriptor) -> l3i::error::Result<()> {
            d.type_alias("Quaternion", "{ x: number }");
            Ok(())
        }
    }
    let error = RuntimePlan::builder().extension(Alias).finalize().unwrap_err().to_string();
    assert!(error.contains("Quaternion") && error.contains("packed scalar"), "{error}");
}

/// In a strict script one kind does not stand in for another.
#[cfg(feature = "analysis")]
#[test]
fn a_strict_script_cannot_confuse_two_kinds() {
    use std::collections::HashMap;

    use l3i::analysis::{Analysis, AnalysisOptions, Definitions, Mode, ModuleConfig, SourceCode, SourceProvider};

    struct Scripts(HashMap<&'static str, &'static str>);
    impl SourceProvider for Scripts {
        fn read_source(&self, name: &str) -> Option<SourceCode> {
            self.0.get(name).map(|text| SourceCode { text: (*text).to_owned(), is_script: true })
        }
        fn resolve_module(&self, _requirer: &str, _required: &str) -> Option<String> {
            None
        }
        fn module_config(&self, _name: &str) -> ModuleConfig {
            ModuleConfig { mode: Mode::Strict, ..ModuleConfig::default() }
        }
        fn human_name(&self, name: &str) -> Option<String> {
            Some(format!("{name}.luau"))
        }
    }

    let plan = plan(RuntimePolicy::new());
    let scripts = [
        (
            "good",
            "--!strict\nlocal raster = require('@dream/raster')\nlocal c: Color = raster.rgba8(1, 2, 3, 4)\nprint(raster.red(c))\n",
        ),
        (
            "color_as_rotation",
            "--!strict\nlocal quat = require('@dream/quat')\nlocal raster = require('@dream/raster')\nprint(quat.rotate(raster.WHITE, vector.create(1, 0, 0)))\n",
        ),
        (
            "wide_as_color",
            "--!strict\nlocal raster = require('@dream/raster')\nlocal c: Color = raster.WHITE16\nprint(c)\n",
        ),
        ("number_as_clip", "--!strict\nlocal raster = require('@dream/raster')\nprint(raster.clipBounds(5))\n"),
    ];
    let sources = plan.analysis_sources(Scripts(scripts.iter().copied().collect()));
    let options = AnalysisOptions {
        definitions: vec![Definitions { name: "dream.d.luau".to_owned(), source: plan.type_definitions() }],
        ..Default::default()
    };
    let analysis = Analysis::new(sources, options).unwrap();
    let text = |name: &str| -> String {
        analysis.check(name, false).diagnostics.iter().map(|d| d.text.clone()).collect::<Vec<_>>().join("\n")
    };
    assert!(analysis.check("good", false).is_clean(), "{}", text("good"));
    let mixed = text("color_as_rotation");
    assert!(mixed.contains("Color") && mixed.contains("Quaternion"), "{mixed}");
    let wide = text("wide_as_color");
    assert!(wide.contains("Color16") && wide.contains("Color"), "{wide}");
    assert!(!analysis.check("number_as_clip", false).is_clean(), "a number is not a ClipRect");
}

/// A native function whose parameters are annotated with packed kinds keeps its lowering: over a
/// thousand calls no entry guard and no operation guard exits. Compiled with the erasure off, the
/// same function takes an exit on every call, which is what the erasure exists to prevent. A local
/// alias that shadows a kind's name keeps its own type.
#[cfg(feature = "jit")]
#[test]
fn annotated_native_functions_stay_native() {
    use l3i::extension::NativeCodePolicy;
    use l3i::native_code::NativeCodeMode;
    use l3i::source::LoadScope;

    let policy = RuntimePolicy::new().native_code(NativeCodePolicy {
        mode: NativeCodeMode::Annotated,
        record_counters: true,
        ..NativeCodePolicy::default()
    });
    let runtime = Runtime::from_plan(&plan(policy)).unwrap();
    let generator = runtime.native_code().expect("native code");
    if !generator.is_available() {
        eprintln!("no Luau code generator on this platform; skipping");
        return;
    }
    let source = "--!native\n\
                  local Q: dream_quat_Math = quat.math()\n\
                  local C: dream_raster_Math = raster.math()\n\
                  local function turn(q: Quaternion, v: vector, c: Color?): number\n\
                      local w: Color = c or raster.WHITE\n\
                      local r: Quaternion = Q:mul(q, q)\n\
                      local k: AnimationKey = Q:key(r, 3)\n\
                      local p: vector = Q:rotate(Q:keyRotation(k), v)\n\
                      local x: number = C:red(w) + Q:keyFlags(k)\n\
                      return x\n\
                  end\n\
                  type Color16 = { r: number }\n\
                  local function shadowed(c: Color16): number return 1 end\n\
                  return turn, shadowed\n";
    let exits = |options: &l3i::source::CompileOptions, call: &str| -> u64 {
        let (turn, shadowed): (l3i::value::Function, l3i::value::Function) = {
            let stack = runtime.stack();
            let chunk = stack.load_source("=annotated", source, options).unwrap();
            chunk.invoke(&stack, ()).unwrap()
        };
        runtime.set_global("turn", &turn).unwrap();
        runtime.set_global("shadowed", &shadowed).unwrap();
        let before = generator.execution_stats(&runtime.stack()).vm_exits_taken;
        runtime.exec(call).unwrap();
        generator.execution_stats(&runtime.stack()).vm_exits_taken - before
    };
    let options = runtime.compile_options();
    let calls = "local q = quat.axisAngle(vector.create(0, 0, 1), 0.3) \
                 for i = 1, 1000 do turn(q, vector.create(i, 0, 0), raster.BLACK) end";
    assert_eq!(exits(&options, calls), 0, "an annotated native function left native code");
    let mut unerased = options.clone();
    unerased.packed_types.clear();
    assert!(
        exits(&unerased, calls) >= 1000,
        "without the erasure the entry guard should refuse the annotated packed arguments"
    );
    // The alias is a table type: a number fails its entry guard, so the name was left alone.
    assert!(exits(&options, "for i = 1, 100 do shadowed(5) end") >= 100, "the local alias lost its type");
}

/// Erasure renames, it never moves: a runtime error inside an annotated function reports its line.
#[test]
fn erasure_keeps_line_numbers() {
    let runtime = Runtime::from_plan(&plan(RuntimePolicy::new())).unwrap();
    let error = runtime
        .exec(
            "local function f(q: Quaternion,\n\
                              c: Color): Quaternion\n\
                 local x: ClipRect? = nil\n\
                 error('line four')\n\
             end\n\
             f(quat.IDENTITY, raster.WHITE)",
        )
        .unwrap_err()
        .to_string();
    assert!(error.contains(":4:") && error.contains("line four"), "{error}");
}

/// #30's lint was to catch packed parameters the erasure cannot reach. Luau's compiler gives a
/// guard only to a bare name it does not know (`Compiler/src/Types.cpp`): a prefixed name and a
/// generic parameter are `any`, an alias is its body, and the erasure renames every bare packed
/// name, inside aliases too. Every other form a script can write is held here at zero exits over a
/// thousand native calls, so the only reference left with a guard is a kind missing from
/// `CompileOptions::packed_types`, which is a list the host passes, not something a lint over the
/// source could see.
#[cfg(feature = "jit")]
#[test]
fn every_annotation_form_the_erasure_leaves_stays_native() {
    use l3i::extension::NativeCodePolicy;
    use l3i::native_code::NativeCodeMode;
    use l3i::source::LoadScope;

    let policy = RuntimePolicy::new().native_code(NativeCodePolicy {
        mode: NativeCodeMode::Annotated,
        record_counters: true,
        ..NativeCodePolicy::default()
    });
    let runtime = Runtime::from_plan(&plan(policy)).unwrap();
    let generator = runtime.native_code().expect("native code");
    if !generator.is_available() {
        eprintln!("no Luau code generator on this platform; skipping");
        return;
    }
    // (form, script, whether the compiler guards it when nothing is erased)
    let forms = [
        ("an alias of a kind", "type Rot = Quaternion\nlocal function f(q: Rot): number return 1 end", true),
        ("an alias of an optional kind", "type Rot = Quaternion?\nlocal function f(q: Rot): number return 1 end", true),
        ("an alias chain", "type A = Quaternion\ntype B = A\nlocal function f(q: B): number return 1 end", true),
        ("a generic alias", "type Box<T> = T\nlocal function f(q: Box<Quaternion>): number return 1 end", false),
        ("a generic parameter", "local function f<T>(q: T): number return 1 end", false),
        ("a union", "local function f(q: Quaternion | number): number return 1 end", false),
        ("an intersection", "local function f(q: Quaternion & Quaternion): number return 1 end", false),
        ("an optional", "local function f(q: Quaternion?): number return 1 end", true),
        ("a parenthesised kind", "local function f(q: (Quaternion)): number return 1 end", true),
        ("typeof a packed value", "local function f(q: typeof(quat.IDENTITY)): number return 1 end", false),
        ("a wide kind", "local function f(q: Bits64): number return 1 end", true),
        ("an @native function", "@native local function f(q: Quaternion): number return 1 end", true),
    ];
    let options = runtime.compile_options();
    let mut unerased = options.clone();
    unerased.packed_types.clear();
    let exits = |name: &str, body: &str, options: &l3i::source::CompileOptions| -> u64 {
        let source = format!("--!native\n{body}\nreturn f\n");
        let f: l3i::value::Function = {
            let stack = runtime.stack();
            let chunk = stack.load_source("=form", &source, options).unwrap();
            chunk.invoke(&stack, ()).unwrap()
        };
        runtime.set_global("f", &f).unwrap();
        let value = if name == "a wide kind" { "bits64.fromNumber(7)" } else { "quat.IDENTITY" };
        let before = generator.execution_stats(&runtime.stack()).vm_exits_taken;
        runtime.exec(&format!("local v = {value} for i = 1, 1000 do f(v) end")).unwrap();
        generator.execution_stats(&runtime.stack()).vm_exits_taken - before
    };
    for (name, body, guarded) in forms {
        let erased = exits(name, body, &options);
        assert_eq!(erased, 0, "{name}: {erased} exits over 1000 calls");
        // The control: without the erasure, the forms the compiler guards exit on every call, so
        // the zero above is the erasure at work and not a call that never reached native code.
        // The others are `any` to the compiler whatever the erasure does.
        let control = exits(name, body, &unerased);
        if guarded {
            assert!(control >= 1000, "{name}: only {control} exits without the erasure");
        } else {
            assert_eq!(control, 0, "{name}: {control} exits without the erasure");
        }
    }
}
