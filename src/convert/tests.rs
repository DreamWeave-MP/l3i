//! Ported from `testluavalue.cpp` (NumericConversions, FiniteFloat) and `testluaustack.cpp`
//! (NumericStringsNeverConvertToIntegral, IntegralConversionRejectsInvalidNumbersAndRoundsFractions,
//! IntegerKindVmSemanticsProbe, IntegerVersusNumberPreservedThroughTableReference).

use super::*;
use crate::error::Error;
use crate::runtime::Runtime;
use crate::stack::{Frame, Type};

fn with_frame(body: impl FnOnce(&Frame<'_>)) {
    let runtime = Runtime::new().unwrap();
    let stack = runtime.stack();
    let frame = stack.frame();
    body(&frame);
}

#[test]
fn integers_convert_by_range_and_numbers_by_rounding() {
    with_frame(|frame| {
        let integer = frame.push(&Integer(42)).unwrap();
        assert_eq!(integer.read::<f64>().unwrap(), 42.0);
        assert!(integer.read::<String>().is_err());
        assert!(integer.is_integer());
        assert!(integer.is::<i32>() && integer.is::<u8>() && integer.is::<f32>() && integer.is::<Option<f32>>());
        assert_eq!(integer.read::<Integer>().unwrap(), Integer(42));

        let precise: i64 = (1 << 62) + (1 << 38) + 1;
        let precise_value = frame.push(&Integer(precise)).unwrap();
        assert_eq!(precise_value.read::<f32>().unwrap(), precise as f32);
        assert_eq!(precise_value.read::<i64>().unwrap(), precise);

        let negative = frame.push(&Integer(-1)).unwrap();
        assert!(!negative.is::<u8>());
        assert_eq!(
            negative.read::<u8>().unwrap_err(),
            Error::runtime(format!("Lua stack index {}: expected integer, got integer", negative.index()))
        );
        let too_large = frame.push(&Integer(256)).unwrap();
        assert!(!too_large.is::<u8>());
        assert!(too_large.read::<u8>().is_err());

        assert_eq!(frame.push_number(1.0).read::<i32>().unwrap(), 1);
        assert_eq!(frame.push_number(1.5).read::<i32>().unwrap(), 2);
        assert_eq!(frame.push_number(-1.5).read::<i32>().unwrap(), -2);
        assert_eq!(frame.push_number(-128.4).read::<i8>().unwrap(), -128);
        assert_eq!(frame.push_number(127.4).read::<i8>().unwrap(), 127);
        assert_eq!(frame.push_number(4.0).read::<i32>().unwrap(), 4);

        for value in [-129.5, -128.5, 127.5, 128.5] {
            let invalid = frame.push_number(value);
            assert!(!invalid.is::<i8>(), "{value}");
            assert!(invalid.read::<i8>().unwrap_err().to_string().ends_with("expected integer, got number"));
        }
        for value in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY, 1e300] {
            let invalid = frame.push_number(value);
            assert!(!invalid.is::<i32>(), "{value}");
            let error = invalid.read::<i32>().unwrap_err().to_string();
            assert!(error.ends_with("expected integer, got number"), "{error}");
            assert!(invalid.read::<i64>().unwrap_err().to_string().ends_with("expected integer, got number"));
        }

        let two_63 = 2f64.powi(63);
        assert!(frame.push_number(two_63).read::<i64>().is_err());
        assert!(frame.push_number(two_63.next_down()).read::<i64>().is_ok());
        let two_64 = 2f64.powi(64);
        assert!(frame.push_number(two_64).read::<u64>().is_err());
        assert!(frame.push_number(two_64.next_down()).read::<u64>().is_ok());

        let huge = frame.push_number(f64::MAX);
        assert!(!huge.is::<f32>());
        assert_eq!(
            huge.read::<f32>().unwrap_err(),
            Error::runtime("Lua number does not fit destination floating-point type")
        );

        let before = frame.len();
        assert_eq!(
            frame.push(&u64::MAX).unwrap_err(),
            Error::runtime("Integer cannot be represented exactly as a Lua number")
        );
        assert_eq!(frame.len(), before, "a rejected push leaves nothing behind");
        assert_eq!(frame.push(&(1u64 << 60)).unwrap().read::<u64>().unwrap(), 1 << 60);
        assert_eq!(frame.push(&(i64::MIN)).unwrap().type_of(), Type::Number);
        assert!(frame.push(&((1i64 << 53) + 1)).is_err());
    });
}

