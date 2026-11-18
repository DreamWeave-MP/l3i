//! The packed scalar carrier's contract (`src/packed/carrier.rs`), one suite for every carrier: the
//! 64 bits arrive whole wherever the value goes, the kind nibble decides what a parameter accepts,
//! values compare and key tables by their bits and kind, and `typeof` names the kind. The suite
//! is the specification: no expectation differs by carrier, except where a test reads the raw slot.

#![allow(clippy::cast_possible_truncation, clippy::cast_sign_loss, clippy::unreadable_literal)]

use std::fmt::Write as _;

use l3i::Runtime;
use l3i::convert::Push;
use l3i::error::Result;
use l3i::packed::{BufferPack, Packed, PackedScalar, encode};
use l3i::quat::{AnimationKey, Quat, Quaternion};
use l3i::raster::{ClipRect, Color, Color16};
use l3i::stack::Scope;

const CARRIER: &str = l3i::packed::carrier::CARRIER;

/// A host kind whose payload and flags are exactly what it holds: any bits survive its unpack.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Probe(u64, u8);

impl PackedScalar for Probe {
    const KIND: u8 = 9;
    const NAME: &'static str = "Probe";
    fn pack(&self) -> (u64, u8) {
        (self.0, self.1)
    }
    fn unpack(payload: u64, flags: u8) -> Result<Self> {
        Ok(Probe(payload, flags))
    }
}

fn runtime() -> Runtime {
    let runtime = Runtime::new().unwrap();
    runtime.register_packed::<Probe>().unwrap();
    runtime
}

/// The payloads every round trip covers: zero, one, either side of 2^53, all 56 bits.
const PAYLOADS: [u64; 6] = [0, 1, (1 << 53) - 1, (1 << 53) + 1, 0x00AB_CDEF_0123_4567, (1 << 56) - 1];

/// The type tag and 64 bits of the value a Rust push leaves, read through the raw slot.
fn raw_bits(runtime: &Runtime, value: &impl Push) -> (i32, u64) {
    runtime
        .stack()
        .with_frame(|frame| {
            let view = value.push_into(frame)?;
            // SAFETY: the view names a live slot of the frame.
            let raw = unsafe { l3i::ffi::l3i_stack_slot(frame.state(), view.index()).as_ref() }.expect("a slot");
            Ok((raw.tag(), raw.pointer_bits()))
        })
        .unwrap()
}

#[test]
fn every_flag_and_the_whole_payload_survive_the_carrier() {
    let runtime = runtime();
    let identity = runtime.bind_function("dreamweave.tests.identity", |p: Packed<Probe>| p).unwrap();
    runtime.set_global("identity", &identity).unwrap();
    // Through a table, an upvalue, a coroutine and back as a function result.
    let through = runtime
        .load_function(
            "return function(v) \
               local t = { v } \
               local f = function() return t[1] end \
               local co = coroutine.wrap(function(x) local y = coroutine.yield(x) return y end) \
               co(f()) \
               return identity(co(t[1])) \
             end",
        )
        .unwrap();
    for flags in 0..16u8 {
        for payload in PAYLOADS {
            let value = Packed(Probe(payload, flags));
            let bits = encode(Probe::KIND, flags, payload).unwrap() as u64;
            // The value word holds the bits on the light carrier; a boxed value's slot holds its box.
            if CARRIER != "boxed userdata" {
                let (tag, raw) = raw_bits(&runtime, &value);
                assert_eq!(raw, bits, "the raw slot holds the exact bits for flags {flags} payload {payload:#x}");
                assert_eq!(tag, l3i::ffi::LUA_TLIGHTUSERDATA);
            }
            let back: Packed<Probe> = through.invoke(&runtime.stack(), (&value,)).unwrap();
            assert_eq!(back, value, "flags {flags} payload {payload:#x} through a table, an upvalue and a coroutine");
            assert_eq!(back.bits().unwrap() as u64, bits);
        }
    }
    // Kind 0 and kind 16 do not exist.
    assert!(encode(0, 0, 0).is_err() && encode(16, 0, 0).is_err());
    // A wide value keeps all 64 bits, the top bit and the kind nibble's place included.
    for bits in [0u64, 1, u64::MAX, 0x8000_0000_0000_0000, 0x1234_5678_9ABC_DEF0] {
        if CARRIER != "boxed userdata" {
            assert_eq!(raw_bits(&runtime, &Color16::from_packed(bits)).1, bits);
        }
        let back: Color16 = runtime.stack().with_frame(|f| Color16::from_packed(bits).push_into(f)?.read()).unwrap();
        assert_eq!(back.packed(), bits);
    }
}

