//! Numbers as plural rules and formatters read them: [`Operand`].

use std::fmt;

use fixed_decimal::{Decimal, FloatPrecision};

use std::ffi::c_int;

use crate::bind::{Call, Param, ParamItem, ParamKind};
use crate::convert::{FromView, Integer, RawValue};
use crate::error::Result;
use crate::raw::ffi;
use crate::stack::{Type, ValueView};

/// A number to select a plural category for or to format.
///
/// A decimal string is exact and keeps what it shows: `"1.00"` has two visible fraction
/// digits, which CLDR's rules can tell from `"1"` (English `1` is `one`, `1.00` is `other`). A
/// Luau number has no visible fraction digits of its own; it reads as the shortest decimal that
/// round-trips to the same double, so `1.0` is `1` and `0.1` is `0.1`, not the binary fraction.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Operand<'a> {
    /// A whole number.
    Integer(i64),
    /// A double; NaN and the infinities are refused.
    Number(f64),
    /// A decimal string: an optional sign, digits with at most one decimal point, and an
    /// optional exponent (`-12.50`, `.5`, `1e6`). Visible trailing zeros count.
    Decimal(&'a str),
}

/// Why an [`Operand`] is not a number.
#[derive(Clone, PartialEq, Debug)]
pub enum NumberError {
    /// NaN or an infinity.
    NotFinite(f64),
    /// A string that is not a decimal number.
    Malformed(String),
    /// A decimal string past the 32767 digits either side of the point a decimal holds.
    TooLong(String),
}

impl fmt::Display for NumberError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            NumberError::NotFinite(value) => write!(f, "{value} is not a finite number"),
            NumberError::Malformed(text) => write!(f, "'{text}' is not a decimal number"),
            NumberError::TooLong(text) => {
                write!(f, "a decimal string of {} bytes has more digits than a decimal holds", text.len())
            }
        }
    }
}

impl std::error::Error for NumberError {}

/// Doubles below this magnitude that are whole convert to `i64` exactly.
const WHOLE_LIMIT: f64 = 9_223_372_036_854_775_808.0;

impl Operand<'_> {
    /// The operand as a whole number, when it is one with no visible fraction: integers, and
    /// whole doubles inside `i64`.
    #[inline]
    pub(crate) fn whole(&self) -> Option<i64> {
        match *self {
            Operand::Integer(value) => Some(value),
            Operand::Number(value) if value.trunc() == value && value.abs() < WHOLE_LIMIT => Some(value as i64),
            _ => None,
        }
    }

    /// The operand as an exact decimal.
    pub fn decimal(&self) -> std::result::Result<Decimal, NumberError> {
        match *self {
            Operand::Integer(value) => Ok(Decimal::from(value)),
            Operand::Number(value) if !value.is_finite() => Err(NumberError::NotFinite(value)),
            Operand::Number(value) => {
                Decimal::try_from_f64(value, FloatPrecision::RoundTrip).map_err(|_| NumberError::NotFinite(value))
            }
            Operand::Decimal(text) => Decimal::try_from_str(text).map_err(|error| match error {
                fixed_decimal::ParseError::Limit => NumberError::TooLong(text.to_owned()),
                _ => NumberError::Malformed(text.to_owned()),
            }),
        }
    }
}

impl<'v> FromView<'v> for Operand<'v> {
    const EXPECTED: &'static str = "number or decimal string";

    fn from_view(view: ValueView<'v>) -> Result<Self> {
        match view.type_of() {
            Type::Integer => Ok(Operand::Integer(view.read::<Integer>()?.0)),
            Type::Number => Ok(Operand::Number(view.read::<f64>()?)),
            Type::String => Ok(Operand::Decimal(view.read::<&str>()?)),
            _ => Err(view.type_error_expecting(<Self as FromView<'v>>::EXPECTED)),
        }
    }

    #[inline(always)]
    fn from_raw_arg(raw: &RawValue, view: impl FnOnce() -> ValueView<'v>) -> Result<Self> {
        match raw.tag() {
            ffi::LUA_TNUMBER => Ok(Operand::Number(raw.number())),
            ffi::LUA_TINTEGER => Ok(Operand::Integer(raw.integer())),
            _ => Self::from_view(view()),
        }
    }
}

impl Param for Operand<'_> {
    type Item<'c> = Operand<'c>;
}

