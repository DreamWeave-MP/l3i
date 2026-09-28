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
//!
//! Kinds are a registry, not a convention. A kind number is what a packed integer means once
//! it is written to a file or a socket, so the numbers are fixed: `1..=4` are l3i's own and
//! never change ([`BUILTIN_KINDS`]); `5..=15` are a host's, chosen by the application. Every
//! runtime carries a table of the kinds it knows, filled from the plan (`ExtensionDescriptor::
//! packed`) or by [`crate::Runtime::register_packed`], and a `Packed<T>` crossing into or out
//! of a VM where `T` is not the registered owner of its kind fails with a logic error rather
//! than decoding another type's bits. A plan with two types on one kind does not finalize.

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

/// Packed values up to this size cross through a stack scratch; larger ones through a `Vec`.
const SCRATCH: usize = 64;

impl BufferView<'_> {
    /// Reads a packed `T` at `offset`, bounds checked once. The bytes are copied out first, so
    /// `read_from` never sees a slice of Luau's storage.
    pub fn read_packed<T: BufferPack>(&self, offset: usize) -> Result<T> {
        if T::SIZE <= SCRATCH {
            let mut scratch = [0u8; SCRATCH];
            self.read(offset, &mut scratch[..T::SIZE])?;
            T::read_from(&scratch[..T::SIZE])
        } else {
            let mut bytes = vec![0u8; T::SIZE];
            self.read(offset, &mut bytes)?;
            T::read_from(&bytes)
        }
    }

    /// Writes a packed `T` at `offset`, bounds checked once; `write_to` fills a scratch that is
    /// then copied in.
    pub fn write_packed<T: BufferPack>(&self, offset: usize, value: &T) -> Result<()> {
        self.check_packed(offset, T::SIZE)?;
        if T::SIZE <= SCRATCH {
            let mut scratch = [0u8; SCRATCH];
            value.write_to(&mut scratch[..T::SIZE])?;
            self.write(offset, &scratch[..T::SIZE])
        } else {
            let mut bytes = vec![0u8; T::SIZE];
            value.write_to(&mut bytes)?;
            self.write(offset, &bytes)
        }
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

/// The first kind a host may claim; `1..HOST_KIND_FIRST` are l3i's ([`BUILTIN_KINDS`]).
pub const HOST_KIND_FIRST: u8 = 5;
/// The last kind: the discriminator is four bits and 0 is reserved.
pub const LAST_KIND: u8 = 15;

/// A semantic value that lives in one Luau integer: 4-bit kind, 4-bit flags, 56-bit payload.
pub trait PackedScalar: Sized + 'static {
    /// The kind discriminator, `1..=15` (0 is reserved so a plain zero integer never passes).
    /// Kinds 1 to 4 are [`crate::quat::Quaternion`], [`crate::quat::AnimationKey`],
    /// [`crate::raster::Color`], and [`crate::raster::ClipRect`]; a host's own kinds are
    /// [`HOST_KIND_FIRST`]`..=`[`LAST_KIND`], registered per runtime (see the module docs).
    const KIND: u8;
    /// The name used in type errors, e.g. `Quaternion`.
    const NAME: &'static str;
    /// The 56-bit payload and 4 flag bits of this value.
    fn pack(&self) -> (u64, u8);
    /// Rebuilds the value from its payload and flags; the kind has been checked already.
    fn unpack(payload: u64, flags: u8) -> Result<Self>;
}

/// One registered kind: the number, the Rust type that owns it, and its name for messages.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PackedKind {
    pub kind: u8,
    pub type_id: std::any::TypeId,
    pub name: &'static str,
}

impl PackedKind {
    #[must_use]
    pub fn of<T: PackedScalar>() -> PackedKind {
        PackedKind { kind: T::KIND, type_id: std::any::TypeId::of::<T>(), name: T::NAME }
    }

    /// Whether this is one of l3i's own kinds.
    #[must_use]
    pub fn is_builtin(&self) -> bool {
        builtin_kinds().iter().any(|builtin| builtin.type_id == self.type_id)
    }