#[test]
fn a_parameter_accepts_its_own_kind_and_refuses_every_other() {
    let runtime = runtime();
    let rotation = Quaternion::pack(Quat::IDENTITY);
    let key = AnimationKey::pack(Quat::IDENTITY, 3);
    let color = Color::rgba(1, 2, 3, 4).pack();
    let clip = ClipRect::new(0, 0, 4, 4).unwrap().pack();
    let probe = Packed(Probe(7, 1));
    runtime.set_global("rotation", &rotation).unwrap();
    runtime.set_global("key", &key).unwrap();
    runtime.set_global("color", &color).unwrap();
    runtime.set_global("clip", &clip).unwrap();
    runtime.set_global("probe", &probe).unwrap();
    runtime.set_global("wide", &Color16::WHITE).unwrap();
    let takes = [
        ("rotation", runtime.bind_function("dreamweave.tests.q", |_: Packed<Quaternion>| true).unwrap()),
        ("key", runtime.bind_function("dreamweave.tests.k", |_: Packed<AnimationKey>| true).unwrap()),
        ("color", runtime.bind_function("dreamweave.tests.c", |_: Packed<Color>| true).unwrap()),
        ("clip", runtime.bind_function("dreamweave.tests.r", |_: Packed<ClipRect>| true).unwrap()),
        ("probe", runtime.bind_function("dreamweave.tests.p", |_: Packed<Probe>| true).unwrap()),
    ];
    let values = ["rotation", "key", "color", "clip", "probe"];
    for (expected, function) in &takes {
        runtime.set_global("take", function).unwrap();
        for given in values {
            let accepted = runtime.eval::<bool>(&format!("return (pcall(take, {given}))")).unwrap();
            assert_eq!(accepted, given == *expected, "a {expected} parameter given a {given}");
            let value = runtime.global(given).unwrap();
            let matches = runtime
                .stack()
                .with_frame(|frame| {
                    let view = value.push_to(frame)?;
                    Ok(match *expected {
                        "rotation" => <Packed<Quaternion> as l3i::convert::FromView>::matches(view),
                        "key" => <Packed<AnimationKey> as l3i::convert::FromView>::matches(view),
                        "color" => <Packed<Color> as l3i::convert::FromView>::matches(view),
                        "clip" => <Packed<ClipRect> as l3i::convert::FromView>::matches(view),
                        _ => <Packed<Probe> as l3i::convert::FromView>::matches(view),
                    })
                })
                .unwrap();
            assert_eq!(matches, given == *expected, "matches: a {expected} parameter given a {given}");
        }
        for other in ["nil", "1", "1.5", "'x'", "vector.create(1, 2, 3)", "{}", "wide"] {
            assert!(
                !runtime.eval::<bool>(&format!("return (pcall(take, {other}))")).unwrap(),
                "{expected} given {other}"
            );
        }
    }
    // A wide parameter takes any value on the carrier: nothing in the bits names a wide kind. The
    // test pins it, so tightening it is a deliberate change.
    let wide = runtime.bind_function("dreamweave.tests.w", |c: Color16| c.packed() as f64).unwrap();
    runtime.set_global("take", &wide).unwrap();
    for given in values.iter().chain(&["wide"]) {
        assert!(
            runtime.eval::<bool>(&format!("return (pcall(take, {given}))")).unwrap(),
            "a wide parameter given a {given}"
        );
    }
    for other in ["nil", "1.5", "'x'", "vector.create(1, 2, 3)"] {
        assert!(!runtime.eval::<bool>(&format!("return (pcall(take, {other}))")).unwrap(), "wide given {other}");
    }
}

