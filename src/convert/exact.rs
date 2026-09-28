//! Integers that never round, and 64-bit patterns that never become numbers.
//!
//! The plain Rust integer conversions keep OpenMW's behaviour: a Lua number rounds half away
//! from zero, so `640.4` reads as `640`. That is compatibility, not a good contract for a
//! structural value: an index, an offset, a count, a size, a schema version, an id. [`Exact`]
//! accepts a Luau integer or a number whose fractional part is exactly zero, both in range,
//! and rejects everything else. [`Bits64`] carries all 64 bits of an opaque value (a hash, a
//! peer id) through a Luau integer as a bit pattern; numerically it may look negative to a
//! script, which is the deal for opaque ids, and a numeric `u64` stays limited to what an
//! integer or an exact number can hold.

use super::scalar::{Scalar, integer_out_of_range, raw_scalar, read_scalar};
use super::{FromView, Push, RawValue};
use crate::error::{Error, Result};
use crate::raw::ffi;
use crate::stack::{Scope, Type, ValueView};

/// An integer parameter or result that never rounds: a Luau integer in range, or a number
/// with a zero fractional part in range.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
pub struct Exact<T>(pub T);

/// A 64-bit pattern carried in a Luau integer: opaque ids and hashes, all 64 bits, no numeric
/// meaning. Reads accept an integer only; a number never becomes one.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
pub struct Bits64(pub u64);

/// The integer value of `number` when it is finite, integral, and inside `T`'s range.
#[inline]
fn exact_from_number<T: TryFrom<i64> + TryFrom<u64>>(number: f64, signed: bool) -> Option<T> {
    if !number.is_finite() || number.fract() != 0.0 {
        return None;
    }
    if signed {
        if (-9_223_372_036_854_775_808.0..9_223_372_036_854_775_808.0).contains(&number) {
            T::try_from(number as i64).ok()
        } else {
            None
        }
    } else if (0.0..18_446_744_073_709_551_616.0).contains(&number) {
        T::try_from(number as u64).ok()
    } else {
        None
    }
}

#[cold]
fn not_exact(view: &ValueView<'_>, value: f64, target: &str) -> Error {
    Error::runtime(format!("Lua stack index {}: number {value} is not an exact {target}", view.index()))
}

macro_rules! exact_integers {
    ($($t:ty, signed = $signed:expr;)*) => {$(
        impl<'v> FromView<'v> for Exact<$t> {
            const EXPECTED: &'static str = "integer";

            #[inline(always)]
            fn from_view(view: ValueView<'v>) -> Result<Exact<$t>> {
                match read_scalar(view) {
                    Scalar::Integer(raw) => {
                        <$t>::try_from(raw).map(Exact).map_err(|_| integer_out_of_range(&view, raw, stringify!($t)))
                    }
                    Scalar::Number(number) => exact_from_number::<$t>(number, $signed)
                        .map(Exact)
                        .ok_or_else(|| not_exact(&view, number, stringify!($t))),
                    Scalar::Other => Err(view.type_error(Type::Integer)),
                }
            }

            #[inline(always)]
            fn from_raw_arg(raw: &RawValue, view: impl FnOnce() -> ValueView<'v>) -> Result<Exact<$t>> {
                match raw_scalar(raw) {
                    Scalar::Integer(raw) => {
                        <$t>::try_from(raw).map(Exact).map_err(|_| integer_out_of_range(&view(), raw, stringify!($t)))
                    }
                    Scalar::Number(number) => exact_from_number::<$t>(number, $signed)
                        .map(Exact)
                        .ok_or_else(|| not_exact(&view(), number, stringify!($t))),
                    Scalar::Other => Err(view().type_error(Type::Integer)),
                }
            }

            #[inline]
            fn matches(view: ValueView<'v>) -> bool {
                match read_scalar(view) {
                    Scalar::Integer(raw) => <$t>::try_from(raw).is_ok(),
                    Scalar::Number(number) => exact_from_number::<$t>(number, $signed).is_some(),
                    Scalar::Other => false,
                }
            }
        }

        impl Push for Exact<$t> {
            #[inline]
            fn push_into<'s, S: Scope>(&self, scope: &'s S) -> Result<ValueView<'s>> {
                self.0.push_into(scope)
            }
            #[inline]
            fn push_only<S: Scope>(&self, scope: &S) -> Result<()> {
                self.0.push_only(scope)
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

impl<'v> FromView<'v> for Bits64 {
    const EXPECTED: &'static str = "integer";

    #[inline(always)]
    fn from_view(view: ValueView<'v>) -> Result<Bits64> {
        match read_scalar(view) {
            Scalar::Integer(bits) => Ok(Bits64(bits as u64)),
            _ => Err(view.type_error(Type::Integer)),
        }
    }

    #[inline(always)]
    fn from_raw_arg(raw: &RawValue, view: impl FnOnce() -> ValueView<'v>) -> Result<Bits64> {
        if raw.tag() == ffi::LUA_TINTEGER { Ok(Bits64(raw.integer() as u64)) } else { Err(view().type_error(Type::Integer)) }
    }

    #[inline]
    fn matches(view: ValueView<'v>) -> bool {
        view.is_integer()
    }
}

impl Push for Bits64 {
    #[inline]
    fn push_only<S: Scope>(&self, scope: &S) -> Result<()> {
        // SAFETY: pushes one integer; the scope's stack has room by the scope contract.
        unsafe { ffi::lua_pushinteger64(scope.state(), self.0 as i64) };
        Ok(())
    }
    #[inline]
    fn push_into<'s, S: Scope>(&self, scope: &'s S) -> Result<ValueView<'s>> {
        self.push_only(scope)?;
        Ok(scope.top_value())
    }
}