#[test]
fn floats_keep_nan_and_infinity_but_reject_out_of_range_finite_values() {
    with_frame(|frame| {
        assert!(frame.push_number(f64::INFINITY).is::<f32>());
        assert!(frame.push_number(f64::INFINITY).read::<f32>().unwrap().is_infinite());
        assert!(frame.push_number(f64::NAN).is::<f32>());
        assert!(frame.push_number(f64::NAN).read::<f32>().unwrap().is_nan());
        assert!(frame.push_number(f64::MAX).read::<f32>().is_err());
        assert_eq!(frame.push(&2.5f32).unwrap().read::<f32>().unwrap(), 2.5);
        assert_eq!(frame.push(&2.5f64).unwrap().read::<f64>().unwrap(), 2.5);
    });
}

#[test]
fn numeric_strings_never_convert_to_integral_and_numbers_never_to_strings() {
    with_frame(|frame| {
        for text in ["5", " 5 ", "0x10"] {
            let value = frame.push_string(text);
            assert!(!value.is::<i32>(), "{text}");
            assert!(value.read::<i32>().unwrap_err().to_string().ends_with("expected number, got string"));
            assert!(value.read::<i64>().unwrap_err().to_string().ends_with("expected number, got string"));
            assert!(value.read::<f64>().unwrap_err().to_string().ends_with("expected number, got string"));
        }
        let number = frame.push_number(5.0);
        assert!(number.read::<String>().unwrap_err().to_string().ends_with("expected string, got number"));
        assert!(number.read::<&str>().is_err());
        assert!(number.read::<bool>().unwrap_err().to_string().ends_with("expected boolean, got number"));
        let text = frame.push_string("héllo");
        assert_eq!(text.read::<&str>().unwrap(), "héllo");
        assert_eq!(text.read::<&[u8]>().unwrap(), "héllo".as_bytes());
        assert_eq!(text.read::<String>().unwrap(), "héllo");
        let bytes = frame.push(&[0xffu8, 0xfe][..]).unwrap();
        assert!(bytes.is::<&[u8]>() && !bytes.is::<&str>());
        assert_eq!(bytes.read::<&str>().unwrap_err(), Error::runtime("Lua string is not valid UTF-8"));
        assert_eq!(frame.push(&String::new()).unwrap().read::<&str>().unwrap(), "");
    });
}

