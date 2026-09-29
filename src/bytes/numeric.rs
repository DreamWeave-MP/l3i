//! The widths and byte orders Luau's `buffer` library lacks: big-endian 16, 32 and 64-bit
//! integers and floats, 24-bit integers in either order, and IEEE 754 half floats.
//!
//! Each read exists as a module function over a `buffer | string` and as a method on the
//! [`Math`] receiver over a `buffer`; the writes take a `buffer`. The receiver's integer forms
//! lower to native code under `jit` ([`super::lowering`]); its float forms and the module
//! functions run through the binder. Values written through a `u`/`i` form are truncated to
//! the width, as `buffer.writeu16` truncates; 64-bit values are Luau integers.

use crate::convert::{BufferView, BytesView, Exact, Integer};
use crate::error::Result;
use crate::extension::{ModuleDecl, UserdataBuilder};

use super::{Math, span};

/// An IEEE 754 binary16 as an `f32`.
#[must_use]
pub fn f16_to_f32(bits: u16) -> f32 {
    let sign = u32::from(bits >> 15) << 31;
    let exponent = u32::from((bits >> 10) & 0x1F);
    let mantissa = u32::from(bits & 0x3FF);
    let value = match exponent {
        0 => {
            // Zero, or a subnormal: mantissa * 2^-24, exact in an f32.
            mantissa as f32 * (1.0 / 16_777_216.0)
        }
        31 => {
            if mantissa == 0 {
                f32::INFINITY
            } else {
                f32::NAN
            }
        }
        _ => f32::from_bits(((exponent + 112) << 23) | (mantissa << 13)),
    };
    f32::from_bits(value.to_bits() | sign)
}

/// An `f32` as IEEE 754 binary16, rounded to nearest even; values past the half range become
/// infinities.
#[must_use]
pub fn f32_to_f16(value: f32) -> u16 {
    let bits = value.to_bits();
    let sign = ((bits >> 16) & 0x8000) as u16;
    let exponent = ((bits >> 23) & 0xFF) as i32;
    let mantissa = bits & 0x7F_FFFF;
    if exponent == 0xFF {
        // Infinity or NaN; keep a NaN a NaN.
        return sign | 0x7C00 | if mantissa != 0 { 0x200 } else { 0 };
    }
    let unbiased = exponent - 127 + 15;
    if unbiased >= 0x1F {
        return sign | 0x7C00;
    }
    if unbiased <= 0 {
        if unbiased < -10 {
            return sign;
        }
        // Subnormal half: shift the implicit one in and round.
        let full = mantissa | 0x80_0000;
        let shift = (14 - unbiased) as u32;
        let half = full >> shift;
        let remainder = full & ((1 << shift) - 1);
        let midpoint = 1 << (shift - 1);
        let rounded = if remainder > midpoint || (remainder == midpoint && half & 1 == 1) { half + 1 } else { half };
        return sign | rounded as u16;
    }
    let half = ((unbiased as u32) << 10) | (mantissa >> 13);
    let remainder = mantissa & 0x1FFF;
    let rounded = if remainder > 0x1000 || (remainder == 0x1000 && half & 1 == 1) { half + 1 } else { half };
    sign | rounded as u16
}

macro_rules! reads {
    ($(($name:ident, $what:literal, $n:literal, $out:ty, $convert:expr)),* $(,)?) => {$(
        fn $name(source: BytesView<'_>, offset: Exact<i64>) -> Result<$out> {
            let start = span($what, source.len(), offset, $n)?;
            let mut raw = [0u8; $n];
            source.read(start, &mut raw)?;
            let convert: fn([u8; $n]) -> $out = $convert;
            Ok(convert(raw))
        }
    )*};
}

