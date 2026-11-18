//! The differential gate of the carrier change (gate B and E): every lowered and bound packed
//! operation over a fixed corpus, its results written down carrier-independently (a packed or
//! wide result as its 64 bits, a number or vector as its bits, an error as its text) and compared
//! line for line with `tests/support/packed/corpus.golden`, which the Luau 0.746 integer carrier
//! wrote before it was deleted. The light carrier must reproduce it on both paths, the boxed
//! carrier on the bound path (it has no native code). A changed line is a changed result.
//!
//! The golden was written by the integer carrier at c280c32 and is never rewritten: there is no
//! carrier left that could stand in for the oracle.

#![allow(clippy::too_many_lines)]

use std::fmt::Write as _;

use l3i::Runtime;
use l3i::convert::Bits64;
use l3i::extension::{RuntimePlan, RuntimePolicy};
use l3i::quat::QuatExtension;
use l3i::raster::RasterExtension;
use l3i::stack::Scope;
use l3i::value::Value;

const CARRIER: &str = l3i::packed::carrier::CARRIER;

/// `(receiver, method, argument count, result count)`: every operation with a lowering.
const OPS: &[(&str, &str, usize, usize)] = &[
    ("Q", "rotate", 2, 1),
    ("Q", "mul", 2, 1),
    ("Q", "slerp", 3, 1),
    ("Q", "fromXYZW", 4, 1),
    ("Q", "key", 2, 1),
    ("Q", "keyRotation", 1, 1),
    ("Q", "keyFlags", 1, 1),
    ("C", "rgba8", 4, 1),
    ("C", "rgb8", 3, 1),
    ("C", "red", 1, 1),
    ("C", "alpha", 1, 1),
    ("C", "channels", 1, 4),
    ("C", "withAlpha", 2, 1),
    ("C", "lerp", 3, 1),
    ("C", "mul", 2, 1),
    ("C", "add", 2, 1),
    ("C", "scale", 2, 1),
    ("C", "premultiply", 1, 1),
    ("C", "widen", 1, 1),
    ("C", "narrow", 1, 1),
    ("C", "rgba16", 4, 1),
    ("C", "rgb16", 3, 1),
    ("C", "red16", 1, 1),
    ("C", "channels16", 1, 4),
    ("C", "withAlpha16", 2, 1),
    ("C", "lerp16", 3, 1),
    ("C", "mul16", 2, 1),
    ("C", "add16", 2, 1),
    ("C", "scale16", 2, 1),
    ("C", "premultiply16", 1, 1),
];

/// The corpus, by script: a fixed linear congruential sequence, so every carrier sees the same
/// inputs. `args(op, i)` builds case `i` of `op`; rotations, keys, colors and wide colors come
/// from the real constructors (the binder's), so every input is canonical.
const CORPUS: &str = r"
local seed = 12345
local function rand()
  seed = (seed * 1103515245 + 12345) % 2147483648
  return seed / 2147483648
end
-- Components from basic arithmetic alone: fromXYZW normalizes with sqrt, which every platform
-- rounds the same, where sin and cos differ from one libm to the next.
local function rotation()
  return quat.fromXYZW(rand() * 2 - 1, rand() * 2 - 1, rand() * 2 - 1, rand() * 2 - 0.9)
