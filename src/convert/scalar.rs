use std::ffi::c_int;

use super::{FromView, RawValue, Push};
use crate::error::{Error, Result};
use crate::raw::ffi;
use crate::stack::{Scope, Type, ValueView};

// ---------------------------------------------------------------------------------------------
// bool: exact
// ---------------------------------------------------------------------------------------------

impl<'v> FromView<'v> for bool {
    const EXPECTED: &'static str = "boolean";

    #[inline(always)]
    fn from_view(view: ValueView<'v>) -> Result<bool> {
        if !view.is_boolean() {
            return Err(view.type_error(Type::Boolean));
        }
        // SAFETY: the view proved the slot exists.
        Ok(unsafe { ffi::lua_toboolean(view.state(), view.index()) } != 0)
    }

    #[inline(always)]
    fn from_raw_arg(raw: &RawValue, view: impl FnOnce() -> ValueView<'v>) -> Result<bool> {
        if raw.tag() == ffi::LUA_TBOOLEAN { Ok(raw.boolean()) } else { Err(view().type_error(Type::Boolean)) }
    }

    #[inline]
    fn matches(view: ValueView<'v>) -> bool {
        view.is_boolean()
    }
}

impl Push for bool {
    #[inline]
    fn push_into<'s, S: Scope>(&self, scope: &'s S) -> Result<ValueView<'s>> {
        self.push_only(scope)?;
        Ok(scope.top_value())
    }
    #[inline]
    fn push_only<S: Scope>(&self, scope: &S) -> Result<()> {
        unsafe { ffi::lua_pushboolean(scope.state(), c_int::from(*self)) };
        Ok(())
    }
}

// ---------------------------------------------------------------------------------------------
// Luau 64-bit integers
// ---------------------------------------------------------------------------------------------

/// An explicit Luau 64-bit integer. Plain Rust integers push as Lua numbers; wrap them in
/// `Integer` to push the `integer` runtime type instead.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
pub struct Integer(pub i64);

impl<'v> FromView<'v> for Integer {
    const EXPECTED: &'static str = "integer";

    #[inline(always)]
    fn from_view(view: ValueView<'v>) -> Result<Integer> {
        match read_scalar(view) {
            Scalar::Integer(value) => Ok(Integer(value)),
            _ => Err(view.type_error(Type::Integer)),
        }
    }

    #[inline(always)]
    fn from_raw_arg(raw: &RawValue, view: impl FnOnce() -> ValueView<'v>) -> Result<Integer> {
        if raw.tag() == ffi::LUA_TINTEGER { Ok(Integer(raw.integer())) } else { Err(view().type_error(Type::Integer)) }
    }

    #[inline]
    fn matches(view: ValueView<'v>) -> bool {
        view.is_integer()
    }
}

impl Push for Integer {
    #[inline]
    fn push_only<S: Scope>(&self, scope: &S) -> Result<()> {
        unsafe { ffi::lua_pushinteger64(scope.state(), self.0) };
        Ok(())
    }
    #[inline]
    fn push_into<'s, S: Scope>(&self, scope: &'s S) -> Result<ValueView<'s>> {
        self.push_only(scope)?;
        Ok(scope.top_value())
    }
}

/// The payload of an `integer` slot, or `None` for any other type.
/// A slot read in one call: its type, with the payload for numbers and integers.
#[derive(Clone, Copy)]
enum Scalar {
    Number(f64),
    Integer(i64),
    Other,
}

/// The scalar in a directly read slot; strings are never coerced.
#[inline(always)]
fn raw_scalar(raw: &RawValue) -> Scalar {
    match raw.tag() {
        ffi::LUA_TNUMBER => Scalar::Number(raw.number()),
        ffi::LUA_TINTEGER => Scalar::Integer(raw.integer()),
        _ => Scalar::Other,
    }
}

/// One FFI call classifies the slot and reads a numeric payload; strings are never coerced.
#[inline]
fn read_scalar(view: ValueView<'_>) -> Scalar {
    if !view.exists() {
        return Scalar::Other;
    }
    let mut number = 0.0f64;
    let mut integer = 0i64;
    // SAFETY: the slot exists on a live thread; the helper writes only the matching output.
    match unsafe { ffi::l3i_read_scalar(view.state(), view.index(), &mut number, &mut integer) } {
        ffi::LUA_TNUMBER => Scalar::Number(number),
        ffi::LUA_TINTEGER => Scalar::Integer(integer),
        _ => Scalar::Other,
    }
}

/// The payload of a slot the caller has already matched as `Type::Integer`.
#[inline]
pub(crate) fn read_integer64(view: ValueView<'_>) -> Option<i64> {
    match read_scalar(view) {
        Scalar::Integer(value) => Some(value),
        _ => None,
    }
}

