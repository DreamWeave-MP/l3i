//! The plan's generated `.d.luau` is checked by Luau's own frontend, and strict scripts that
//! `require` every built-in module by its canonical path type check against the plan's stubs:
//! the declared API and the runtime agree, with no compatibility global in sight.

use std::collections::HashMap;
use std::rc::Rc;

use l3i::analysis::{Analysis, AnalysisOptions, Definitions, Mode, ModuleConfig, SourceCode, SourceProvider};
use l3i::extension::{RuntimePlan, RuntimePolicy};
use l3i::quat::QuatExtension;
use l3i::raster::RasterExtension;
use l3i::soft_render::SoftRenderExtension;

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

fn plan() -> Rc<RuntimePlan> {
    RuntimePlan::builder()
        .policy(RuntimePolicy::new())
        .extension(QuatExtension)
        .extension(RasterExtension)
        .extension(SoftRenderExtension)
        .finalize()
        .unwrap()
}

const SCRIPTS: &[(&str, &str)] = &[
    (
        "quat_script",
        "--!strict\n\
         local quat = require('@dream/quat')\n\
         local q: integer = quat.axisAngle(vector.create(0, 0, 1), 1.0)\n\
         local r: vector = quat.rotate(q, vector.create(1, 0, 0))\n\
         local m: integer = quat.mul(q, quat.IDENTITY)\n\
         local s: integer = quat.slerp(q, quat.inverse(m), 0.5)\n\
         local x, y, z, w = quat.toXYZW(s)\n\
         local angle: number = quat.angleTo(q, m) + x + y + z + w\n\
         local k: integer = quat.key(q, 3)\n\
         local back: integer = quat.keyRotation(k)\n\
         local flags: number = quat.keyFlags(k)\n\
         local math = quat.math()\n\
         local lowered: integer = math:mul(back, math:slerp(q, m, 0.25))\n\
         print(r, angle, flags, lowered)\n",
    ),
    (
        "raster_script",
        "--!strict\n\
         local raster = require('@dream/raster')\n\
         local c: integer = raster.rgba8(1, 2, 3, 4)\n\
         local d: integer = raster.lerp(c, raster.WHITE, 0.5)\n\
         local r, g, b, a = raster.channels(d)\n\
         local sum: number = r + g + b + a + raster.packed(c)\n\
         local m = raster.math()\n\
         local e: integer = m:add(m:mul(c, d), m:scale(raster.BLACK, 0.5))\n\
         local wide: integer = raster.widen(e)\n\
         local narrow: integer = m:narrow(m:lerp16(wide, raster.WHITE16, 0.5))\n\
         local clip: integer = raster.clip(0, 0, 10, 10)\n\
         local minX, minY, maxX, maxY = raster.clipBounds(raster.CLIP_ALL)\n\
         print(sum, narrow, clip, minX + minY + maxX + maxY)\n",
    ),
    (
        "soft_script",
        "--!strict\n\
         local raster = require('@dream/raster')\n\
         local soft = require('@dream/soft-render')\n\
         local renderer = soft.renderer()\n\
         local frame = renderer:beginFrame(64, 48)\n\
         frame:clear(raster.BLACK)\n\
         frame:rect(vector.create(0, 0, 0), vector.create(8, 8, 0), raster.WHITE, raster.CLIP_ALL)\n\
         local texture = renderer:createTexture(2, 2, buffer.create(16))\n\
         frame:image(vector.create(0, 0, 0), vector.create(2, 2, 0), vector.create(0, 0, 0), vector.create(1, 1, 0), texture, raster.WHITE, raster.CLIP_ALL)\n\
         local vertices = soft.vertices()\n\
         local mesh = buffer.create(soft.VERTEX_BYTES * 3)\n\
         local next: number = vertices:write(mesh, 0, vector.create(0, 0, 0), vector.create(0, 0, 0), soft.premultiply(raster.WHITE))\n\
         frame:mesh(mesh, buffer.create(12), texture, raster.CLIP_ALL)\n\
         frame:finish()\n\
         local width: number = renderer.width + frame.width + texture.width\n\
         texture:free()\n\
         print(next, width)\n",
    ),
    (
        "net_script",
        "--!strict\n\
         local net = require('@dream/net')\n\
         local schema = net.schema({ version = 1, channels = { { name = 'state', delivery = 'unreliable' } }, events = { { name = 'ping', channel = 'state', maxPayload = 8 } } })\n\
         local id: integer = schema:eventId('ping') or 0i\n\
         local name: string = schema:eventName(id) or ''\n\
         local count: number = schema.eventCount\n\
         local client = net.client({ schema = schema })\n\
         client:update()\n\
         local kind, peer, a = client:pollInto(buffer.create(64))\n\
         client:sendEvent(id, buffer.create(8), 0, 8)\n\
         local rtt: number? = client.rtt\n\
         print(name, count, kind, peer, a, rtt, client.status, net.MAX_CHANNELS)\n",
    ),
];