reads! {
    (read_u16be, "bytes.readu16be", 2, f64, |b| f64::from(u16::from_be_bytes(b))),
    (read_i16be, "bytes.readi16be", 2, f64, |b| f64::from(i16::from_be_bytes(b))),
    (read_u24le, "bytes.readu24", 3, f64, |b| f64::from(u32::from_le_bytes([b[0], b[1], b[2], 0]))),
    (read_i24le, "bytes.readi24", 3, f64, |b| f64::from(i32::from_le_bytes([b[0], b[1], b[2], 0]) << 8 >> 8)),
    (read_u24be, "bytes.readu24be", 3, f64, |b| f64::from(u32::from_be_bytes([0, b[0], b[1], b[2]]))),
    (read_i24be, "bytes.readi24be", 3, f64, |b| f64::from(i32::from_be_bytes([0, b[0], b[1], b[2]]) << 8 >> 8)),
    (read_u32be, "bytes.readu32be", 4, f64, |b| f64::from(u32::from_be_bytes(b))),
    (read_i32be, "bytes.readi32be", 4, f64, |b| f64::from(i32::from_be_bytes(b))),
    (read_f32be, "bytes.readf32be", 4, f64, |b| f64::from(f32::from_be_bytes(b))),
    (read_f64be, "bytes.readf64be", 8, f64, f64::from_be_bytes),
    (read_i64be, "bytes.readi64be", 8, Integer, |b| Integer(i64::from_be_bytes(b))),
    (read_f16, "bytes.readf16", 2, f64, |b| f64::from(f16_to_f32(u16::from_le_bytes(b)))),
    (read_f16be, "bytes.readf16be", 2, f64, |b| f64::from(f16_to_f32(u16::from_be_bytes(b)))),
}

/// A number to an integer of the width, truncating as `buffer.writeu32` does: through `i64`,
/// then the low bits.
#[inline]
fn truncate(value: f64) -> i64 {
    value as i64
}

macro_rules! writes {
    ($(($name:ident, $what:literal, $n:literal, $in:ty, $convert:expr)),* $(,)?) => {$(
        fn $name(buffer: BufferView<'_>, offset: Exact<i64>, value: $in) -> Result<()> {
            let start = span($what, buffer.len(), offset, $n)?;
            let convert: fn($in) -> [u8; $n] = $convert;
            buffer.write(start, &convert(value))
        }
    )*};
}

writes! {
    (write_u16be, "bytes.writeu16be", 2, f64, |v| (truncate(v) as u16).to_be_bytes()),
    (write_i16be, "bytes.writei16be", 2, f64, |v| (truncate(v) as u16).to_be_bytes()),
    (write_u24le, "bytes.writeu24", 3, f64, |v| { let b = (truncate(v) as u32).to_le_bytes(); [b[0], b[1], b[2]] }),
    (write_i24le, "bytes.writei24", 3, f64, |v| { let b = (truncate(v) as u32).to_le_bytes(); [b[0], b[1], b[2]] }),
    (write_u24be, "bytes.writeu24be", 3, f64, |v| { let b = (truncate(v) as u32).to_be_bytes(); [b[1], b[2], b[3]] }),
    (write_i24be, "bytes.writei24be", 3, f64, |v| { let b = (truncate(v) as u32).to_be_bytes(); [b[1], b[2], b[3]] }),
    (write_u32be, "bytes.writeu32be", 4, f64, |v| (truncate(v) as u32).to_be_bytes()),
    (write_i32be, "bytes.writei32be", 4, f64, |v| (truncate(v) as u32).to_be_bytes()),
    (write_f32be, "bytes.writef32be", 4, f64, |v| (v as f32).to_be_bytes()),
    (write_f64be, "bytes.writef64be", 8, f64, f64::to_be_bytes),
    (write_i64be, "bytes.writei64be", 8, Integer, |v| v.0.to_be_bytes()),
    (write_f16, "bytes.writef16", 2, f64, |v| f32_to_f16(v as f32).to_le_bytes()),
    (write_f16be, "bytes.writef16be", 2, f64, |v| f32_to_f16(v as f32).to_be_bytes()),
}

