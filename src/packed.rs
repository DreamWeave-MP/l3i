//! Packed values: fixed-size byte layouts for buffers and semantic 64-bit scalars.
//!
//! [`BufferPack`] is the reusable mechanism of `L3I_EXTENSION_RUNTIME_ARCHITECTURE.md` §32: a
//! domain type states its byte size and how to read and write itself, and l3i owns the safe
//! crossing into and out of Luau buffers (bounds checked, one copy, no per-field calls).
//!
//! [`PackedScalar`] (§28.1) is a value that physically occupies one Luau integer. The top byte
//! is a discriminator (a 4-bit kind and 4 bits the kind may use for flags), the low 56 bits the
//! payload. The kind is checked on every read, so untyped script code handing the wrong integer
//! to a native operation fails with a type error instead of decoding garbage.
//! [`crate::quat::Quaternion`] and [`crate::quat::AnimationKey`] are the first kinds: a rotation
//! in 56 bits, with or without four bits of side data.

use crate::convert::{BufferView, FromView, Integer, Push};
use crate::error::{Error, Result};
use crate::stack::{Scope, Type, ValueView};

/// A value with a fixed little-endian byte layout inside a Luau buffer.
pub trait BufferPack: Sized {
    /// The encoded size in bytes.
    const SIZE: usize;
    /// Decodes from exactly `SIZE` bytes.
    fn read_from(bytes: &[u8]) -> Result<Self>;
    /// Encodes into exactly `SIZE` bytes.
    fn write_to(&self, bytes: &mut [u8]) -> Result<()>;
}

impl BufferView<'_> {
    /// Reads a packed `T` at `offset`, bounds checked once.
    pub fn read_packed<T: BufferPack>(&self, offset: usize) -> Result<T> {
        let range = self.range(offset, T::SIZE)?;
        range.with_bytes(T::read_from)
    }

    /// Writes a packed `T` at `offset`, bounds checked once.
    pub fn write_packed<T: BufferPack>(&self, offset: usize, value: &T) -> Result<()> {
        let mut range = self.range(offset, T::SIZE)?;
        range.with_bytes_mut(|bytes| value.write_to(bytes))
    }
}

macro_rules! primitive_pack {
    ($($t:ty),*) => {$(
        impl BufferPack for $t {
            const SIZE: usize = std::mem::size_of::<$t>();
            fn read_from(bytes: &[u8]) -> Result<Self> {
                let array: [u8; std::mem::size_of::<$t>()] =
                    bytes.try_into().map_err(|_| Error::runtime("buffer access out of bounds"))?;
                Ok(<$t>::from_le_bytes(array))
            }
            fn write_to(&self, bytes: &mut [u8]) -> Result<()> {
                if bytes.len() != Self::SIZE {
                    return Err(Error::runtime("buffer access out of bounds"));
                }
                bytes.copy_from_slice(&self.to_le_bytes());
                Ok(())
            }
        }
    )*};
}

primitive_pack!(u8, i8, u16, i16, u32, i32, u64, i64, f32, f64);

impl BufferPack for crate::convert::Vector3 {
    const SIZE: usize = 12;
    fn read_from(bytes: &[u8]) -> Result<Self> {
        if bytes.len() != 12 {
            return Err(Error::runtime("buffer access out of bounds"));
        }
        Ok(crate::convert::Vector3 {
            x: f32::read_from(&bytes[0..4])?,
            y: f32::read_from(&bytes[4..8])?,
            z: f32::read_from(&bytes[8..12])?,
        })
    }
    fn write_to(&self, bytes: &mut [u8]) -> Result<()> {
        if bytes.len() != 12 {
            return Err(Error::runtime("buffer access out of bounds"));
        }
        self.x.write_to(&mut bytes[0..4])?;
        self.y.write_to(&mut bytes[4..8])?;
        self.z.write_to(&mut bytes[8..12])
    }
}

/// Bits of a packed scalar's payload.
pub const PAYLOAD_BITS: u32 = 56;
/// The payload mask (low 56 bits).
pub const PAYLOAD_MASK: u64 = (1 << PAYLOAD_BITS) - 1;
/// Bits of the kind discriminator (top nibble).
pub const KIND_BITS: u32 = 4;
/// Bits the kind may use for flags (second nibble from the top).
pub const FLAG_BITS: u32 = 4;

/// A semantic value that lives in one Luau integer: 4-bit kind, 4-bit flags, 56-bit payload.
pub trait PackedScalar: Sized {
    /// The kind discriminator, `1..=15` (0 is reserved so a plain zero integer never passes).
    /// Kinds 1 to 4 are [`crate::quat::Quaternion`], [`crate::quat::AnimationKey`],
    /// [`crate::raster::Color`], and [`crate::raster::ClipRect`]; a host's own kinds are `5..=15`.
    const KIND: u8;
    /// The name used in type errors, e.g. `Quaternion`.
    const NAME: &'static str;
    /// The 56-bit payload and 4 flag bits of this value.
    fn pack(&self) -> (u64, u8);
    /// Rebuilds the value from its payload and flags; the kind has been checked already.
    fn unpack(payload: u64, flags: u8) -> Result<Self>;
}