#[test]
fn values_compare_and_key_tables_by_bits_and_kind() {
    let runtime = runtime();
    let make = runtime
        .bind_function("dreamweave.tests.make", |payload: f64, flags: f64| Packed(Probe(payload as u64, flags as u8)))
        .unwrap();
    let make16 =
        runtime.bind_function("dreamweave.tests.make16", |bits: f64| Color16::from_packed(bits as u64)).unwrap();
    runtime.set_global("make", &make).unwrap();
    runtime.set_global("make16", &make16).unwrap();
    runtime
        .exec(
            "local a, b, c = make(5, 1), make(5, 1), make(5, 2) \
             assert(a == b and rawequal(a, b), 'equal bits are equal') \
             assert(a ~= c and not rawequal(a, c), 'other flags are other values') \
             local t = { [a] = 'hit' } \
             assert(t[b] == 'hit' and rawget(t, b) == 'hit', 'a second push keys the same entry') \
             assert(t[c] == nil, 'other bits miss') \
             local f = function() return a end \
             assert(f() == b, 'through an upvalue') \
             local w = make16(5) \
             assert(w ~= make16(6) and w == make16(5), 'wide values compare by bits')",
        )
        .unwrap();
    // A Color16 never equals a value of another kind with the same bits: the light tag, or the
    // boxed carrier's kind, is part of the value.
    let same = runtime
        .bind_function("dreamweave.tests.same", |bits: f64| {
            Color16::from_packed(encode(Probe::KIND, 0, bits as u64).unwrap() as u64)
        })
        .unwrap();
    runtime.set_global("same", &same).unwrap();
    runtime
        .exec(
            "assert(make(3, 0) ~= same(3) and not rawequal(make(3, 0), same(3)), 'kind is part of the value') \
             assert(typeof(make(1, 0)) == 'Probe' and typeof(make16(1)) == 'Color16', typeof(make(1, 0))) \
             assert(string.match(tostring(make(1, 0)), '^Probe: 0x%x+$'), tostring(make(1, 0))) \
             assert(not pcall(function() return make(1, 0) + 1 end), 'no arithmetic') \
             assert(not pcall(function() return make(1, 0) < make(2, 0) end), 'no ordering') \
             assert(not pcall(function() return -make(1, 0) end), 'no unary minus')",
        )
        .unwrap();
}

