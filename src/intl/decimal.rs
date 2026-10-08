//! CLDR decimal formatting: [`DecimalFormatter`].

use std::fmt;

use fixed_decimal::{Decimal, SignDisplay};
use icu_decimal::DecimalFormatterPreferences;
use icu_decimal::options::{DecimalFormatterOptions, GroupingStrategy};
use writeable::Writeable;

use super::{Locale, NumberError, Operand, UnsupportedLocale};

/// The most fraction digits a formatter shows or pads to: ECMA-402's limit.
pub const MAX_FRACTION_DIGITS: u8 = 100;

/// The fraction digits shown when no option says otherwise, as ECMA-402 and ICU's decimal
/// style have it.
const DEFAULT_MAX_FRACTION_DIGITS: u8 = 3;

/// When grouping separators are written.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum Grouping {
    /// As the locale groups: `1,234` in English, `1234` in Polish, `12,34,567` in Hindi.
    #[default]
    Auto,
    /// Never.
    Never,
    /// Only with at least two digits before the first separator: `1234`, `12,345`.
    Min2,
}

impl Grouping {
    /// The strategy named `name`: `auto`, `never` or `min2`.
    pub fn parse(name: &str) -> Option<Grouping> {
        match name {
            "auto" => Some(Grouping::Auto),
            "never" => Some(Grouping::Never),
            "min2" => Some(Grouping::Min2),
            _ => None,
        }
    }

    /// The strategy's name.
    pub const fn name(self) -> &'static str {
        match self {
            Grouping::Auto => "auto",
            Grouping::Never => "never",
            Grouping::Min2 => "min2",
        }
    }
}

/// How a [`DecimalFormatter`] writes numbers. The fraction digit counts resolve as ECMA-402's
/// do: the minimum defaults to 0, the maximum to the larger of 3 and the minimum.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct DecimalOptions {
    /// When grouping separators are written.
    pub grouping: Grouping,
    /// Fraction digits always written, zeros padding a shorter number.
    pub min_fraction_digits: Option<u8>,
    /// Fraction digits at most: more are rounded half to even, and trailing zeros past the
    /// minimum are dropped.
    pub max_fraction_digits: Option<u8>,
}

/// Why a formatter could not be built.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum FormatterError {
    /// A fraction digit count past [`MAX_FRACTION_DIGITS`].
    FractionDigitsOutOfRange {
        /// The option's name, as the Luau options table spells it.
        option: &'static str,
        /// The value given.
        value: i64,
    },
    /// The minimum fraction digits exceed the maximum.
    FractionDigitsInverted {
        /// The minimum given.
        min: u8,
        /// The maximum given.
        max: u8,
    },
    /// ICU4X has no formatter for the locale.
    Unsupported(UnsupportedLocale),
}

impl fmt::Display for FormatterError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FormatterError::FractionDigitsOutOfRange { option, value } => {
                write!(f, "{option} must be a whole number from 0 to {MAX_FRACTION_DIGITS}, got {value}")
            }
            FormatterError::FractionDigitsInverted { min, max } => {
                write!(f, "minFractionDigits ({min}) is more than maxFractionDigits ({max})")
            }
            FormatterError::Unsupported(error) => error.fmt(f),
        }
    }
}

impl std::error::Error for FormatterError {}

/// `value` as a fraction digit count for `option`.
pub(crate) fn fraction_digits(option: &'static str, value: i64) -> Result<u8, FormatterError> {
    u8::try_from(value)
        .ok()
        .filter(|digits| *digits <= MAX_FRACTION_DIGITS)
        .ok_or(FormatterError::FractionDigitsOutOfRange { option, value })
}

