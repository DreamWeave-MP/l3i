//! Plain 64-bit values after the integer type: `@dream/bits64` and the `Bits64` it makes, the
//! number readers that lost their integer arm (their accepted and refused numbers unchanged), and
//! the writers that refuse a number they cannot hold exactly instead of rounding it.

#![allow(clippy::cast_possible_truncation, clippy::cast_precision_loss, clippy::cast_sign_loss, clippy::float_cmp)]

use l3i::Runtime;
use l3i::convert::{Bits64, Exact, Push};
use l3i::extension::{RuntimePlan, RuntimePolicy};

fn runtime() -> Runtime {
    let plan = RuntimePlan::builder()
        .policy(RuntimePolicy::new().compat_global("@dream/bits64", "bits64"))
        .finalize()
        .unwrap();
    Runtime::from_plan(&plan).unwrap()
}

#[test]
fn the_module_converts_without_losing_or_inventing_bits() {
    runtime()
        .exec(
            "local ones, zero = bits64.fromHex('ffffffffffffffff'), bits64.fromNumber(0) \
             assert(bits64.hex(ones) == 'ffffffffffffffff' and bits64.hex(zero) == '0000000000000000', 'hex') \
             assert(bits64.fromHex('0x2A') == bits64.fromNumber(42) and bits64.fromHex('2a') == bits64.fromNumber(42), 'fromHex') \
             for _, s in { '', 'g', '0x', '12345678901234567' } do assert(not pcall(bits64.fromHex, s), s) end \
             local hi, lo = bits64.parts(bits64.fromHex('0123456789abcdef')) \
             assert(hi == 0x01234567 and lo == 0x89abcdef, 'parts') \
             assert(bits64.fromParts(hi, lo) == bits64.fromHex('0123456789abcdef'), 'fromParts inverts parts') \
             assert(not pcall(bits64.fromParts, 2^32, 0) and not pcall(bits64.fromParts, 0, -1) and not pcall(bits64.fromParts, 1.5, 0)) \
             assert(bits64.toNumber(bits64.fromNumber(2^53)) == 2^53, '2^53 is a number') \
             local ok, err = pcall(bits64.toNumber, bits64.fromParts(0x200000, 1)) \
             assert(not ok and err:find('above 2%^53'), err) \
             assert(bits64.fromNumber(2^63) == bits64.fromHex('8000000000000000'), 'an exact whole number past 2^53 is still its bits') \
             assert(not pcall(bits64.fromNumber, -1) and not pcall(bits64.fromNumber, 0.5) and not pcall(bits64.fromNumber, 2^64)) \
             local b = buffer.create(16) \
             bits64.write(b, 4, bits64.fromHex('0102030405060708')) \
             assert(buffer.readu8(b, 4) == 8 and buffer.readu8(b, 11) == 1, 'little-endian by default') \
             bits64.write(b, 4, bits64.fromHex('0102030405060708'), 'big') \
             assert(buffer.readu8(b, 4) == 1 and buffer.readu8(b, 11) == 8, 'big-endian on request') \
             assert(bits64.read(b, 4, 'big') == bits64.fromHex('0102030405060708') and bits64.read(b, 4) == bits64.fromHex('0807060504030201')) \
             assert(not pcall(bits64.read, b, 9) and not pcall(bits64.read, b, -1) and not pcall(bits64.read, b, 0, 'middle')) \
             local t = { [ones] = 'all' } assert(t[bits64.fromHex('FFFFFFFFFFFFFFFF')] == 'all', 'a table key by value') \
             assert(not pcall(function() return ones + 1 end) and not pcall(function() return ones < zero end), 'no arithmetic')",
        )
        .unwrap();
}

/// The readers that took an integer as well as a number now take the number alone, and accept
/// and refuse exactly the numbers they did before: whole numbers in range for the exact readers,
/// rounded numbers in range for the compatibility readers, never NaN or an infinity.
#[test]
fn number_readers_accept_the_same_numbers_as_before() {
    let runtime = Runtime::new().unwrap();
    let mut probes = vec![0.0, -0.0, 0.5, -0.5, 1.5, -1.5, f64::NAN, f64::INFINITY, f64::NEG_INFINITY];
    for edge in [2f64.powi(31), 2f64.powi(32), 2f64.powi(53), 2f64.powi(63), 2f64.powi(64)] {
        for value in [
            edge - 1.0,
            edge,
            edge + 1.0,
            edge - 0.5,
            edge + 0.5,
            edge * (1.0 - f64::EPSILON),
            edge * (1.0 + f64::EPSILON),
        ] {
            probes.push(value);
            probes.push(-value);
        }
    }
    // An independent oracle: whole numbers in range; for the rounding readers, rounding half away
    // from zero first, then the range.
    let whole = |v: f64, lo: f64, hi: f64| v.is_finite() && v.fract() == 0.0 && v >= lo && v < hi;
    let rounded = |v: f64, lo: f64, hi: f64| v.is_finite() && v.round() >= lo && v.round() < hi;
    for value in probes {
        let stack = runtime.stack();
        stack
            .with_frame(|frame| {
                let view = frame.push_number(value)?;
                let checks = [
                    ("Exact<i32>", view.read::<Exact<i32>>().is_ok(), whole(value, -(2f64.powi(31)), 2f64.powi(31))),
                    ("Exact<u32>", view.read::<Exact<u32>>().is_ok(), whole(value, 0.0, 2f64.powi(32))),
                    ("Exact<i64>", view.read::<Exact<i64>>().is_ok(), whole(value, -(2f64.powi(63)), 2f64.powi(63))),
                    ("Exact<u64>", view.read::<Exact<u64>>().is_ok(), whole(value, 0.0, 2f64.powi(64))),
                    ("i32", view.read::<i32>().is_ok(), rounded(value, -(2f64.powi(31)), 2f64.powi(31))),
                    ("u32", view.read::<u32>().is_ok(), rounded(value, 0.0, 2f64.powi(32))),
                    ("i64", view.read::<i64>().is_ok(), rounded(value, -(2f64.powi(63)), 2f64.powi(63))),
                    ("u64", view.read::<u64>().is_ok(), rounded(value, 0.0, 2f64.powi(64))),
                ];
                for (reader, got, expected) in checks {
                    assert_eq!(got, expected, "{reader} reading {value:e}");
                    let matched = match reader {
                        "Exact<i32>" => view.is::<Exact<i32>>(),
                        "Exact<u64>" => view.is::<Exact<u64>>(),
                        "i32" => view.is::<i32>(),
                        _ => got,
                    };
                    assert_eq!(matched, got, "{reader} matches reading {value:e}");
                }
                assert!(view.read::<Bits64>().is_err(), "a number is never a Bits64");
                Ok(())
            })
            .unwrap();
    }
}

