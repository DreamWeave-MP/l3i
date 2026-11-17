//! The first gate of the carrier change (gate A0): for every lowered operation, every operand case
//! is run through the lowered call and through the bound method in one runtime, and the two must
//! agree on accepting or refusing it, on every result's type, 64 bits and light tag, and on the
//! error text. A real value of the right kind must stay native (no VM exit): correct results at
//! interpreter speed are a regression too.
//!
//! The cases: good values from real operations and forged with every flag value and payloads
//! above 2^53 and at 2^56 - 1; nil, a number, a vector, a string, an integer; a light userdata
//! under tag 0 with each of the sixteen top nibbles; every other packed kind and each wide kind;
//! values whose nibble and light tag disagree, both ways. Forged values are written through the
//! raw FFI, since the host push refuses them.

#![allow(clippy::cast_possible_truncation, clippy::cast_sign_loss, clippy::too_many_lines)]

use std::fmt::Write as _;

use l3i::Runtime;
use l3i::bind::{Call, Return};
use l3i::convert::{Integer, Push};
use l3i::error::Result;
use l3i::extension::{NativeCodePolicy, RuntimePlan, RuntimePolicy};
use l3i::native_code::NativeCodeMode;
use l3i::quat::QuatExtension;
use l3i::raster::RasterExtension;
use l3i::stack::{Scope, ValueView};
use l3i::value::Value;

/// A value the host push would refuse: any 64 bits under any light tag, or a raw integer.
#[derive(Clone, Copy)]
enum Made {
    Light(u64, i32),
    Int(i64),
}

impl Push for Made {
    fn push_into<'s, S: Scope>(&self, scope: &'s S) -> Result<ValueView<'s>> {
        match *self {
            Made::Light(bits, tag) => {
                // SAFETY: room for one slot is checked first; the tag is below LUA_LUTAG_LIMIT.
                unsafe {
                    assert!(l3i::ffi::lua_checkstack(scope.state(), 1) != 0);
                    l3i::ffi::l3i_push_light_bits(scope.state(), bits, tag);
                }
                Ok(scope.top_value())
            }
            Made::Int(value) => Integer(value).push_into(scope),
        }
    }
}

impl Return for Made {
    fn push_results(self, call: &Call<'_>) -> Result<std::ffi::c_int> {
        self.push_into(call)?;
        Ok(1)
    }
}

/// What a packed value of light tag `tag` is on this carrier: a light userdata, or on the integer
/// oracle an integer for l3i's tags. Tag -1 asks for an integer on any carrier.
fn make(hi: u32, lo: u32, tag: i32) -> Made {
    let bits = (u64::from(hi) << 32) | u64::from(lo);
    let oracle = l3i::packed::carrier::CARRIER == "integer";
    if tag < 0 || (oracle && tag >= 128) { Made::Int(bits as i64) } else { Made::Light(bits, tag) }
}

/// One operand slot of a lowered operation.
#[derive(Clone, Copy, PartialEq)]
enum Slot {
    /// A packed scalar of this nibble kind.
    Kind(u8),
    /// A wide scalar (Color16).
    Wide,
    Number,
    Vector,
    Buffer,
}

struct Op {
    /// The receiver local (`Q`, `C`, `V`) and the method.
    receiver: &'static str,
    method: &'static str,
    slots: &'static [Slot],
    /// Lua expressions for good arguments, one per slot.
    good: &'static [&'static str],
    results: usize,
}

const QUAT: u8 = 1;
const KEY: u8 = 2;
const COLOR: u8 = 3;