/// One locale's decimal format, built once from ICU4X's compiled CLDR data and reused for
/// every number: the locale's digits, decimal and grouping separators, group sizes and signs.
///
/// Numbers are rounded half to even to the maximum fraction digits, trailing fraction zeros
/// past the minimum are dropped, and zeros pad to the minimum. Negative zero keeps its sign
/// (`-0`), as ECMA-402's default sign display does; an explicit `+` is not written. A Unicode
/// numbering system extension picks the digits (`ar-EG-u-nu-latn` writes `123`).
#[derive(Debug)]
pub struct DecimalFormatter {
    formatter: icu_decimal::DecimalFormatter,
    grouping: Grouping,
    min_fraction_digits: u8,
    max_fraction_digits: u8,
    locale: Locale,
}

impl DecimalFormatter {
    /// The formatter for `locale` under `options`.
    pub fn new(locale: &Locale, options: DecimalOptions) -> Result<DecimalFormatter, FormatterError> {
        let min = match options.min_fraction_digits {
            Some(min) => fraction_digits("minFractionDigits", i64::from(min))?,
            None => 0,
        };
        let max = match options.max_fraction_digits {
            Some(max) => fraction_digits("maxFractionDigits", i64::from(max))?,
            None => min.max(DEFAULT_MAX_FRACTION_DIGITS),
        };
        if min > max {
            return Err(FormatterError::FractionDigitsInverted { min, max });
        }
        let strategy = match options.grouping {
            Grouping::Auto => GroupingStrategy::Auto,
            Grouping::Never => GroupingStrategy::Never,
            Grouping::Min2 => GroupingStrategy::Min2,
        };
        let formatter = icu_decimal::DecimalFormatter::try_new(
            DecimalFormatterPreferences::from(locale.icu()),
            DecimalFormatterOptions::from(strategy),
        )
        .map_err(|error| FormatterError::Unsupported(UnsupportedLocale::new("decimal format", locale, &error)))?;
        Ok(DecimalFormatter {
            formatter,
            grouping: options.grouping,
            min_fraction_digits: min,
            max_fraction_digits: max,
            locale: locale.clone(),
        })
    }

    /// The decimal to write for `operand`, rounded and padded.
    fn decimal(&self, operand: Operand<'_>) -> Result<Decimal, NumberError> {
        // A whole number needs neither rounding nor trimming; negative zero keeps its sign
        // through the decimal path.
        let negative_zero = matches!(operand, Operand::Number(value) if value == 0.0 && value.is_sign_negative());
        if let (Some(whole), false) = (operand.whole(), negative_zero) {
            let mut decimal = Decimal::from(whole);
            decimal.absolute.pad_end(-i16::from(self.min_fraction_digits));
            return Ok(decimal);
        }
        let mut decimal = operand.decimal()?;
        decimal.absolute.trim_start();
        decimal.round(-i16::from(self.max_fraction_digits));
        decimal.absolute.trim_end();
        decimal.absolute.pad_end(-i16::from(self.min_fraction_digits));
        decimal.apply_sign_display(SignDisplay::Auto);
        Ok(decimal)
    }

    /// Appends `operand`, formatted, to `out`.
    pub fn format_to(&self, operand: Operand<'_>, out: &mut String) -> Result<(), NumberError> {
        let decimal = self.decimal(operand)?;
        // Writing into a `String` cannot fail.
        let _ = self.formatter.format(&decimal).write_to(out);
        Ok(())
    }

    /// `operand`, formatted.
    pub fn format(&self, operand: Operand<'_>) -> Result<String, NumberError> {
        let mut out = String::new();
        self.format_to(operand, &mut out)?;
        Ok(out)
    }

    /// The locale the formatter was built for.
    pub fn locale(&self) -> &Locale {
        &self.locale
    }

    /// The grouping strategy.
    pub fn grouping(&self) -> Grouping {
        self.grouping
    }

    /// The resolved minimum fraction digits.
    pub fn min_fraction_digits(&self) -> u8 {
        self.min_fraction_digits
    }