impl<'c> ParamItem<'c> for Operand<'c> {
    const KIND: ParamKind = ParamKind::Regular;
    const EXPECTED: &'static str = <Operand<'c> as FromView<'c>>::EXPECTED;

    #[inline]
    fn read_slot(view: ValueView<'c>) -> Result<Self> {
        Operand::from_view(view)
    }

    #[inline(always)]
    fn read_arg(call: &'c Call<'c>, index: c_int) -> Result<Self> {
        match call.raw_arg(index) {
            Some(raw) => Operand::from_raw_arg(raw, || call.arg(index)),
            None => Operand::from_view(call.arg(index)),
        }
    }

    #[inline]
    fn matches(view: ValueView<'c>) -> bool {
        matches!(view.type_of(), Type::Number | Type::Integer | Type::String)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn whole_numbers_skip_the_decimal() {
        assert_eq!(Operand::Integer(-7).whole(), Some(-7));
        assert_eq!(Operand::Number(42.0).whole(), Some(42));
        assert_eq!(Operand::Number(-0.0).whole(), Some(0));
        assert_eq!(Operand::Number(-9_223_372_036_854_775_808.0).whole(), None, "decimal path, exact all the same");
        assert_eq!(Operand::Number(1.5).whole(), None);
        assert_eq!(Operand::Number(f64::NAN).whole(), None);
        assert_eq!(Operand::Number(f64::INFINITY).whole(), None);
        assert_eq!(Operand::Decimal("1").whole(), None);
    }

    #[test]
    fn decimals_keep_what_they_show() {
        let text = |operand: Operand<'_>| operand.decimal().unwrap().to_string();
        assert_eq!(text(Operand::Decimal("1.00")), "1.00");
        assert_eq!(text(Operand::Decimal("-12.50")), "-12.50");
        assert_eq!(text(Operand::Decimal("+3")), "+3");
        assert_eq!(text(Operand::Decimal(".5")), "0.5");
        assert_eq!(text(Operand::Decimal("1e6")), "1000000");
        assert_eq!(text(Operand::Decimal("1.5E-3")), "0.0015");
        assert_eq!(text(Operand::Number(0.1)), "0.1");
        assert_eq!(text(Operand::Number(1.0)), "1");
        assert_eq!(text(Operand::Number(-0.0)), "-0");
        assert_eq!(text(Operand::Number(1e21)), "1000000000000000000000");
        assert_eq!(text(Operand::Integer(i64::MIN)), "-9223372036854775808");
        assert_eq!(text(Operand::Decimal("123456789012345678901234567890.5")), "123456789012345678901234567890.5");
    }

    #[test]
    fn non_numbers_say_why() {
        for bad in ["", "-", "abc", "1,5", " 1", "1 ", "5.", "0x10", "1_000", "1.2.3", "--1", "inf", "NaN"] {
            assert_eq!(Operand::Decimal(bad).decimal(), Err(NumberError::Malformed(bad.to_owned())), "{bad:?}");
        }
        let long = "9".repeat(40_000);
        assert_eq!(Operand::Decimal(&long).decimal(), Err(NumberError::TooLong(long.clone())));
        assert!(NumberError::TooLong(long).to_string().starts_with("a decimal string of 40000 bytes"));
        for value in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            assert!(matches!(Operand::Number(value).decimal(), Err(NumberError::NotFinite(_))));
        }
        assert_eq!(NumberError::NotFinite(f64::INFINITY).to_string(), "inf is not a finite number");
        assert_eq!(NumberError::Malformed("1,5".into()).to_string(), "'1,5' is not a decimal number");
    }
}