/// The payload of a slot the caller has already matched as `Type::Number`.
#[inline]
fn read_number(view: ValueView<'_>) -> Option<f64> {
    match read_scalar(view) {
        Scalar::Number(value) => Some(value),
        _ => None,
    }
}

// ---------------------------------------------------------------------------------------------
// Rust integers
// ---------------------------------------------------------------------------------------------

/// True when `value` survives a round trip through f64, the C++ `isExactlyRepresentableAsDouble`.
fn exactly_representable_as_f64(magnitude: u64) -> bool {
    if magnitude == 0 {
        return true;
    }
    let bits = u64::BITS - magnitude.leading_zeros();
    bits <= f64::MANTISSA_DIGITS || magnitude.trailing_zeros() >= bits - f64::MANTISSA_DIGITS
}

macro_rules! integer_conversions {
    ($($t:ty, signed = $signed:literal, digits = $digits:expr;)*) => {$(
        impl<'v> FromView<'v> for $t {
            const EXPECTED: &'static str = "number";

            #[inline(always)]
            fn from_view(view: ValueView<'v>) -> Result<$t> {
                match read_scalar(view) {
                    Scalar::Integer(raw) => {
                        <$t>::try_from(raw).map_err(|_| integer_out_of_range(&view, raw, stringify!($t)))
                    }
                    Scalar::Number(number) => rounded_integer::<$t>(number, $signed, const { power_of_two($digits) })
                        .ok_or_else(|| number_out_of_range(&view, number, stringify!($t))),
                    Scalar::Other => Err(view.type_error(Type::Number)),
                }
            }

            #[inline(always)]
            fn from_raw_arg(raw: &RawValue, view: impl FnOnce() -> ValueView<'v>) -> Result<$t> {
                match raw_scalar(raw) {
                    Scalar::Integer(raw) => {
                        <$t>::try_from(raw).map_err(|_| integer_out_of_range(&view(), raw, stringify!($t)))
                    }
                    Scalar::Number(number) => rounded_integer::<$t>(number, $signed, const { power_of_two($digits) })
                        .ok_or_else(|| number_out_of_range(&view(), number, stringify!($t))),
                    Scalar::Other => Err(view().type_error(Type::Number)),
                }
            }

            #[inline]
            fn matches(view: ValueView<'v>) -> bool {
                match view.type_of() {
                    Type::Integer => read_integer64(view).is_some_and(|raw| <$t>::try_from(raw).is_ok()),
                    Type::Number => read_number(view)
                        .is_some_and(|n| rounded_integer::<$t>(n, $signed, const { power_of_two($digits) }).is_some()),
                    _ => false,
                }
            }
        }

        impl Push for $t {
            /// Pushes a Lua `number`; values an f64 cannot hold exactly are an error rather than
            /// a silently rounded result.
            #[inline]
            fn push_into<'s, S: Scope>(&self, scope: &'s S) -> Result<ValueView<'s>> {
                self.push_only(scope)?;
                Ok(scope.top_value())
            }
            #[inline]
            fn push_only<S: Scope>(&self, scope: &S) -> Result<()> {
                #[allow(unused_comparisons)]
                let magnitude: u64 = if *self < 0 { (*self as i128).unsigned_abs() as u64 } else { *self as u64 };
                if !exactly_representable_as_f64(magnitude) {
                    return Err(Error::runtime("Integer cannot be represented exactly as a Lua number"));
                }
                unsafe { ffi::lua_pushnumber(scope.state(), *self as f64) };
                Ok(())
            }
        }
    )*};
}

/// `2^digits` as an f64, exact for every width used here.
const fn power_of_two(digits: u32) -> f64 {
    (1u128 << digits) as f64
}

/// Rounds half away from zero and accepts the result only inside `[-upper, upper)` (signed) or
/// `[0, upper)` (unsigned), `upper` being `2^digits`; the C++ `tryGetRoundedIntegerNumber`.
/// NaN fails both comparisons and infinities fail the upper one.
#[inline(always)]
fn rounded_integer<T: TryFrom<i64> + TryFrom<u64>>(number: f64, signed: bool, upper: f64) -> Option<T> {
    let rounded = number.round();
    if signed {
        if !(rounded >= -upper && rounded < upper) {
            return None;
        }
        // The bound guarantees the value fits in i64 and in T.
        T::try_from(rounded as i64).ok()
    } else {
        if !(rounded >= 0.0 && rounded < upper) {
            return None;
        }
        T::try_from(rounded as u64).ok()
    }
}

/// A Luau integer that does not fit the requested Rust integer.
#[cold]
fn integer_out_of_range(view: &ValueView<'_>, value: i64, target: &str) -> Error {
    Error::runtime(format!("Lua stack index {}: integer {value} is out of range for {target}", view.index()))
}