    /// The resolved maximum fraction digits.
    pub fn max_fraction_digits(&self) -> u8 {
        self.max_fraction_digits
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use Operand::{Decimal as D, Integer as I, Number as N};

    fn formatter(tag: &str, grouping: Grouping, min: Option<u8>, max: Option<u8>) -> DecimalFormatter {
        let options = DecimalOptions { grouping, min_fraction_digits: min, max_fraction_digits: max };
        DecimalFormatter::new(&Locale::parse(tag).unwrap(), options).unwrap()
    }

    fn plain(tag: &str) -> DecimalFormatter {
        formatter(tag, Grouping::Auto, None, None)
    }

    fn assert_formats(formatter: &DecimalFormatter, cases: &[(Operand<'_>, &str)]) {
        for (operand, expected) in cases {
            assert_eq!(formatter.format(*operand).unwrap(), *expected, "{} {operand:?}", formatter.locale());
        }
    }

    #[test]
    fn separators_and_digits_follow_the_locale() {
        let v = N(1_234_567.891);
        assert_formats(&plain("en"), &[(v, "1,234,567.891"), (I(1000), "1,000"), (I(999), "999")]);
        assert_formats(&plain("de"), &[(v, "1.234.567,891")]);
        assert_formats(&plain("fr"), &[(v, "1\u{202f}234\u{202f}567,891")]);
        assert_formats(&plain("ar-EG"), &[(I(123), "١٢٣"), (D("-1234.5"), "\u{61c}-١٬٢٣٤٫٥")]);
        assert_formats(&plain("ar-EG-u-nu-latn"), &[(I(123), "123")]);
        assert_formats(&plain("en-u-nu-arab"), &[(I(1234), "١,٢٣٤")]);
        assert_formats(&plain("hi"), &[(I(1_234_567), "12,34,567")]);
        assert_formats(&plain("en-IN"), &[(I(1_234_567), "12,34,567")]);
        assert_formats(&plain("pl"), &[(I(1234), "1234"), (I(12345), "12\u{a0}345")]);
        let fr = formatter("fr", Grouping::Auto, Some(2), Some(2));
        assert_formats(&fr, &[(D("1234567.895"), "1\u{202f}234\u{202f}567,90")]);
    }

    #[test]
    fn grouping_strategies() {
        let auto = plain("en");
        let never = formatter("en", Grouping::Never, None, None);
        let min2 = formatter("en", Grouping::Min2, None, None);
        assert_formats(&auto, &[(I(1234), "1,234"), (I(12345), "12,345")]);
        assert_formats(&never, &[(I(1234), "1234"), (N(1_234_567.5), "1234567.5")]);
        assert_formats(&min2, &[(I(1234), "1234"), (I(12345), "12,345"), (I(1_234_567), "1,234,567")]);
        assert_eq!(
            (auto.grouping(), never.grouping(), min2.grouping()),
            (Grouping::Auto, Grouping::Never, Grouping::Min2)
        );
    }

    #[test]
    fn fraction_digits_pad_trim_and_round_half_even() {
        let default = plain("en");
        assert_eq!((default.min_fraction_digits(), default.max_fraction_digits()), (0, 3));
        assert_formats(
            &default,
            &[
                (N(0.123_456_789), "0.123"),
                (N(1.999_96), "2"),
                (D("1.50"), "1.5"),
                (D("1.000"), "1"),
                (D("007.25"), "7.25"),
                (D("+5"), "5"),
                (D("1e3"), "1,000"),
                (D("0.0005"), "0"),
                (D("0.0015"), "0.002"),
            ],
        );
        let two = formatter("en", Grouping::Auto, Some(2), Some(2));
        assert_formats(
            &two,
            &[
                (I(2), "2.00"),
                (D("2.5"), "2.50"),
                (D("0.125"), "0.12"),
                (D("0.135"), "0.14"),
                (D("1234567.885"), "1,234,567.88"),
                (D("1234567.895"), "1,234,567.90"),
                (N(0.125), "0.12"),
                (N(1.005), "1.00"),
            ],
        );
        let whole = formatter("en", Grouping::Auto, None, Some(0));
        assert_formats(
            &whole,
            &[(D("0.5"), "0"), (D("1.5"), "2"), (D("2.5"), "2"), (D("3.5"), "4"), (D("-2.5"), "-2")],
        );
        let one = formatter("en", Grouping::Auto, Some(1), Some(2));
        assert_formats(&one, &[(I(2), "2.0"), (D("2.50"), "2.5"), (D("2.567"), "2.57")]);
        let four = formatter("en", Grouping::Auto, Some(2), Some(4));
        assert_formats(&four, &[(D("1.23456"), "1.2346"), (D("1.2"), "1.20")]);
        let wide = formatter("en", Grouping::Never, Some(5), None);
        assert_eq!(wide.max_fraction_digits(), 5, "the maximum follows a minimum above 3");
        assert_formats(&wide, &[(I(1), "1.00000")]);
        let narrow = formatter("en", Grouping::Auto, None, Some(1));
        assert_eq!(narrow.min_fraction_digits(), 0);
        assert_formats(&narrow, &[(N(0.25), "0.2"), (N(0.35), "0.4")]);
    }

    #[test]
    fn signs_and_large_values() {
        let en = plain("en");
        assert_formats(
            &en,
            &[
                (I(-1234), "-1,234"),
                (N(-1234.5), "-1,234.5"),
                (N(-0.5), "-0.5"),
                (N(-0.0), "-0"),
                (N(0.0), "0"),
                (I(0), "0"),
                (D("-0"), "-0"),
                (D("-0.0004"), "-0"),
                (I(i64::MIN), "-9,223,372,036,854,775,808"),
                (I(i64::MAX), "9,223,372,036,854,775,807"),
                (N(1e21), "1,000,000,000,000,000,000,000"),
                (N(2f64.powi(53) + 2.0), "9,007,199,254,740,994"),
                (N(5e-324), "0"),
                (D("123456789012345678901234567890.5"), "123,456,789,012,345,678,901,234,567,890.5"),
            ],
        );
        let de = formatter("de", Grouping::Auto, Some(2), Some(2));
        assert_formats(&de, &[(N(-0.0), "-0,00"), (D("-1234.567"), "-1.234,57")]);
    }

    #[test]
    fn bad_numbers_and_options_say_why() {
        let en = plain("en");
        for value in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            assert!(matches!(en.format(N(value)), Err(NumberError::NotFinite(_))));
        }
        assert_eq!(en.format(D("1,5")), Err(NumberError::Malformed("1,5".into())));
        let locale = Locale::parse("en").unwrap();
        let options =
            |min, max| DecimalOptions { grouping: Grouping::Auto, min_fraction_digits: min, max_fraction_digits: max };
        assert_eq!(
            DecimalFormatter::new(&locale, options(Some(101), None)).unwrap_err(),
            FormatterError::FractionDigitsOutOfRange { option: "minFractionDigits", value: 101 }
        );
        assert_eq!(
            DecimalFormatter::new(&locale, options(None, Some(200))).unwrap_err().to_string(),
            "maxFractionDigits must be a whole number from 0 to 100, got 200"
        );
        assert_eq!(
            DecimalFormatter::new(&locale, options(Some(3), Some(2))).unwrap_err().to_string(),
            "minFractionDigits (3) is more than maxFractionDigits (2)"
        );
        assert!(DecimalFormatter::new(&locale, options(Some(100), Some(100))).is_ok());
        assert_eq!(
            fraction_digits("minFractionDigits", -1),
            Err(FormatterError::FractionDigitsOutOfRange { option: "minFractionDigits", value: -1 })
        );
        assert_eq!(Grouping::parse("min2"), Some(Grouping::Min2));
        assert_eq!(Grouping::parse("always"), None);
    }
}