    /// Rejects kind numbers no registry accepts: 0, above 15, or one of l3i's for a host type.
    pub(crate) fn validate(&self) -> Result<()> {
        if self.kind == 0 || self.kind > LAST_KIND {
            return Err(Error::logic(format!("packed scalar {} declares kind {}, outside 1..=15", self.name, self.kind)));
        }
        if self.kind < HOST_KIND_FIRST && !self.is_builtin() {
            return Err(Error::logic(format!(
                "packed scalar {} declares kind {}, which belongs to l3i ({}); host kinds are {HOST_KIND_FIRST}..={LAST_KIND}",
                self.name,
                self.kind,
                builtin_kinds()[usize::from(self.kind) - 1].name
            )));
        }
        Ok(())
    }
}

/// l3i's own kinds, in kind order from 1: fixed for good, since packed integers are written
/// to files and sockets.
#[must_use]
pub fn builtin_kinds() -> [PackedKind; 4] {
    [
        PackedKind::of::<crate::quat::Quaternion>(),
        PackedKind::of::<crate::quat::AnimationKey>(),
        PackedKind::of::<crate::raster::Color>(),
        PackedKind::of::<crate::raster::ClipRect>(),
    ]
}

/// The kind numbers of [`builtin_kinds`], as documentation and as a compile-time anchor.
pub const BUILTIN_KINDS: [u8; 4] = [1, 2, 3, 4];

/// Fails unless `T` is the registered owner of its kind on the VM behind `state`.
#[inline]
fn check_registered<T: PackedScalar>(state: *mut crate::raw::ffi::lua_State) -> Result<()> {
    // SAFETY: `state` comes from a live scope.
    let registered = unsafe { crate::runtime::shared_for(state) }.and_then(|shared| shared.packed_kind_of(T::KIND));
    match registered {
        Some(owner) if owner.type_id == std::any::TypeId::of::<T>() => Ok(()),
        Some(owner) => Err(Error::logic(format!(
            "packed kind {} is registered to {} in this runtime, not {}",
            T::KIND,
            owner.name,
            T::NAME
        ))),
        None => Err(Error::logic(format!(
            "packed scalar {} (kind {}) is not registered in this runtime; declare it with ExtensionDescriptor::packed or Runtime::register_packed",
            T::NAME,
            T::KIND
        ))),
    }
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
        check_registered::<T>(view.state())?;
        // One read for the tag and the payload; the kind check is on the bits.
        match crate::convert::read_integer64(view) {
            Some(bits) => Packed::from_bits(bits),
            None => Err(view.type_error(Type::Integer)),
        }
    }

    /// The bits alone: the registry check is [`crate::bind::ParamItem::read_arg`]'s, which is
    /// the only caller with the VM at hand.
    #[inline(always)]
    fn from_raw_arg(raw: &crate::convert::RawValue, view: impl FnOnce() -> ValueView<'v>) -> Result<Self> {
        if raw.tag() == crate::raw::ffi::LUA_TINTEGER {
            Packed::from_bits(raw.integer())
        } else {
            Err(view().type_error(Type::Integer))
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
    #[inline(always)]
    fn read_arg(call: &'c crate::bind::Call<'c>, index: std::ffi::c_int) -> Result<Self> {
        match call.raw_arg(index) {
            Some(raw) => {
                check_registered::<T>(call.state())?;
                <Packed<T> as FromView<'c>>::from_raw_arg(raw, || call.arg(index))
            }
            None => <Packed<T> as FromView<'c>>::from_view(call.arg(index)),
        }
    }
    #[inline]
    fn matches(view: ValueView<'c>) -> bool {
        <Packed<T> as FromView<'c>>::matches(view)
    }
}

impl<T: PackedScalar> Push for Packed<T> {
    fn push_into<'s, S: Scope>(&self, scope: &'s S) -> Result<ValueView<'s>> {
        check_registered::<T>(scope.state())?;
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