fn ops() -> Vec<Op> {
    use Slot::{Buffer, Kind, Number, Vector, Wide};
    let mut ops = vec![
        Op { receiver: "Q", method: "rotate", slots: &[Kind(QUAT), Vector], good: &["qa", "v"], results: 1 },
        Op { receiver: "Q", method: "mul", slots: &[Kind(QUAT), Kind(QUAT)], good: &["qa", "qb"], results: 1 },
        Op {
            receiver: "Q",
            method: "slerp",
            slots: &[Kind(QUAT), Kind(QUAT), Number],
            good: &["qa", "qb", "0.25"],
            results: 1,
        },
        Op {
            receiver: "Q",
            method: "fromXYZW",
            slots: &[Number, Number, Number, Number],
            good: &["0.1", "-0.4", "0.3", "0.9"],
            results: 1,
        },
        Op { receiver: "Q", method: "key", slots: &[Kind(QUAT), Number], good: &["qa", "5"], results: 1 },
        Op { receiver: "Q", method: "keyRotation", slots: &[Kind(KEY)], good: &["ka"], results: 1 },
        Op { receiver: "Q", method: "keyFlags", slots: &[Kind(KEY)], good: &["ka"], results: 1 },
    ];
    let unary: &[(&str, usize)] =
        &[("red", 1), ("green", 1), ("blue", 1), ("alpha", 1), ("channels", 4), ("premultiply", 1), ("widen", 1)];
    for &(method, results) in unary {
        ops.push(Op { receiver: "C", method, slots: &[Kind(COLOR)], good: &["ca"], results });
    }
    let unary16: &[(&str, usize)] = &[
        ("red16", 1),
        ("green16", 1),
        ("blue16", 1),
        ("alpha16", 1),
        ("channels16", 4),
        ("premultiply16", 1),
        ("narrow", 1),
    ];
    for &(method, results) in unary16 {
        ops.push(Op { receiver: "C", method, slots: &[Wide], good: &["wa"], results });
    }
    for method in ["mul", "add"] {
        ops.push(Op { receiver: "C", method, slots: &[Kind(COLOR), Kind(COLOR)], good: &["ca", "cb"], results: 1 });
    }
    for method in ["mul16", "add16"] {
        ops.push(Op { receiver: "C", method, slots: &[Wide, Wide], good: &["wa", "wb"], results: 1 });
    }
    ops.extend([
        Op {
            receiver: "C",
            method: "lerp",
            slots: &[Kind(COLOR), Kind(COLOR), Number],
            good: &["ca", "cb", "0.3"],
            results: 1,
        },
        Op { receiver: "C", method: "lerp16", slots: &[Wide, Wide, Number], good: &["wa", "wb", "0.3"], results: 1 },
        Op { receiver: "C", method: "scale", slots: &[Kind(COLOR), Number], good: &["ca", "1.5"], results: 1 },
        Op { receiver: "C", method: "scale16", slots: &[Wide, Number], good: &["wa", "1.5"], results: 1 },
        Op { receiver: "C", method: "withAlpha", slots: &[Kind(COLOR), Number], good: &["ca", "40"], results: 1 },
        Op { receiver: "C", method: "withAlpha16", slots: &[Wide, Number], good: &["wa", "4000"], results: 1 },
        Op {
            receiver: "C",
            method: "rgba8",
            slots: &[Number, Number, Number, Number],
            good: &["1", "2", "3", "4"],
            results: 1,
        },
        Op {
            receiver: "C",
            method: "rgba16",
            slots: &[Number, Number, Number, Number],
            good: &["100", "200", "300", "400"],
            results: 1,
        },
        Op { receiver: "C", method: "rgb8", slots: &[Number, Number, Number], good: &["9", "8", "7"], results: 1 },
    ]);
    #[cfg(feature = "soft-render")]
    ops.push(Op {
        receiver: "V",
        method: "write",
        slots: &[Buffer, Number, Vector, Vector, Kind(COLOR)],
        good: &["buffer.create(40)", "20", "v", "v", "ca"],
        results: 1,
    });
    let _ = Buffer;
    ops
}

