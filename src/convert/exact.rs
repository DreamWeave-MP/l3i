//! Integers that never round, and 64-bit patterns that never become numbers.
//!
//! The plain Rust integer conversions keep OpenMW's behaviour: a Lua number rounds half away
//! from zero, so `640.4` reads as `640`. That is compatibility, not a good contract for a
//! structural value: an index, an offset, a count, a size, a schema version, an id. [`Exact`]
//! accepts a number whose fractional part is exactly zero, in range, and rejects everything else.
//! It is input only: for results the vocabulary is a plain Rust integer (an exact Lua `number`,
//! or an error when the value is beyond 2^53) or [`Bits64`]. [`Bits64`] carries all 64 bits of an
//! opaque value (a digest, a client id, a protocol id) as a wide packed scalar
//! ([`crate::packed::WideScalar`]): compared and keyed by value, never arithmetic, never a number.

use super::scalar::{Scalar, raw_scalar, read_scalar};
use super::{FromView, RawValue};
use crate::error::{Error, Result};
use crate::stack::{Type, ValueView};

/// An integer parameter that never rounds: a number with a zero fractional part, in range.
/// Input only; it has no push form.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
pub struct Exact<T>(pub T);

/// A 64-bit pattern: opaque ids and hashes, all 64 bits, no numeric meaning. A wide packed scalar
/// named `Bits64` to scripts (`typeof`), equal and a table key by value, with no arithmetic;
/// `@dream/bits64` converts it to and from numbers, hex and bytes. A number never reads as one.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
pub struct Bits64(pub u64);

impl crate::packed::WideScalar for Bits64 {
    const NAME: &'static str = "Bits64";
    const LIGHT_TAG: crate::packed::carrier::LightTag = crate::packed::carrier::BITS64_TAG;
    #[inline]
    fn to_bits(self) -> u64 {
        self.0
    }
    #[inline]
    fn from_bits(bits: u64) -> Self {
        Bits64(bits)
    }
}

crate::packed::wide_scalar_conversions!(Bits64);

/// The integer value of `number` when it is finite, integral, and inside `T`'s range.
#[inline]
fn exact_from_number<T: TryFrom<i64> + TryFrom<u64>>(number: f64, signed: bool) -> Option<T> {
    // The saturating cast and the round trip reject a fraction, NaN and infinity without a call to
    // `trunc`, on a path every integer argument takes. Saturation lands on exactly 2^63 or 2^64, so
    // those need the range check.
    if signed {
        let whole = number as i64;
        if whole as f64 == number && number < 9_223_372_036_854_775_808.0 { T::try_from(whole).ok() } else { None }
    } else {
        let whole = number as u64;
        if whole as f64 == number && number < 18_446_744_073_709_551_616.0 { T::try_from(whole).ok() } else { None }
    }
}

#[cold]
fn not_exact(view: &ValueView<'_>, value: f64, target: &str) -> Error {
    Error::runtime(format!("Lua stack index {}: number {value} is not an exact {target}", view.index()))
}

macro_rules! exact_integers {
    ($($t:ty, signed = $signed:expr;)*) => {$(
        impl<'v> FromView<'v> for Exact<$t> {
            const EXPECTED: &'static str = "number";

            #[inline(always)]
            fn from_view(view: ValueView<'v>) -> Result<Exact<$t>> {
                match read_scalar(view) {
                    Scalar::Number(number) => exact_from_number::<$t>(number, $signed)
                        .map(Exact)
                        .ok_or_else(|| not_exact(&view, number, stringify!($t))),
                    Scalar::Other => Err(view.type_error(Type::Number)),
                }
            }

            #[inline(always)]
            fn from_raw_arg(raw: &RawValue, view: impl FnOnce() -> ValueView<'v>) -> Result<Exact<$t>> {
                match raw_scalar(raw) {
                    Scalar::Number(number) => exact_from_number::<$t>(number, $signed)
                        .map(Exact)
                        .ok_or_else(|| not_exact(&view(), number, stringify!($t))),
                    Scalar::Other => Err(view().type_error(Type::Number)),
                }
            }

            #[inline]
            fn matches(view: ValueView<'v>) -> bool {
                match read_scalar(view) {
                    Scalar::Number(number) => exact_from_number::<$t>(number, $signed).is_some(),
                    Scalar::Other => false,
                }
            }
        }

    )*};
}

exact_integers! {
    i8, signed = true;
    i16, signed = true;
    i32, signed = true;
    i64, signed = true;
    isize, signed = true;
    u8, signed = false;
    u16, signed = false;
    u32, signed = false;
    u64, signed = false;
    usize, signed = false;
}