end
local function channel() return math.floor(rand() * 256) end
local function color() return raster.rgba8(channel(), channel(), channel(), channel()) end
local function wide() return raster.rgba16(rand() * 65536, rand() * 65536, rand() * 65536, rand() * 65536) end
local function weight() local w = rand() * 1.4 - 0.2 if rand() < 0.1 then return 0 elseif rand() < 0.1 then return 1 end return w end
local boundaries = {
  quat.IDENTITY, quat.fromXYZW(1, 0, 0, 0), quat.fromXYZW(0, 1, 0, 0), quat.fromXYZW(0, 0, 1, 0),
  quat.fromXYZW(1, 1, 1, 1), quat.fromXYZW(1, 1, 0, 0), quat.fromXYZW(0, -1, 0, 1), quat.fromXYZW(0, 0, 0, -1),
  quat.fromXYZW(1e-3, 0, 0, 1),
}
local colors = { raster.WHITE, raster.BLACK, raster.TRANSPARENT, raster.rgba8(255, 0, 0, 1), raster.rgba8(1, 2, 3, 255) }
local wides = { raster.WHITE16, raster.BLACK16, raster.TRANSPARENT16, raster.widen(raster.rgba8(128, 64, 32, 16)) }
local function pick(list, i, make) if i <= #list then return list[i] end return make() end
return function(full, i)
  local receiver, op = string.match(full, '^(%a):(.+)$')
  if receiver == 'Q' then
  if op == 'rotate' then return pick(boundaries, i, rotation), vector.create(rand() * 4 - 2, rand() * 4 - 2, rand() * 4 - 2) end
  if op == 'mul' then return pick(boundaries, i, rotation), rotation() end
  if op == 'slerp' then return pick(boundaries, i, rotation), rotation(), weight() end
  if op == 'fromXYZW' then return rand() * 4 - 2, rand() * 4 - 2, rand() * 4 - 2, rand() * 4 - 2 end
  if op == 'key' then return pick(boundaries, i, rotation), math.floor(rand() * 40) end
  if op == 'keyRotation' or op == 'keyFlags' then return quat.key(pick(boundaries, i, rotation), i % 16) end
  end
  if op == 'rgba8' or op == 'rgba16' then return rand() * 300 - 20, rand() * 300 - 20, rand() * 70000, rand() * 256 end
  if op == 'rgb8' or op == 'rgb16' then return rand() * 300 - 20, rand() * 256, rand() * 70000 end
  if op == 'withAlpha' then return pick(colors, i, color), rand() * 300 - 20 end
  if op == 'withAlpha16' then return pick(wides, i, wide), rand() * 70000 - 100 end
  if op == 'lerp' then return pick(colors, i, color), color(), weight() end
  if op == 'lerp16' then return pick(wides, i, wide), wide(), weight() end
  if op == 'mul' or op == 'add' then return pick(colors, i, color), color() end
  if op == 'mul16' or op == 'add16' then return pick(wides, i, wide), wide() end
  if op == 'scale' then return pick(colors, i, color), rand() * 3 - 0.5 end
  if op == 'scale16' then return pick(wides, i, wide), rand() * 3 - 0.5 end
  if string.sub(op, -2) == '16' or op == 'narrow' then return pick(wides, i, wide) end
  return pick(colors, i, color)
end
";

/// Cases per operation.
const CASES: usize = 40;

/// One result as a carrier-independent line: a packed or wide value as its 64 bits (`Bits64`
/// reads any value on the carrier), a number as its bits, a vector as its three lanes' bits.
fn describe(value: &Value, scope: &impl Scope) -> String {
    scope
        .with_frame(|frame| {
            let view = value.push_to(frame)?;
            if view.is_nil() {
                return Ok("nil".to_owned());
            }
            if view.is_number() {
                return Ok(format!("number {:#018x}", view.read::<f64>()?.to_bits()));
            }
            if view.is_vector() {
                let v = view.read::<l3i::convert::Vector3>()?;
                return Ok(format!("vector {:#010x} {:#010x} {:#010x}", v.x.to_bits(), v.y.to_bits(), v.z.to_bits()));
            }
            if view.is_string() {
                return Ok(format!("error {:?}", String::from_utf8_lossy(&view.read::<Vec<u8>>()?)));
            }
            Ok(format!("packed {:#018x}", view.read::<Bits64>()?.0))
        })
        .unwrap()
}

fn plan(native: bool) -> std::rc::Rc<RuntimePlan> {
    let policy = RuntimePolicy::new().compat_global("@dream/quat", "quat").compat_global("@dream/raster", "raster");
    #[cfg(feature = "jit")]
    let policy = if native {
        policy.native_code(l3i::extension::NativeCodePolicy {
            mode: l3i::native_code::NativeCodeMode::Annotated,
            ..l3i::extension::NativeCodePolicy::default()
        })
    } else {
        policy
    };
    let _ = native;
    RuntimePlan::builder().policy(policy).extension(QuatExtension).extension(RasterExtension).finalize().unwrap()
}