/// The operand cases for a slot, as `(Lua expression, label, real)`: `real` marks a value made by a
/// real operation, which must stay native.
fn cases(slot: Slot) -> Vec<(String, String, bool)> {
    let mut out: Vec<(String, String, bool)> = Vec::new();
    let mut add = |expr: String, label: String, real: bool| out.push((expr, label, real));
    let plain = [
        ("nil", "nil"),
        ("1.5", "number"),
        ("vector.create(1, 2, 3)", "vector"),
        ("'s'", "string"),
        ("true", "boolean"),
    ];
    match slot {
        Slot::Kind(kind) => {
            let real: &[&str] = match kind {
                QUAT => &["qa", "qb", "quat.IDENTITY"],
                KEY => &["ka", "quat.key(qb, 15)"],
                _ => &["ca", "cb", "raster.WHITE", "raster.TRANSPARENT"],
            };
            for expr in real {
                add((*expr).to_owned(), format!("real {expr}"), true);
            }
            let tag = 128 + i32::from(kind);
            // Every flag value; payloads zero, above 2^53, all ones (2^56 - 1), and small.
            for flags in 0..16u32 {
                for (payload_hi, payload_lo, name) in [
                    (0u32, 0u32, "zero"),
                    (0x20_0001, 7, "above 2^53"),
                    (0xFF_FFFF, u32::MAX, "2^56-1"),
                    (0, 0x4433_2211, "small"),
                ] {
                    let hi = (u32::from(kind) << 28) | (flags << 24) | payload_hi;
                    add(
                        format!("make({hi}, {payload_lo}, {tag})"),
                        format!("forged flags {flags} payload {name}"),
                        false,
                    );
                }
            }
            for (expr, label) in plain {
                add(expr.to_owned(), label.to_owned(), false);
            }
            add("42i".to_owned(), "integer 42".to_owned(), false);
            add(format!("make({}, 9, -1)", u32::from(kind) << 28), "integer with the kind's nibble".to_owned(), false);
            for nibble in 0..16u32 {
                add(
                    format!("make({}, 4660, 0)", nibble << 28),
                    format!("host light userdata tag 0 nibble {nibble}"),
                    false,
                );
            }
            for other in 1..16u8 {
                let hi = u32::from(other) << 28;
                add(format!("make({hi}, 3, {})", 128 + i32::from(other)), format!("packed kind {other}"), false);
            }
            for wide in [144, 145] {
                add(
                    format!("make({}, 3, {wide})", u32::from(kind) << 28),
                    format!("wide tag {wide} with the kind's nibble"),
                    false,
                );
                add(format!("make({}, 3, {wide})", 9u32 << 28), format!("wide tag {wide} with nibble 9"), false);
            }
            for other in [1u8, 2, 3, 4, 9] {
                if other == kind {
                    continue;
                }
                add(
                    format!("make({}, 5, {})", u32::from(kind) << 28, 128 + i32::from(other)),
                    format!("nibble {kind} under tag {}", 128 + i32::from(other)),
                    false,
                );
                add(
                    format!("make({}, 5, {tag})", u32::from(other) << 28),
                    format!("nibble {other} under tag {tag}"),
                    false,
                );
            }
        }
        Slot::Wide => {
            for expr in ["wa", "wb", "raster.WHITE16"] {
                add(expr.to_owned(), format!("real {expr}"), true);
            }
            for (hi, lo, name) in [(0u32, 0u32, "zero"), (u32::MAX, u32::MAX, "all ones"), (0x8000_0000, 1, "top bit")]
            {
                add(format!("make({hi}, {lo}, 144)"), format!("forged Color16 {name}"), false);
                add(format!("make({hi}, {lo}, 145)"), format!("Bits64 {name}"), false);
                add(format!("make({hi}, {lo}, 0)"), format!("host light userdata {name}"), false);
                add(format!("make({hi}, {lo}, 131)"), format!("Color tag {name}"), false);
            }
            add("ca".to_owned(), "a Color".to_owned(), false);
            for (expr, label) in plain {
                add(expr.to_owned(), label.to_owned(), false);
            }
            add("42i".to_owned(), "integer 42".to_owned(), false);
        }
        Slot::Number => {
            for (expr, label) in [
                ("nil", "nil"),
                ("'3'", "numeric string"),
                ("0 / 0", "NaN"),
                ("1 / 0", "infinity"),
                ("-0.0", "negative zero"),
                ("2^53 + 2", "above 2^53"),
                ("3.75", "fraction"),
                ("-1", "minus one"),
                ("ca", "a Color"),
                ("vector.create(1, 2, 3)", "vector"),
                ("7i", "integer 7"),
            ] {
                add(expr.to_owned(), label.to_owned(), false);
            }
        }
        Slot::Vector => {
            for (expr, label) in
                [("nil", "nil"), ("1", "number"), ("qa", "a Quaternion"), ("vector.create(0 / 0, 1, 2)", "NaN lane")]
            {
                add(expr.to_owned(), label.to_owned(), false);
            }
        }
        Slot::Buffer => {
            for (expr, label) in [("nil", "nil"), ("'x'", "string"), ("buffer.create(19)", "short buffer")] {
                add(expr.to_owned(), label.to_owned(), false);
            }
        }
    }
    out
}

/// Whether the lowering is bit-exact against its binder. Rotation products, slerp and rotate
/// differ from the binder in the last bits of the arithmetic (the quat tests bound the angle,
/// 5e-5); for those the comparison is acceptance, type, light tag and the kind and flag byte, and
/// the payload bits are compared across carriers by the differential corpus instead.
fn exact(op: &Op) -> bool {
    !(op.receiver == "Q" && matches!(op.method, "rotate" | "mul" | "slerp"))
}