/// The plan's definitions parse and type check in Luau's own frontend, and a strict script
/// against every built-in module has no diagnostics: the declared API and the runtime agree.
#[test]
fn the_generated_definitions_type_check_and_typed_scripts_pass_strict_mode() {
    let plan = plan();
    let definitions = plan.type_definitions();
    assert!(definitions.contains("declare extern type dream_quat_Math with"), "{definitions}");
    assert!(definitions.contains("declare extern type dream_net_Client with"), "{definitions}");
    assert!(definitions.contains("    IDENTITY: integer,"), "{definitions}");
    for fallback in ["(self, ...any): any", "(...any) -> ...any", ": any,\n", ": any\n"] {
        assert!(!definitions.contains(fallback), "every built-in member is typed ({fallback:?} found):\n{definitions}");
    }
    assert!(!definitions.contains("declare quat"), "no compatibility global is declared:\n{definitions}");
    let scripts = plan.analysis_sources(Scripts(SCRIPTS.iter().copied().collect()));
    let options =
        AnalysisOptions { definitions: vec![Definitions { name: "dream.d.luau".to_owned(), source: definitions.clone() }], ..Default::default() };
    let analysis = match Analysis::new(scripts, options) {
        Ok(analysis) => analysis,
        Err(error) => panic!("{error}\n---\n{definitions}"),
    };
    for (name, _) in SCRIPTS {
        let report = analysis.check(name, false);
        let text: Vec<String> = report
            .diagnostics
            .iter()
            .map(|d| format!("{name}:{}:{}: {} ({:?})", d.span.begin_line + 1, d.span.begin_column + 1, d.text, d.kind))
            .collect();
        assert!(report.is_clean(), "{}\n---\n{definitions}", text.join("\n"));
    }
    // The reusable gate every extension crate runs: the same proof, from the plan alone.
    plan.check_definitions().unwrap();
    // A signature that is not Luau, or that names a type that does not exist, fails that gate
    // with the frontend's diagnostics rather than finalizing quietly.
    struct Broken(&'static str);
    impl l3i::extension::Extension for Broken {
        fn id(&self) -> &'static str {
            "dream.broken"
        }
        fn describe(&self, d: &mut l3i::extension::ExtensionDescriptor) -> l3i::Result<()> {
            d.module("@dream/broken").function("f", || 1i64).signature(self.0);
            Ok(())
        }
    }
    let broken = RuntimePlan::builder().extension(Broken("(foo: Completely Broken Syntax")).finalize().unwrap();
    let error = broken.check_definitions().unwrap_err().to_string();
    assert!(error.contains("declared types do not check") && error.contains("@dream/broken"), "{error}");
    let missing = RuntimePlan::builder().extension(Broken("() -> NoSuchType")).finalize().unwrap();
    let error = missing.check_definitions().unwrap_err().to_string();
    assert!(error.contains("NoSuchType"), "{error}");
    // The definitions are also what a runtime built from the plan reports.
    let runtime = l3i::Runtime::from_plan(&plan).unwrap();
    assert_eq!(runtime.type_definitions().as_deref(), Some(definitions.as_str()));
    // A definitions file with an error is refused with its diagnostics, not silently ignored.
    let broken = AnalysisOptions {
        definitions: vec![Definitions { name: "broken.d.luau".to_owned(), source: "declare oops: Nope\n".to_owned() }],
        ..Default::default()
    };
    let error = Analysis::new(Scripts(HashMap::new()), broken).err().unwrap().to_string();
    assert!(error.contains("broken.d.luau:1") && error.contains("Nope"), "{error}");
}