/// Bytes `BufferPack` writes for a fixed set of values per kind, checked in: the wire format of
/// packed scalars, which no carrier change may move. The golden was generated by Luau 0.745's
/// integer build.
#[test]
fn buffer_pack_bytes_match_the_checked_in_golden() {
    let mut out = String::new();
    let mut line = |name: &str, bytes: &[u8]| {
        write!(out, "{name} ").unwrap();
        for byte in bytes {
            write!(out, "{byte:02x}").unwrap();
        }
        out.push('\n');
    };
    let mut put = |name: &str, write: &dyn Fn(&mut [u8]) -> Result<()>| {
        let mut bytes = [0u8; 8];
        write(&mut bytes).unwrap();
        line(name, &bytes);
    };
    // Exact components, not axis-angle: `sin` and `cos` round differently from one libm to the next
    // (Windows' moved a lane of the quarter turn), and the golden pins the encoding, not a libm.
    // These are what `Quat::from_axis_angle` gave on the Linux build that wrote the golden.
    let exact = |[x, y, z, w]: [u64; 4]| Quat {
        x: f64::from_bits(x),
        y: f64::from_bits(y),
        z: f64::from_bits(z),
        w: f64::from_bits(w),
    };
    let rotations = [
        ("identity", Quat::IDENTITY),
        ("quarter z", exact([0, 0, 0x3fe6_a09e_667f_3bcc, 0x3fe6_a09e_667f_3bcd])),
        ("half x", exact([0x3ff0_0000_0000_0000, 0, 0, 0x3c91_a626_3314_5c07])),
        ("odd", exact([0x3fd1_2507_85bc_e34e, 0xbfdc_930c_8990_2583, 0x3fe7_2530_8e25_6610, 0x3fdd_07b8_06c7_6111])),
        ("tiny", exact([0, 0x3f0a_36e2_eaed_5994, 0, 0x3fef_ffff_ff54_3389])),
    ];
    for (name, q) in rotations {
        put(&format!("Quaternion {name}"), &|b| Quaternion::pack(q).write_to(b));
        for flags in [0u8, 7, 15] {
            put(&format!("AnimationKey {name} flags {flags}"), &|b| AnimationKey::pack(q, flags).write_to(b));
        }
    }
    for (name, c) in [
        ("transparent", Color::TRANSPARENT),
        ("white", Color::WHITE),
        ("mixed", Color::rgba(0x11, 0x22, 0x33, 0x44)),
        ("max red", Color::rgba(255, 0, 0, 1)),
    ] {
        put(&format!("Color {name}"), &|b| c.pack().write_to(b));
        put(&format!("Color16 {name}"), &|b| Color16::widen(c).write_to(b));
    }
    for (name, r) in [
        ("all", ClipRect::ALL),
        ("unit", ClipRect::new(0, 0, 1, 1).unwrap()),
        ("odd", ClipRect::new(3, 5, 640, 480).unwrap()),
    ] {
        put(&format!("ClipRect {name}"), &|b| r.pack().write_to(b));
    }
    for (name, bits) in [("zero", 0u64), ("ones", u64::MAX), ("top", 1 << 63)] {
        put(&format!("Color16 raw {name}"), &|b| Color16::from_packed(bits).write_to(b));
    }
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/support/packed/buffer_pack.golden");
    if std::env::var_os("L3I_WRITE_PACKED_GOLDEN").is_some() {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, &out).unwrap();
        return;
    }
    let golden = std::fs::read_to_string(&path).unwrap().replace("\r\n", "\n");
    assert_eq!(out, golden, "BufferPack bytes moved: a wire format changed");
}

#[test]
fn the_host_push_refuses_l3i_tags_and_pointers_with_a_kind_nibble() {
    use l3i::memory::LightUserdata;
    let runtime = runtime();
    let refused =
        |value: LightUserdata| runtime.stack().with_frame(|f| value.push_into(f).map(|_| ())).unwrap_err().to_string();
    let tagged = refused(LightUserdata { pointer: std::ptr::null_mut(), tag: 128 });
    assert!(tagged.contains("128") && tagged.contains("packed scalars"), "{tagged}");
    // Only a 64-bit pointer can set the top nibble of the value word.
    #[cfg(target_pointer_width = "64")]
    {
        let nibble = refused(LightUserdata { pointer: (0x1000_0000_0000_0010_usize) as *mut std::ffi::c_void, tag: 0 });
        assert!(nibble.contains("top nibble") && nibble.contains("HWASan"), "{nibble}");
    }
    // A real pointer under a host tag crosses.
    let mut slot = 0u32;
    runtime
        .stack()
        .with_frame(|f| LightUserdata { pointer: (&raw mut slot).cast(), tag: 127 }.push_into(f).map(|_| ()))
        .unwrap();
    let named = runtime.set_light_userdata_name(128, "Mine").unwrap_err().to_string();
    assert!(named.contains("packed scalars"), "{named}");
    runtime.set_light_userdata_name(127, "Mine").unwrap();
}