/// Every operation's results over the corpus on `path` (`bound` everywhere, `lowered` with jit).
fn run(path: &str) -> String {
    let runtime = Runtime::from_plan(&plan(path == "lowered")).unwrap();
    let mut wrappers = String::from(if path == "lowered" {
        "--!native\nlocal Q: dream_quat_Math = quat.math()\nlocal C: dream_raster_Math = raster.math()\nlocal ops = {}\n"
    } else {
        "local ops = {}\n"
    });
    for (index, (receiver, method, args, results)) in OPS.iter().enumerate() {
        let params: Vec<String> = (1..=*args).map(|i| format!("a{i}")).collect();
        let outs: Vec<String> = (1..=*results).map(|i| format!("r{i}")).collect();
        let (params, outs) = (params.join(", "), outs.join(", "));
        // The bound path is the module function, the same Rust code as the receiver's method
        // (A0 holds the method itself to its lowering), and present without jit too.
        let module = if *receiver == "Q" { "quat" } else { "raster" };
        let call = if path == "lowered" {
            format!("{receiver}:{method}({params})")
        } else {
            format!("{module}.{method}({params})")
        };
        writeln!(wrappers, "ops[{}] = function({params}) local {outs} = {call} return {outs} end", index + 1).unwrap();
    }
    wrappers.push_str("return ops\n");
    let ops: Value = {
        use l3i::source::LoadScope;
        let stack = runtime.stack();
        let chunk = stack.load_source("=corpus", &wrappers, &runtime.compile_options()).unwrap();
        chunk.invoke::<Value, _>(&stack, ()).unwrap()
    };
    let args = runtime.load_function(&format!("return function() {CORPUS} end")).unwrap();
    let args: l3i::value::Function = args.invoke(&runtime.stack(), ()).unwrap();
    let drive = runtime
        .load_function(
            "return function(ops, args) return function(index, name, i) \
             return pcall(ops[index], args(name, i)) end end",
        )
        .unwrap();
    let drive: l3i::value::Function = drive.invoke(&runtime.stack(), (&ops, &args)).unwrap();
    let mut out = String::new();
    for (index, (receiver, method, _, _)) in OPS.iter().enumerate() {
        for case in 1..=CASES {
            let stack = runtime.stack();
            let (ok, a, b, c, d): (bool, Value, Value, Value, Value) =
                drive.invoke(&stack, (index as f64 + 1.0, format!("{receiver}:{method}"), case as f64)).unwrap();
            let mut line = format!("{receiver}:{method} {case} {}", if ok { "ok" } else { "error" });
            for value in [&a, &b, &c, &d] {
                line.push_str(" | ");
                line.push_str(&describe(value, &stack));
            }
            writeln!(out, "{line}").unwrap();
        }
    }
    out
}

fn golden_path() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/support/packed/corpus.golden")
}

/// The golden is one file: the bound results, then the lowered ones, each under a header.
fn compare(path: &str, actual: &str) {
    let golden = std::fs::read_to_string(golden_path()).unwrap().replace("\r\n", "\n");
    let section = golden
        .split(&format!("== {path}\n"))
        .nth(1)
        .and_then(|rest| rest.split("\n== ").next())
        .unwrap_or_else(|| panic!("the golden has no {path} section"));
    // slerp runs acos and sin, which differ from one libm to the next: its results are compared on
    // the platform that wrote the golden only (A0 holds its lowering to its binder everywhere).
    let portable =
        |line: &&str| cfg!(all(target_os = "linux", target_arch = "x86_64")) || !line.starts_with("Q:slerp ");
    let expected: Vec<&str> = section.trim_end().lines().filter(portable).collect();
    let actual: Vec<&str> = actual.trim_end().lines().filter(portable).collect();
    assert_eq!(expected.len(), actual.len(), "{path}: the corpus changed size");
    let differences: Vec<String> = expected
        .iter()
        .zip(&actual)
        .filter(|(e, a)| e != a)
        .map(|(e, a)| format!("golden  {e}\n{CARRIER:7} {a}"))
        .collect();
    assert!(
        differences.is_empty(),
        "{path}: {} results differ from the integer carrier's:\n{}",
        differences.len(),
        differences.join("\n")
    );
}

#[test]
fn every_carrier_reproduces_the_integer_carriers_corpus() {
    let bound = run("bound");
    #[cfg(feature = "jit")]
    let lowered = run("lowered");
    compare("bound", &bound);
    #[cfg(feature = "jit")]
    compare("lowered", &lowered);
}
