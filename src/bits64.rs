//! `@dream/bits64`: the opaque 64-bit value ([`Bits64`]) as scripts make and take it apart.
//!
//! A `Bits64` is a wide packed scalar: all 64 bits, equal and a table key by value, `typeof`
//! `Bits64`, and no arithmetic. It is what a digest, a client id or a protocol id is to a script,
//! and what `bytes.readi64be` and `bytes.readVarint64` return. This module converts it to and from
//! numbers (exact, never rounded: a value above 2^53 is an error, never a nearby number), two
//! 32-bit halves, hex, and eight bytes of a buffer in either byte order. Luau 0.747 has no 64-bit
//! integer type; a 64-bit quantity a script computes with lives in a buffer.

use crate::convert::{Bits64, BufferView, Exact};
use crate::error::{Error, Result};
use crate::extension::{Extension, ExtensionDescriptor};

/// The extension id.
pub const EXTENSION_ID: &str = "dream.bits64";
/// The module path.
pub const MODULE: &str = "@dream/bits64";

/// The largest whole number below which every whole number is an exact Lua number.
const EXACT_LIMIT: u64 = 1 << 53;

/// The `dream.bits64` extension.
#[derive(Clone, Copy, Debug, Default)]
pub struct Bits64Extension;

fn big_endian(order: Option<&str>, name: &str) -> Result<bool> {
    match order {
        None | Some("little") => Ok(false),
        Some("big") => Ok(true),
        Some(other) => Err(Error::runtime(format!("{name}: byte order must be 'little' or 'big', got '{other}'"))),
    }
}

fn offset(buffer: &BufferView<'_>, offset: Exact<i64>, name: &str) -> Result<usize> {
    usize::try_from(offset.0)
        .ok()
        .filter(|start| start.checked_add(8).is_some_and(|end| end <= buffer.len()))
        .ok_or_else(|| Error::runtime(format!("{name}: 8 bytes at offset {} are outside the buffer", offset.0)))
}

/// The value as an exact number: an error above 2^53.
fn to_number(bits: Bits64) -> Result<f64> {
    if bits.0 > EXACT_LIMIT {
        return Err(Error::runtime(format!(
            "bits64.toNumber: {} is above 2^53, where a number is no longer exact",
            bits.0
        )));
    }
    Ok(bits.0 as f64)
}

fn from_hex(text: &str) -> Result<Bits64> {
    let digits = text.strip_prefix("0x").or_else(|| text.strip_prefix("0X")).unwrap_or(text);
    if digits.is_empty() || digits.len() > 16 || !digits.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(Error::runtime(format!("bits64.fromHex: '{text}' is not one to sixteen hex digits")));
    }
    u64::from_str_radix(digits, 16).map(Bits64).map_err(|error| Error::runtime(format!("bits64.fromHex: {error}")))
}

impl Extension for Bits64Extension {
    fn id(&self) -> &'static str {
        EXTENSION_ID
    }

    fn describe(&self, d: &mut ExtensionDescriptor) -> Result<()> {
        d.module(MODULE)
            .doc("Opaque 64-bit values: exact conversions, halves, hex and bytes; no arithmetic.")
            .function("fromNumber", |n: Exact<u64>| Bits64(n.0))
            .signature("(n: number) -> Bits64")
            .doc("A whole number in [0, 2^64) as its 64 bits; a fraction or a negative number is an error.")
            .function("toNumber", to_number)
            .signature("(value: Bits64) -> number")
            .doc("The value as a number; an error above 2^53, where a number is no longer exact.")
            .function("fromParts", |hi: Exact<u32>, lo: Exact<u32>| Bits64((u64::from(hi.0) << 32) | u64::from(lo.0)))
            .signature("(hi: number, lo: number) -> Bits64")
            .doc("The value whose high and low 32 bits are hi and lo.")
            .function("parts", |value: Bits64| ((value.0 >> 32) as u32, value.0 as u32))
            .signature("(value: Bits64) -> (number, number)")
            .doc("The high and low 32 bits.")
            .function("hex", |value: Bits64| format!("{:016x}", value.0))
            .signature("(value: Bits64) -> string")
            .doc("Sixteen lowercase hex digits.")
            .function("fromHex", |text: &str| from_hex(text))
            .signature("(text: string) -> Bits64")
            .doc("One to sixteen hex digits, with or without 0x.")
            .function("read", |buffer: BufferView<'_>, at: Exact<i64>, order: Option<&str>| -> Result<Bits64> {
                let big = big_endian(order, "bits64.read")?;
                let start = offset(&buffer, at, "bits64.read")?;
                let mut bytes = [0u8; 8];
                buffer.read(start, &mut bytes)?;
                Ok(Bits64(if big { u64::from_be_bytes(bytes) } else { u64::from_le_bytes(bytes) }))
            })
            .signature("(buffer: buffer, offset: number, order: (\"little\" | \"big\")?) -> Bits64")
            .doc("Eight bytes at offset, little-endian unless order is 'big'.")
            .function(
                "write",
                |buffer: BufferView<'_>, at: Exact<i64>, value: Bits64, order: Option<&str>| -> Result<()> {
                    let big = big_endian(order, "bits64.write")?;
                    let start = offset(&buffer, at, "bits64.write")?;
                    buffer.write(start, &if big { value.0.to_be_bytes() } else { value.0.to_le_bytes() })
                },
            )
            .signature("(buffer: buffer, offset: number, value: Bits64, order: (\"little\" | \"big\")?) -> ()")
            .doc("Writes the eight bytes at offset, little-endian unless order is 'big'.");
        Ok(())
    }
}
