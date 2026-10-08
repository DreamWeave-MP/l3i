//! CLDR plural rules: [`PluralRules`].

use std::fmt;

use icu_plurals::{PluralCategory, PluralRuleType, PluralRulesPreferences};

use super::{Locale, NumberError, Operand, UnsupportedLocale};

/// Which rules: counts (`1 file`, `2 files`) or ranks (`1st`, `2nd`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum PluralKind {
    /// Cardinal rules, for quantities.
    Cardinal,
    /// Ordinal rules, for positions in an order.
    Ordinal,
}

impl PluralKind {
    /// The kind named `name`: `cardinal` or `ordinal`.
    pub fn parse(name: &str) -> Option<PluralKind> {
        match name {
            "cardinal" => Some(PluralKind::Cardinal),
            "ordinal" => Some(PluralKind::Ordinal),
            _ => None,
        }
    }

    /// The kind's name.
    pub const fn name(self) -> &'static str {
        match self {
            PluralKind::Cardinal => "cardinal",
            PluralKind::Ordinal => "ordinal",
        }
    }
}

/// A CLDR plural category.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Category {
    Zero,
    One,
    Two,
    Few,
    Many,
    Other,
}

impl Category {
    /// The category's CLDR keyword: `zero`, `one`, `two`, `few`, `many` or `other`.
    pub const fn name(self) -> &'static str {
        match self {
            Category::Zero => "zero",
            Category::One => "one",
            Category::Two => "two",
            Category::Few => "few",
            Category::Many => "many",
            Category::Other => "other",
        }
    }

    const fn of(category: PluralCategory) -> Category {
        match category {
            PluralCategory::Zero => Category::Zero,
            PluralCategory::One => Category::One,
            PluralCategory::Two => Category::Two,
            PluralCategory::Few => Category::Few,
            PluralCategory::Many => Category::Many,
            PluralCategory::Other => Category::Other,
        }
    }
}

impl fmt::Display for Category {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// One locale's cardinal or ordinal rules, built once from ICU4X's compiled CLDR data and
/// reused for every number.
///
/// A locale CLDR has no rules for takes its nearest parent's, down to the root rules, where
/// every number is `other`; that fallback is ICU4X's. Unicode extensions do not change plural
/// rules.
#[derive(Debug)]
pub struct PluralRules {
    rules: icu_plurals::PluralRules,
    kind: PluralKind,
    locale: Locale,
}

impl PluralRules {
    /// The `kind` rules for `locale`.
    pub fn new(locale: &Locale, kind: PluralKind) -> Result<PluralRules, UnsupportedLocale> {
        let rule_type = match kind {
            PluralKind::Cardinal => PluralRuleType::Cardinal,
            PluralKind::Ordinal => PluralRuleType::Ordinal,
        };
        let rules = icu_plurals::PluralRules::try_new(PluralRulesPreferences::from(locale.icu()), rule_type.into())
            .map_err(|error| UnsupportedLocale::new("plural rules", locale, &error))?;
        Ok(PluralRules { rules, kind, locale: locale.clone() })
    }

    /// The category `operand` falls in. Whole numbers never become decimals on the way.
    #[inline]
    pub fn category(&self, operand: Operand<'_>) -> Result<Category, NumberError> {
        let category = match operand.whole() {
            Some(whole) => self.rules.category_for(whole),
            None => self.rules.category_for(&operand.decimal()?),
        };
        Ok(Category::of(category))
    }

    /// The categories these rules can select, in CLDR order; always ends with `other`.
    pub fn categories(&self) -> impl ExactSizeIterator<Item = Category> {
        let mut all = [Category::Other; 6];
        let mut count = 0;
        for category in self.rules.categories() {
            all[count] = Category::of(category);
            count += 1;
        }
        all.into_iter().take(count)
    }

    /// The locale the rules were built for.
    pub fn locale(&self) -> &Locale {
        &self.locale
    }