/// A Lua number that, rounded, does not fit the requested Rust integer (or is not finite).
#[cold]
fn number_out_of_range(view: &ValueView<'_>, value: f64, target: &str) -> Error {
    Error::runtime(format!("Lua stack index {}: number {value} is out of range for {target}", view.index()))
}

integer_conversions! {
    i8, signed = true, digits = 7;
    i16, signed = true, digits = 15;
    i32, signed = true, digits = 31;
    i64, signed = true, digits = 63;
    isize, signed = true, digits = isize::BITS - 1;
    u8, signed = false, digits = 8;
    u16, signed = false, digits = 16;
    u32, signed = false, digits = 32;
    u64, signed = false, digits = 64;
    usize, signed = false, digits = usize::BITS;
}

// ---------------------------------------------------------------------------------------------
// Floating point
// ---------------------------------------------------------------------------------------------

impl<'v> FromView<'v> for f64 {
    const EXPECTED: &'static str = "number";

    #[inline(always)]
    fn from_view(view: ValueView<'v>) -> Result<f64> {
        match read_scalar(view) {
            Scalar::Number(value) => Ok(value),
            Scalar::Integer(value) => Ok(value as f64),
            Scalar::Other => Err(view.type_error(Type::Number)),
        }
    }

    #[inline(always)]
    fn from_raw_arg(raw: &RawValue, view: impl FnOnce() -> ValueView<'v>) -> Result<f64> {
        match raw_scalar(raw) {
            Scalar::Number(value) => Ok(value),
            Scalar::Integer(value) => Ok(value as f64),
            Scalar::Other => Err(view().type_error(Type::Number)),
        }
    }

    #[inline]
    fn matches(view: ValueView<'v>) -> bool {
        view.is_number()
    }
}

impl Push for f64 {
    #[inline]
    fn push_only<S: Scope>(&self, scope: &S) -> Result<()> {
        unsafe { ffi::lua_pushnumber(scope.state(), *self) };
        Ok(())
    }
    #[inline]
    fn push_into<'s, S: Scope>(&self, scope: &'s S) -> Result<ValueView<'s>> {
        unsafe { ffi::lua_pushnumber(scope.state(), *self) };
        Ok(scope.top_value())
    }
}

#[inline(always)]
fn f32_from_scalar<'v>(scalar: Scalar, view: impl FnOnce() -> ValueView<'v>) -> Result<f32> {
    let value = match scalar {
        Scalar::Integer(value) => return Ok(value as f32),
        Scalar::Number(value) => value,
        Scalar::Other => return Err(view().type_error(Type::Number)),
    };
    if value.is_finite() && (value < -f64::from(f32::MAX) || value > f64::from(f32::MAX)) {
        return Err(Error::runtime("Lua number does not fit destination floating-point type"));
    }
    Ok(value as f32)
}

impl<'v> FromView<'v> for f32 {
    const EXPECTED: &'static str = "number";

    /// Finite values outside the f32 range are rejected; NaN and infinities pass through. A
    /// Luau integer converts in one step (i64 to f32), as the C++ `static_cast` did, rather than
    /// rounding twice through f64.
    #[inline(always)]
    fn from_view(view: ValueView<'v>) -> Result<f32> {
        f32_from_scalar(read_scalar(view), || view)
    }

    #[inline(always)]
    fn from_raw_arg(raw: &RawValue, view: impl FnOnce() -> ValueView<'v>) -> Result<f32> {
        f32_from_scalar(raw_scalar(raw), view)
    }

    #[inline]
    fn matches(view: ValueView<'v>) -> bool {
        match view.type_of() {
            Type::Integer => true,
            Type::Number => read_number(view)
                .is_some_and(|n| !n.is_finite() || (n >= -f64::from(f32::MAX) && n <= f64::from(f32::MAX))),
            _ => false,
        }
    }
}

impl Push for f32 {
    #[inline]
    fn push_only<S: Scope>(&self, scope: &S) -> Result<()> {
        unsafe { ffi::lua_pushnumber(scope.state(), f64::from(*self)) };
        Ok(())
    }
    #[inline]
    fn push_into<'s, S: Scope>(&self, scope: &'s S) -> Result<ValueView<'s>> {
        unsafe { ffi::lua_pushnumber(scope.state(), f64::from(*self)) };
        Ok(scope.top_value())
    }
}

// ---------------------------------------------------------------------------------------------
// nil
// ---------------------------------------------------------------------------------------------

impl Push for () {
    /// Unit pushes nil, the counterpart of `Option::None`.
    #[inline]
    fn push_into<'s, S: Scope>(&self, scope: &'s S) -> Result<ValueView<'s>> {
        unsafe { ffi::lua_pushnil(scope.state()) };
        Ok(scope.top_value())
    }
}