/// A result slot as a comparable description: the type tag and, by type, the exact bits (only the
/// kind and flag byte of a packed scalar, and only the type of a vector, when not `exact`).
fn describe(value: &Value, scope: &impl Scope, exact: bool) -> String {
    scope
        .with_frame(|frame| {
            let view = value.push_to(frame)?;
            // SAFETY: the view names a live slot of the frame.
            let raw = unsafe { l3i::ffi::l3i_stack_slot(frame.state(), view.index()).as_ref() }.expect("a slot");
            Ok(match raw.tag() {
                l3i::ffi::LUA_TNIL => "nil".to_owned(),
                l3i::ffi::LUA_TBOOLEAN => format!("boolean {}", raw.boolean()),
                l3i::ffi::LUA_TSTRING => format!("string {:?}", view.read::<String>()?),
                l3i::ffi::LUA_TVECTOR if !exact => "vector".to_owned(),
                l3i::ffi::LUA_TVECTOR => format!("vector {:#018x} {:#010x}", raw.pointer_bits(), raw.extra()),
                l3i::ffi::LUA_TLIGHTUSERDATA if !exact => {
                    format!("light top byte {:#04x} tag {}", raw.pointer_bits() >> 56, raw.extra())
                }
                l3i::ffi::LUA_TLIGHTUSERDATA => format!("light {:#018x} tag {}", raw.pointer_bits(), raw.extra()),
                tag if !exact => format!("type {tag} top byte {:#04x}", raw.pointer_bits() >> 56),
                tag => format!("type {tag} bits {:#018x}", raw.pointer_bits()),
            })
        })
        .unwrap()
}