    /// Cardinal or ordinal.
    pub fn kind(&self) -> PluralKind {
        self.kind
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rules(tag: &str, kind: PluralKind) -> PluralRules {
        PluralRules::new(&Locale::parse(tag).unwrap(), kind).unwrap()
    }

    fn assert_categories(rules: &PluralRules, cases: &[(Operand<'_>, &str)]) {
        for (operand, expected) in cases {
            assert_eq!(rules.category(*operand).unwrap().name(), *expected, "{} {operand:?}", rules.locale());
        }
    }

    use Operand::{Decimal as D, Integer as I, Number as N};

    #[test]
    fn english_cardinal_and_ordinal() {
        let cardinal = rules("en", PluralKind::Cardinal);
        assert_categories(
            &cardinal,
            &[(I(1), "one"), (I(0), "other"), (I(2), "other"), (I(-1), "one"), (N(1.0), "one"), (N(1.5), "other")],
        );
        assert_categories(&cardinal, &[(D("1"), "one"), (D("1.0"), "other"), (D("1.00"), "other"), (D("-1"), "one")]);
        let ordinal = rules("en", PluralKind::Ordinal);
        let ranks = [
            (1, "one"),
            (2, "two"),
            (3, "few"),
            (4, "other"),
            (11, "other"),
            (12, "other"),
            (13, "other"),
            (21, "one"),
            (22, "two"),
            (23, "few"),
            (101, "one"),
            (111, "other"),
            (112, "other"),
            (1003, "few"),
        ];
        for (rank, expected) in ranks {
            assert_eq!(ordinal.category(I(rank)).unwrap().name(), expected, "{rank}");
            assert_eq!(ordinal.category(N(rank as f64)).unwrap().name(), expected, "{rank}.0");
        }
        assert_eq!(cardinal.categories().map(Category::name).collect::<Vec<_>>(), ["one", "other"]);
        assert_eq!(ordinal.categories().map(Category::name).collect::<Vec<_>>(), ["one", "two", "few", "other"]);
        assert_eq!((cardinal.kind(), ordinal.kind()), (PluralKind::Cardinal, PluralKind::Ordinal));
    }

    #[test]
    fn polish_one_few_many() {
        let pl = rules("pl-PL", PluralKind::Cardinal);
        assert_categories(
            &pl,
            &[
                (I(0), "many"),
                (I(1), "one"),
                (I(2), "few"),
                (I(4), "few"),
                (I(5), "many"),
                (I(12), "many"),
                (I(14), "many"),
                (I(22), "few"),
                (I(25), "many"),
                (I(112), "many"),
                (I(122), "few"),
                (N(1.5), "other"),
                (D("1.0"), "other"),
                (D("2.00"), "other"),
            ],
        );
        assert_eq!(pl.categories().map(Category::name).collect::<Vec<_>>(), ["one", "few", "many", "other"]);
        assert_eq!(pl.locale().as_str(), "pl-PL");
    }

    #[test]
    fn russian_cardinal() {
        let ru = rules("ru", PluralKind::Cardinal);
        assert_categories(
            &ru,
            &[
                (I(1), "one"),
                (I(3), "few"),
                (I(5), "many"),
                (I(11), "many"),
                (I(21), "one"),
                (I(22), "few"),
                (I(111), "many"),
                (I(-1), "one"),
                (I(0), "many"),
                (N(1.5), "other"),
                (D("21.0"), "other"),
            ],
        );
    }

    #[test]
    fn arabic_has_all_six() {
        let ar = rules("ar", PluralKind::Cardinal);
        assert_categories(
            &ar,
            &[
                (I(0), "zero"),
                (I(1), "one"),
                (I(2), "two"),
                (I(3), "few"),
                (I(10), "few"),
                (I(11), "many"),
                (I(99), "many"),
                (I(100), "other"),
                (I(102), "other"),
                (I(103), "few"),
                (I(111), "many"),
                (N(0.5), "other"),
            ],
        );
        assert_eq!(ar.categories().count(), 6);
    }

    #[test]
    fn visible_fraction_digits_count() {
        // French `many` needs a multiple of a million with no visible fraction.
        let fr = rules("fr", PluralKind::Cardinal);
        assert_categories(
            &fr,
            &[
                (I(0), "one"),
                (N(1.5), "one"),
                (I(2), "other"),
                (I(1_000_000), "many"),
                (N(1e6), "many"),
                (D("1e6"), "many"),
                (D("1000000"), "many"),
                (D("1000000.0"), "other"),
            ],
        );
        // Latvian reads the visible fraction digits themselves: `0.1` is `one`, `0.10` is not.
        let lv = rules("lv", PluralKind::Cardinal);
        assert_categories(
            &lv,
            &[(I(10), "zero"), (D("0.1"), "one"), (N(0.1), "one"), (D("0.10"), "other"), (D("0.11"), "zero")],
        );
    }

    #[test]
    fn large_numbers_and_edges() {
        let pl = rules("pl", PluralKind::Cardinal);
        assert_categories(
            &pl,
            &[
                (I(i64::MAX), "many"),
                (I(i64::MIN), "many"),
                (N(9_007_199_254_740_992.0), "few"),
                (N(1e20), "many"),
                (N(-0.0), "many"),
                (N(f64::MAX), "many"),
                (N(f64::MIN_POSITIVE), "other"),
                (D("12345678901234567892"), "few"),
                (D("-0"), "many"),
            ],
        );
        for value in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            assert!(matches!(pl.category(N(value)), Err(NumberError::NotFinite(_))));
        }
        assert_eq!(pl.category(D("1,5")), Err(NumberError::Malformed("1,5".into())));
    }

    #[test]
    fn locales_without_rules_fall_back_and_extensions_are_ignored() {
        assert_categories(&rules("pl-u-nu-arab", PluralKind::Cardinal), &[(I(2), "few")]);
        assert_categories(&rules("pt-BR", PluralKind::Cardinal), &[(I(0), "one"), (I(1), "one"), (I(2), "other")]);
        let unknown = rules("qaa", PluralKind::Cardinal);
        assert_categories(&unknown, &[(I(1), "other"), (I(2), "other")]);
        assert_eq!(unknown.categories().map(Category::name).collect::<Vec<_>>(), ["other"]);
    }
}