pub(crate) fn describe_module(module: &mut ModuleDecl) {
    macro_rules! read {
        ($name:literal, $f:ident, $ty:literal) => {
            module.function($name, $f).signature(concat!("(source: buffer | string, offset: number) -> ", $ty));
        };
    }
    macro_rules! write {
        ($name:literal, $f:ident, $ty:literal) => {
            module.function($name, $f).signature(concat!("(target: buffer, offset: number, value: ", $ty, ") -> ()"));
        };
    }
    read!("readu16be", read_u16be, "number");
    read!("readi16be", read_i16be, "number");
    read!("readu24", read_u24le, "number");
    read!("readi24", read_i24le, "number");
    read!("readu24be", read_u24be, "number");
    read!("readi24be", read_i24be, "number");
    read!("readu32be", read_u32be, "number");
    read!("readi32be", read_i32be, "number");
    read!("readf32be", read_f32be, "number");
    read!("readf64be", read_f64be, "number");
    read!("readi64be", read_i64be, "integer");
    read!("readf16", read_f16, "number");
    read!("readf16be", read_f16be, "number");
    write!("writeu16be", write_u16be, "number");
    write!("writei16be", write_i16be, "number");
    write!("writeu24", write_u24le, "number");
    write!("writei24", write_i24le, "number");
    write!("writeu24be", write_u24be, "number");
    write!("writei24be", write_i24be, "number");
    write!("writeu32be", write_u32be, "number");
    write!("writei32be", write_i32be, "number");
    write!("writef32be", write_f32be, "number");
    write!("writef64be", write_f64be, "number");
    write!("writei64be", write_i64be, "integer");
    write!("writef16", write_f16, "number");
    write!("writef16be", write_f16be, "number");
}

pub(crate) fn describe_receiver(math: &mut UserdataBuilder<'_, Math>) {
    macro_rules! read {
        ($name:literal, $f:ident, $ty:literal) => {
            math.method($name, |_: &Math, b: BufferView<'_>, o: Exact<i64>| $f(BytesView::Buffer(b), o))
                .signature(concat!("(self, source: buffer, offset: number): ", $ty));
        };
    }
    macro_rules! write {
        ($name:literal, $f:ident, $ty:literal) => {
            math.method($name, |_: &Math, b: BufferView<'_>, o: Exact<i64>, v| $f(b, o, v)).signature(concat!(
                "(self, target: buffer, offset: number, value: ",
                $ty,
                "): ()"
            ));
        };
    }
    read!("readu16be", read_u16be, "number");
    read!("readi16be", read_i16be, "number");
    read!("readu24", read_u24le, "number");
    read!("readi24", read_i24le, "number");
    read!("readu24be", read_u24be, "number");
    read!("readi24be", read_i24be, "number");
    read!("readu32be", read_u32be, "number");
    read!("readi32be", read_i32be, "number");
    read!("readf32be", read_f32be, "number");
    read!("readf64be", read_f64be, "number");
    read!("readi64be", read_i64be, "integer");
    read!("readf16", read_f16, "number");
    read!("readf16be", read_f16be, "number");
    write!("writeu16be", write_u16be, "number");
    write!("writei16be", write_i16be, "number");
    write!("writeu24", write_u24le, "number");
    write!("writei24", write_i24le, "number");
    write!("writeu24be", write_u24be, "number");
    write!("writei24be", write_i24be, "number");
    write!("writeu32be", write_u32be, "number");
    write!("writei32be", write_i32be, "number");
    write!("writef32be", write_f32be, "number");
    write!("writef64be", write_f64be, "number");
    write!("writei64be", write_i64be, "integer");
    write!("writef16", write_f16, "number");
    write!("writef16be", write_f16be, "number");
}

#[cfg(test)]
mod tests {
    use super::{f16_to_f32, f32_to_f16};

    #[test]
    fn half_floats_round_trip_every_finite_value() {
        for bits in 0..=u16::MAX {
            let value = f16_to_f32(bits);
            if value.is_nan() {
                assert_eq!(bits & 0x7C00, 0x7C00);
                continue;
            }
            assert_eq!(f32_to_f16(value), bits, "bits {bits:#06x} -> {value}");
        }
        assert_eq!(f16_to_f32(0x3C00), 1.0);
        assert_eq!(f16_to_f32(0xC000), -2.0);
        assert_eq!(f16_to_f32(0x0001), 5.960_464_5e-8);
        assert_eq!(f16_to_f32(0x7BFF), 65504.0);
        assert_eq!(f32_to_f16(65520.0), 0x7C00, "rounds up to infinity");
        assert_eq!(f32_to_f16(1.0 + 1.0 / 2048.0), 0x3C00, "ties to even");
        assert_eq!(f32_to_f16(1.0 + 3.0 / 2048.0), 0x3C02, "ties to even, odd side");
        assert_eq!(f32_to_f16(1e-9), 0);
    }
}