/// Light userdata written through the raw FFI, past the host push: the outcome follows the nibble,
/// whatever the tag. Only the light carrier carries packed scalars in light userdata.
#[test]
fn forged_light_userdata_are_read_by_their_nibble() {
    if CARRIER != "light userdata" {
        return;
    }
    let runtime = runtime();
    let take = runtime.bind_function("dreamweave.tests.take", |q: Packed<Probe>| q.0.0 as f64).unwrap();
    let forge = runtime
        .bind_function(
            "dreamweave.tests.forge",
            |call: &l3i::bind::Call, hi: f64, lo: f64, tag: f64| -> Result<l3i::value::Value> {
                let bits = ((hi as u64) << 32) | lo as u64;
                // SAFETY: room is checked; any tag below LUA_LUTAG_LIMIT is a valid light userdata.
                unsafe {
                    assert!(l3i::ffi::lua_checkstack(call.state(), 1) != 0);
                    l3i::ffi::l3i_push_light_bits(call.state(), bits, tag as i32);
                }
                l3i::value::Value::store(call.top_value())
            },
        )
        .unwrap();
    runtime.set_global("take", &take).unwrap();
    runtime.set_global("forge", &forge).unwrap();
    let nine = f64::from(9u32 << 28);
    for (hi, tag, accepted) in [
        (nine, 137.0, true),
        (nine, 130.0, true),
        (nine, 0.0, true),
        (nine, 144.0, true),
        (f64::from(2u32 << 28), 137.0, false),
    ] {
        let ok = runtime.eval::<bool>(&format!("return (pcall(take, forge({hi}, 77, {tag})))")).unwrap();
        assert_eq!(ok, accepted, "nibble {} under tag {tag}", (hi as u64) >> 28);
    }
}

/// The boxed carrier interns: equal bits are one object, which a collection may free and a later
/// push makes again; every push may allocate, so a memory limit holds against packed scalars too.
#[test]
fn interning_survives_collection_and_respects_the_memory_limit() {
    let runtime = runtime();
    let make = runtime.bind_function("dreamweave.tests.make", |payload: f64| Packed(Probe(payload as u64, 0))).unwrap();
    runtime.set_global("make", &make).unwrap();
    let collect = runtime
        .bind_function("dreamweave.tests.collect", |call: &l3i::bind::Call, _what: Option<String>| {
            // SAFETY: a live thread inside a call.
            unsafe { l3i::ffi::lua_gc(call.state(), l3i::ffi::LUA_GCCOLLECT, 0) };
        })
        .unwrap();
    runtime.set_global("collectgarbage", &collect).unwrap();
    runtime
        .exec(
            "for round = 1, 3 do \
               local keep = {} \
               for i = 1, 2000 do keep[i] = make(i) end \
               collectgarbage('collect') \
               for i = 1, 2000 do assert(rawequal(keep[i], make(i)), 'interned while alive') end \
               keep = nil \
               collectgarbage('collect') \
               assert(make(7) == make(7) and rawequal(make(7), make(7)), 'made again after collection') \
             end",
        )
        .unwrap();
    // Under GC stress: a collector step after every push (scripts cannot ask Luau for one).
    let step = runtime
        .bind_function("dreamweave.tests.step", |call: &l3i::bind::Call| {
            // SAFETY: a live thread inside a call; a step may run finalizers but raises nothing.
            unsafe { l3i::ffi::lua_gc(call.state(), l3i::ffi::LUA_GCSTEP, 1) };
        })
        .unwrap();
    runtime.set_global("step", &step).unwrap();
    runtime
        .exec(
            "local t = {} \
             for i = 1, 500 do t[make(i % 50)] = (t[make(i % 50)] or 0) + 1 step() end \
             local n = 0 for _, v in t do n += 1 assert(v == 10) end assert(n == 50, n)",
        )
        .unwrap();
    let limited = Runtime::builder().memory_limit(4 << 20).build().unwrap();
    limited.register_packed::<Probe>().unwrap();
    let make = limited.bind_function("dreamweave.tests.make", |payload: f64| Packed(Probe(payload as u64, 0))).unwrap();
    limited.set_global("make", &make).unwrap();
    let outcome = limited.exec("local keep = {} for i = 1, 1000000 do keep[i] = make(i) end");
    if CARRIER == "boxed userdata" {
        let error = outcome.unwrap_err().to_string();
        assert!(error.to_lowercase().contains("memory"), "{error}");
    } else {
        // A light userdata or an integer allocates nothing of its own; the table alone fails.
        let _ = outcome;
    }
}