/// A Rust integer becomes a number only when the number holds it exactly: 2^53 does, 2^53 + 1
/// does not and is an error with the documented message, never the nearby number.
#[test]
fn integer_results_are_exact_or_an_error() {
    let runtime = Runtime::new().unwrap();
    let stack = runtime.stack();
    let exact = stack.with_frame(|frame| (1u64 << 53).push_into(frame)?.read::<f64>()).unwrap();
    assert_eq!(exact, 2f64.powi(53));
    for refused in [(1u64 << 53) + 1, u64::MAX] {
        let error = stack.with_frame(|frame| refused.push_into(frame).map(|_| ())).unwrap_err().to_string();
        assert!(error.contains("cannot be represented exactly"), "{refused}: {error}");
    }
    let error = stack.with_frame(|frame| (-(1i64 << 53) - 1).push_into(frame).map(|_| ())).unwrap_err().to_string();
    assert!(error.contains("cannot be represented exactly"), "{error}");
}

/// A `Bits64` direct field reads the same through the direct path, a bound method and native
/// code, with its kind's `typeof`. The boxed carrier cannot produce one in a direct getter.
#[cfg(not(l3i_carrier = "boxed"))]
#[test]
fn a_bits64_direct_field_reads_alike_on_every_path() {
    use l3i::direct::field::{DirectField, FieldValue};
    use l3i::extension::{Extension, ExtensionDescriptor};
    use l3i::userdata::{Owned, Userdata};

    #[derive(Clone)]
    struct Id(std::cell::Cell<u64>);
    // SAFETY: plain data, no Lua references.
    unsafe impl Userdata for Id {
        const NAME: &'static str = "dream.tests.Id";
    }
    struct Raw;
    impl DirectField<Id> for Raw {
        fn get(id: &Id) -> FieldValue {
            FieldValue::Bits64(id.0.get())
        }
    }
    struct Ids;
    impl Extension for Ids {
        fn id(&self) -> &'static str {
            "dream.ids"
        }
        fn describe(&self, d: &mut ExtensionDescriptor) -> l3i::Result<()> {
            let mut id = d.userdata::<Id>("dream.tests.Id");
            id.tag(l3i::extension::TagPolicy::Required);
            id.field::<Raw>("raw").signature("Bits64");
            id.method("value", |id: &Id| Bits64(id.0.get())).signature("(self): Bits64");
            id.method("set", |id: &Id, value: Bits64| id.0.set(value.0)).signature("(self, value: Bits64)");
            d.module("@dream/ids")
                .function("new", || Owned(Id(std::cell::Cell::new(0x8000_0000_0000_0042))))
                .signature("() -> dream_tests_Id");
            Ok(())
        }
    }
    let plan = RuntimePlan::builder()
        .policy(RuntimePolicy::new().compat_global("@dream/ids", "ids").compat_global("@dream/bits64", "bits64"))
        .extension(Ids)
        .finalize()
        .unwrap();
    let runtime = Runtime::from_plan(&plan).unwrap();
    runtime
        .exec(
            "local id = ids.new() \
             assert(id.raw == id:value() and rawequal(id.raw, id:value()), 'direct and bound agree') \
             assert(id.raw == bits64.fromHex('8000000000000042'), 'all 64 bits') \
             assert(typeof(id.raw) == typeof(bits64.fromNumber(1)), typeof(id.raw)) \
             id:set(bits64.fromHex('ffffffffffffffff')) assert(id.raw == bits64.fromHex('ffffffffffffffff'), 'after a change')",
        )
        .unwrap();
    #[cfg(feature = "jit")]
    {
        use l3i::source::LoadScope;
        let read = {
            let stack = runtime.stack();
            let source = "--!native\nreturn function(id) local a = id.raw local b = id:value() return a == b, a end\n";
            let chunk = stack.load_source("=native_field", source, &runtime.compile_options()).unwrap();
            chunk.invoke::<l3i::value::Function, _>(&stack, ()).unwrap()
        };
        runtime.set_global("nativeRead", &read).unwrap();
        runtime
            .exec("local id = ids.new() local same, raw = nativeRead(id) assert(same and raw == bits64.fromHex('8000000000000042'), 'native path')")
            .unwrap();
    }
}