/// The Luau integer carrying a packed scalar of kind `T`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Packed<T>(pub T);

/// Encodes `payload` and `flags` under `kind` as the integer bit pattern.
#[inline]
#[must_use]
pub fn encode(kind: u8, flags: u8, payload: u64) -> i64 {
    debug_assert!(kind != 0 && kind < 16 && flags < 16 && payload <= PAYLOAD_MASK);
    let bits = (u64::from(kind) << 60) | (u64::from(flags) << 56) | (payload & PAYLOAD_MASK);
    bits as i64
}

/// Splits an integer bit pattern into `(kind, flags, payload)`.
#[inline]
#[must_use]
pub fn decode(bits: i64) -> (u8, u8, u64) {
    let bits = bits as u64;
    ((bits >> 60) as u8, ((bits >> 56) & 0xF) as u8, bits & PAYLOAD_MASK)
}

impl<T: PackedScalar> Packed<T> {
    /// The integer bit pattern of this value.
    pub fn bits(&self) -> i64 {
        let (payload, flags) = self.0.pack();
        encode(T::KIND, flags, payload)
    }

    /// Decodes `bits`, checking the kind first.
    pub fn from_bits(bits: i64) -> Result<Self> {
        let (kind, flags, payload) = decode(bits);
        if kind != T::KIND {
            return Err(Error::runtime(format!("expected a packed {}, got kind {kind}", T::NAME)));
        }
        T::unpack(payload, flags).map(Packed)
    }
}

impl<'v, T: PackedScalar> FromView<'v> for Packed<T> {
    const EXPECTED: &'static str = T::NAME;

    #[inline]
    fn from_view(view: ValueView<'v>) -> Result<Self> {
        // One read for the tag and the payload; the kind check is on the bits.
        match crate::convert::read_integer64(view) {
            Some(bits) => Packed::from_bits(bits),
            None => Err(view.type_error(Type::Integer)),
        }
    }

    fn matches(view: ValueView<'v>) -> bool {
        crate::convert::read_integer64(view).is_some_and(|bits| decode(bits).0 == T::KIND)
    }
}

impl<T: PackedScalar> crate::bind::Param for Packed<T> {
    type Item<'c> = Packed<T>;
}

impl<'c, T: PackedScalar> crate::bind::ParamItem<'c> for Packed<T> {
    const KIND: crate::bind::ParamKind = crate::bind::ParamKind::Regular;
    const EXPECTED: &'static str = T::NAME;
    #[inline]
    fn read_slot(view: ValueView<'c>) -> Result<Self> {
        <Packed<T> as FromView<'c>>::from_view(view)
    }
    #[inline]
    fn matches(view: ValueView<'c>) -> bool {
        <Packed<T> as FromView<'c>>::matches(view)
    }
}

impl<T: PackedScalar> Push for Packed<T> {
    fn push_into<'s, S: Scope>(&self, scope: &'s S) -> Result<ValueView<'s>> {
        Integer(self.bits()).push_into(scope)
    }
}

impl<T: PackedScalar> crate::bind::Return for Packed<T> {
    fn push_results(self, call: &crate::bind::Call<'_>) -> Result<std::ffi::c_int> {
        self.push_into(call)?;
        Ok(1)
    }
}

impl<T: PackedScalar> BufferPack for Packed<T> {
    const SIZE: usize = 8;
    fn read_from(bytes: &[u8]) -> Result<Self> {
        Packed::from_bits(i64::read_from(bytes)?)
    }
    fn write_to(&self, bytes: &mut [u8]) -> Result<()> {
        self.bits().write_to(bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, PartialEq)]
    struct Probe(u64, u8);

    impl PackedScalar for Probe {
        const KIND: u8 = 3;
        const NAME: &'static str = "Probe";
        fn pack(&self) -> (u64, u8) {
            (self.0, self.1)
        }
        fn unpack(payload: u64, flags: u8) -> Result<Self> {
            Ok(Probe(payload, flags))
        }
    }

    #[test]
    fn round_trips_and_rejects_other_kinds() {
        let packed = Packed(Probe(PAYLOAD_MASK - 5, 9));
        let bits = packed.bits();
        assert_eq!(decode(bits), (3, 9, PAYLOAD_MASK - 5));
        assert_eq!(Packed::<Probe>::from_bits(bits).unwrap(), packed);
        let other = encode(4, 0, 1);
        assert!(Packed::<Probe>::from_bits(other).is_err());
        assert!(Packed::<Probe>::from_bits(0).is_err());
    }

    #[test]
    fn primitives_pack_little_endian() {
        let mut bytes = [0u8; 4];
        0x0403_0201u32.write_to(&mut bytes).unwrap();
        assert_eq!(bytes, [1, 2, 3, 4]);
        assert_eq!(u32::read_from(&bytes).unwrap(), 0x0403_0201);
        assert!(u32::read_from(&bytes[..3]).is_err());
    }
}