#[test]
fn booleans_are_exact_and_options_map_nil() {
    with_frame(|frame| {
        assert!(frame.push(&true).unwrap().read::<bool>().unwrap());
        assert!(frame.push_number(1.0).read::<bool>().is_err());
        assert_eq!(frame.push(&None::<i32>).unwrap().type_of(), Type::Nil);
        assert_eq!(frame.push_nil().read::<Option<i32>>().unwrap(), None);
        assert_eq!(frame.push(&Some(3i32)).unwrap().read::<Option<i32>>().unwrap(), Some(3));
        assert!(frame.push_string("x").read::<Option<i32>>().is_err());
        assert!(frame.at(500).read::<Option<i32>>().is_err(), "absence is not nil");
        assert!(frame.at(500).read::<crate::stack::ValueView<'_>>().is_err());
        assert!(frame.push_nil().read::<crate::stack::ValueView<'_>>().is_ok());
    });
}

#[test]
fn integer_kind_vm_semantics_probe() {
    // Pins this Luau release's integer semantics: cross-kind equality is false, same-kind
    // compares payloads, arithmetic with a number errors. Re-verify on any Luau bump.
    let runtime = Runtime::new().unwrap();
    let stack = runtime.stack();
    {
        let frame = stack.frame();
        frame.push(&Integer(5)).unwrap();
        frame.set_global("int_five").unwrap();
        frame.push(&Integer(5)).unwrap();
        frame.set_global("int_five_again").unwrap();
    }
    runtime.exec("assert(int_five ~= 5)").unwrap();
    runtime.exec("assert(int_five == int_five_again)").unwrap();
    let error = runtime.exec("return int_five + 1").unwrap_err().to_string();
    assert!(error.contains("attempt to perform arithmetic"), "{error}");
}

#[test]
fn integer_versus_number_is_preserved_through_a_table() {
    let runtime = Runtime::new().unwrap();
    let stack = runtime.stack();
    let table = crate::value::Table::new(&stack, 0, 4).unwrap();
    {
        let frame = stack.frame();
        let view = table.push_to(&frame).unwrap();
        frame.push(&Integer(i64::MIN)).unwrap();
        view.raw_set(&frame, "min").unwrap();
        frame.push(&Integer(i64::MAX)).unwrap();
        view.raw_set(&frame, "max").unwrap();
        frame.push(&7i64).unwrap();
        view.raw_set(&frame, "plain").unwrap();
        frame.push(&7.0).unwrap();
        view.raw_set(&frame, "real").unwrap();
    }
    runtime.collect_garbage();
    // A copied reference owns an independent pin that survives collection.
    let copy = table.clone();
    drop(table);
    runtime.collect_garbage();
    let frame = stack.frame();
    let view = copy.push_to(&frame).unwrap();
    let min = view.raw_get(&frame, "min").unwrap();
    assert_eq!(min.type_of(), Type::Integer);
    assert_eq!(min.read::<i64>().unwrap(), i64::MIN);
    assert_eq!(view.raw_get(&frame, "max").unwrap().read::<i64>().unwrap(), i64::MAX);
    assert_eq!(view.raw_get(&frame, "plain").unwrap().type_of(), Type::Number);
    assert_eq!(view.raw_get(&frame, "real").unwrap().type_of(), Type::Number);
}

#[test]
fn vectors_and_buffers_are_first_class() {
    with_frame(|frame| {
        let v = frame.push(&Vector3::new(1.0, 2.5, -3.0)).unwrap();
        assert!(v.is_vector() && v.is::<Vector3>());
        assert_eq!(v.read::<Vector3>().unwrap(), Vector3::new(1.0, 2.5, -3.0));
        assert!(v.read::<f64>().unwrap_err().to_string().ends_with("expected number, got vector"));
        assert!(
            frame.push_number(1.0).read::<Vector3>().unwrap_err().to_string().ends_with("expected vector, got number")
        );

        let buffer = new_buffer(frame, 16).unwrap();
        assert_eq!(buffer.len(), 16);
        assert_eq!(buffer.to_vec(), vec![0; 16]);
        buffer.write_f32x3(4, Vector3::new(1.0, 2.0, 3.0)).unwrap();
        assert_eq!(buffer.read_f32x3(4).unwrap(), Vector3::new(1.0, 2.0, 3.0));
        assert_eq!(buffer.read_f32(8).unwrap(), 2.0);
        assert_eq!(
            buffer.write_f32x3(5, Vector3::default()).unwrap_err(),
            Error::runtime("buffer access out of bounds")
        );
        assert_eq!(buffer.read_u8(16).unwrap_err(), Error::runtime("buffer access out of bounds"));
        assert!(buffer.write(usize::MAX, &[1]).is_err(), "offset overflow is out of bounds, not a wrap");
        buffer.write_u8(15, 0xAB).unwrap();
        assert_eq!(buffer.read_u8(15).unwrap(), 0xAB);

        let slot = frame.top_value();
        assert!(slot.is_buffer() && slot.is::<BufferView<'_>>());
        let again = slot.read::<BufferView<'_>>().unwrap();
        assert_eq!(again.read_u8(15).unwrap(), 0xAB);
        assert!(frame.push_number(1.0).read::<BufferView<'_>>().is_err());
    });
}

#[test]
fn buffers_written_from_rust_are_visible_to_scripts_and_vice_versa() {
    let runtime = Runtime::new().unwrap();
    runtime.exec("shared = buffer.create(12) buffer.writef32(shared, 0, 9.5)").unwrap();
    let stack = runtime.stack();
    let frame = stack.frame();
    let global = crate::value::Value::get_global(&frame, "shared").unwrap();
    let buffer = global.push_to(&frame).unwrap().read::<BufferView<'_>>().unwrap();
    assert_eq!(buffer.read_f32(0).unwrap(), 9.5);
    buffer.write_f32x3(0, Vector3::new(1.0, 2.0, 3.0)).unwrap();
    drop(frame);
    runtime
        .exec("assert(buffer.readf32(shared, 0) == 1 and buffer.readf32(shared, 4) == 2 and buffer.readf32(shared, 8) == 3)")
        .unwrap();
}