#[test]
fn every_lowered_operation_agrees_with_its_binder_on_every_operand() {
    let policy = RuntimePolicy::new()
        .compat_global("@dream/quat", "quat")
        .compat_global("@dream/raster", "raster")
        .compat_global("@dream/soft-render", "soft")
        .native_code(NativeCodePolicy {
            mode: NativeCodeMode::Annotated,
            record_counters: true,
            ..NativeCodePolicy::default()
        });
    let builder = RuntimePlan::builder().policy(policy).extension(QuatExtension).extension(RasterExtension);
    #[cfg(feature = "soft-render")]
    let builder = builder.extension(l3i::soft_render::SoftRenderExtension);
    let plan = builder.finalize().unwrap();
    let runtime = Runtime::from_plan(&plan).unwrap();
    let generator = runtime.native_code().expect("built with native code");
    if !generator.is_available() {
        eprintln!("no Luau code generator on this platform; skipping");
        return;
    }
    let maker = runtime.bind_function("dream.tests.make", make).unwrap();
    runtime.set_global("make", &maker).unwrap();

    let ops = ops();
    // The cases, built by script: real values come from the real operations.
    let mut source = String::from(
        "local qa = quat.axisAngle(vector.create(0, 0, 1), 0.3)\n\
         local qb = quat.axisAngle(vector.create(1, 2, 0.5), 1.1)\n\
         local ka = quat.key(qa, 3)\n\
         local ca = raster.rgba8(17, 34, 51, 68)\n\
         local cb = raster.rgba8(200, 100, 50, 255)\n\
         local wa = raster.widen(ca)\n\
         local wb = raster.rgba16(100, 20000, 65535, 3)\n\
         local v = vector.create(1, 2, 3)\n\
         local cases = {}\n",
    );
    let mut labels: Vec<Vec<(String, bool)>> = Vec::new();
    for (index, op) in ops.iter().enumerate() {
        let mut op_labels = Vec::new();
        writeln!(source, "cases[{}] = {{", index + 1).unwrap();
        for (position, slot) in op.slots.iter().enumerate() {
            for (expr, label, real) in cases(*slot) {
                let mut args: Vec<String> = op.good.iter().map(|g| (*g).to_owned()).collect();
                args[position] = expr;
                writeln!(source, "  table.pack({}),", args.join(", ")).unwrap();
                op_labels.push((format!("{}:{} argument {} {label}", op.receiver, op.method, position + 1), real));
            }
        }
        writeln!(source, "}}").unwrap();
        labels.push(op_labels);
    }
    source.push_str("return cases\n");
    let cases = runtime.load_function(&format!("return function() {source} end")).unwrap();
    let cases: Value = cases.invoke::<Value, _>(&runtime.stack(), ()).unwrap();

    // Each operation as a lowered namecall and as the bound method, in fixed-arity wrappers so
    // the hook sees one result and a fixed argument count.
    let mut driver = String::from(
        "--!native\n\
         local Q: dream_quat_Math = quat.math()\n\
         local C: dream_raster_Math = raster.math()\n",
    );
    #[cfg(feature = "soft-render")]
    driver.push_str("local V: dream_soft_render_Vertices = soft.vertices()\n");
    driver.push_str("local lowered, bound = {}, {}\n");
    for (index, op) in ops.iter().enumerate() {
        let params: Vec<String> = (1..=op.slots.len()).map(|i| format!("a{i}")).collect();
        let results: Vec<String> = (1..=op.results).map(|i| format!("r{i}")).collect();
        let (params, results) = (params.join(", "), results.join(", "));
        let (receiver, method) = (op.receiver, op.method);
        writeln!(
            driver,
            "lowered[{n}] = function({params}) local {results} = {receiver}:{method}({params}) return {results} end\n\
             bound[{n}] = function({params}) local {results} = {receiver}.{method}({receiver}, {params}) return {results} end",
            n = index + 1
        )
        .unwrap();
    }
    driver.push_str("return { lowered = lowered, bound = bound }\n");
    // Loaded through the path that compiles natively (`Runtime::load` does not). Only the
    // wrappers are native (the policy compiles `--!native` modules alone), so an exit counted
    // around a call is the lowered call's own.
    let wrappers: Value = {
        use l3i::source::LoadScope;
        let stack = runtime.stack();
        let chunk = stack.load_source("=equivalence", &driver, &runtime.compile_options()).unwrap();
        chunk.invoke::<Value, _>(&stack, ()).unwrap()
    };
    let dispatch = runtime
        .load_function(
            "return function(wrappers, cases) return function(path, op, i) \
             local args = cases[op][i] \
             return pcall(wrappers[path][op], table.unpack(args, 1, args.n)) end end",
        )
        .unwrap();
    let drive: l3i::value::Function =
        dispatch.invoke::<l3i::value::Function, _>(&runtime.stack(), (&wrappers, &cases)).unwrap();

    let mut failures = Vec::new();
    let mut total = 0usize;
    for (op_index, op) in ops.iter().enumerate() {
        let mut stayed_native = 0usize;
        for (case_index, (label, real)) in labels[op_index].iter().enumerate() {
            total += 1;
            let stack = runtime.stack();
            let args = ("lowered", op_index as f64 + 1.0, case_index as f64 + 1.0);
            let before = generator.execution_stats(&stack);
            let lowered = drive.invoke::<(bool, Value, Value, Value, Value), _>(&stack, args).unwrap();
            let after = generator.execution_stats(&stack);
            let exits = after.vm_exits_taken - before.vm_exits_taken;
            let bound =
                drive.invoke::<(bool, Value, Value, Value, Value), _>(&stack, ("bound", args.1, args.2)).unwrap();
            let describe_all = |(ok, a, b, c, d): &(bool, Value, Value, Value, Value)| {
                format!(
                    "{} {} | {} | {} | {}",
                    if *ok { "ok" } else { "error" },
                    describe(a, &stack, exact(op)),
                    describe(b, &stack, exact(op)),
                    describe(c, &stack, exact(op)),
                    describe(d, &stack, exact(op))
                )
            };
            let (lowered_text, bound_text) = (describe_all(&lowered), describe_all(&bound));
            if lowered_text != bound_text {
                failures.push(format!("{label}: lowered `{lowered_text}` but bound `{bound_text}`"));
            }
            if exits == 0 && !bound.0 {
                failures.push(format!("{label}: lowered code accepted an operand the binder refuses"));
            }
            if *real && exits != 0 {
                failures.push(format!("{label}: a real value left native code ({exits} exits)"));
            }
            if exits == 0 && lowered.0 {
                stayed_native += 1;
            }
        }
        if stayed_native == 0 {
            failures.push(format!("{}:{}: no case ran natively; the operation is not lowered", op.receiver, op.method));
        }
    }
    eprintln!(
        "{total} operand cases over {} lowered operations on the {} carrier",
        ops.len(),
        l3i::packed::carrier::CARRIER
    );
    assert!(failures.is_empty(), "{} disagreements:\n{}", failures.len(), failures.join("\n"));
}
