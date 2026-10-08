//! Internationalization primitives: the `dream.intl` extension, module `@dream/intl`.
//!
//! ```lua
//! local intl = require('@dream/intl')
//! local locale = intl.locale('pt_br')
//! print(locale:tag(), locale:language(), locale:region())   -- pt-BR  pt  BR
//! print(intl.canonicalize('ZH-hant-tw'))                    -- zh-Hant-TW
//! ```
//!
//! # Locale identity
//!
//! [`Locale`] is a BCP 47 language tag, validated and spelled canonically by ICU4X. The
//! canonical spelling is the locale's identity: two spellings of one locale give the same tag,
//! which is what to key a cache or compare with. `intl.canonicalize` returns that spelling
//! without making a userdata, and makes no heap allocation for a tag of up to 64 bytes.
//!
//! Choosing a locale, and falling back from one to another to find a translation, are the
//! application's: this module has no current locale and no fallback order.
//!
//! # Plural rules
//!
//! [`PluralRules`] are one locale's CLDR cardinal or ordinal rules, built once and reused. They
//! return a category, never a message: what text a category selects is the caller's.
//!
//! ```lua
//! local polish = intl.pluralRules('pl', 'cardinal')
//! print(polish:category(1), polish:category(2), polish:category(5))   -- one few many
//! print(polish:category('1.0'))                                         -- other
//! print(intl.pluralRules('en', 'ordinal'):category(22))                -- two
//! ```
//!
//! Numbers are [`Operand`]s: a Luau number or integer, or a decimal string when visible
//! fraction digits matter (`1.00` has two, and CLDR can tell it from `1`; the double `1.0`
//! cannot carry them).
//!
//! # Decimal formatting
//!
//! A [`DecimalFormatter`] writes numbers with a locale's digits, separators, group sizes and
//! signs, rounding half to even to its maximum fraction digits and padding to its minimum:
//!
//! ```lua
//! local french = intl.decimalFormatter('fr', { minFractionDigits = 2, maxFractionDigits = 2 })
//! print(french:format('1234567.895'))                    -- 1 234 567,90 (narrow no-break spaces)
//! print(intl.decimalFormatter('ar-EG'):format(123))      -- ١٢٣
//! print(intl.decimalFormatter('en', { grouping = 'min2' }):format(1234))   -- 1234
//! ```
//!
//! A decimal string formats exactly; a Luau number formats as its shortest round-trip decimal
//! (`1.005` is `1.005`, so it rounds to `1.00` at two digits, not by its binary value).
//!
//! # Handles
//!
//! Rules and formatters are built from ICU4X's compiled CLDR data, which is in the binary: no
//! file, no provider, no capability. Each constructor builds once and the handle is reused;
//! nothing is cached behind it and nothing is shared between runtimes. A formatter's handle
//! writes into its own buffer, so a warm `format` allocates nothing native for the text it
//! returns.

mod decimal;
mod locale;
mod operand;
mod plural;

pub use decimal::{DecimalFormatter, DecimalOptions, FormatterError, Grouping, MAX_FRACTION_DIGITS};
pub use locale::{Locale, LocaleError, canonicalize, with_canonical};
pub use operand::{NumberError, Operand};
pub use plural::{Category, PluralKind, PluralRules};

use std::cell::RefCell;
use std::fmt;

use crate::bind::{Call, StackResults};
use crate::convert::Exact;
use crate::error::{Error, Result};
use crate::extension::{Extension, ExtensionDescriptor, TagPolicy};
use crate::options::Options;
use crate::stack::{Frame, Scope, ValueView};
use crate::userdata::Owned;

/// The extension id.
pub const EXTENSION_ID: &str = "dream.intl";
/// The module path.
pub const MODULE: &str = "@dream/intl";

/// ICU4X could not build rules or a formatter for a locale. With the compiled CLDR data a
/// locale without data of its own falls back to its parents and the root, so this is rare.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct UnsupportedLocale {
    /// What was being built.
    pub what: &'static str,
    /// The locale's canonical tag.
    pub locale: String,
    /// ICU4X's reason.
    pub reason: String,
}

impl UnsupportedLocale {
    fn new(what: &'static str, locale: &Locale, reason: &impl fmt::Display) -> UnsupportedLocale {
        UnsupportedLocale { what, locale: locale.as_str().to_owned(), reason: reason.to_string() }
    }
}

impl fmt::Display for UnsupportedLocale {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "no {} for '{}': {}", self.what, self.locale, self.reason)
    }
}

impl std::error::Error for UnsupportedLocale {}

// SAFETY: the payload holds no Lua references.
unsafe impl crate::userdata::Userdata for Locale {
    const NAME: &'static str = "dream.intl.Locale";
}

// SAFETY: the payload holds no Lua references.
unsafe impl crate::userdata::Userdata for PluralRules {
    const NAME: &'static str = "dream.intl.PluralRules";
}

/// `dream.intl.DecimalFormatter`: a formatter and the buffer its results are written into.
struct FormatterHandle {
    formatter: DecimalFormatter,
    scratch: RefCell<String>,
}

// SAFETY: the payload holds no Lua references.
unsafe impl crate::userdata::Userdata for FormatterHandle {
    const NAME: &'static str = "dream.intl.DecimalFormatter";
}

/// The locale a constructor's first argument names: a [`Locale`] or a tag.
fn locale_arg(what: &str, view: ValueView<'_>) -> Result<Locale> {
    if let Some(locale) = crate::userdata::receiver::<Locale>(view) {
        return Ok(locale.clone());
    }
    match view.read::<&str>() {
        Ok(tag) => Locale::parse(tag).map_err(|error| Error::runtime(format!("{what}: {error}"))),
        Err(_) => Err(view.field_type_error(what, "a language tag or a dream.intl.Locale")),
    }
}

/// `intl.pluralRules(locale, type?)`.
fn new_plural_rules(locale: ValueView<'_>, kind: Option<&str>) -> Result<Owned<PluralRules>> {
    const WHAT: &str = "intl.pluralRules";
    let locale = locale_arg(WHAT, locale)?;
    let kind = match kind {
        None => PluralKind::Cardinal,
        Some(name) => PluralKind::parse(name)
            .ok_or_else(|| Error::runtime(format!("{WHAT}: unknown type '{name}' (cardinal or ordinal)")))?,
    };
    PluralRules::new(&locale, kind).map(Owned).map_err(|error| Error::runtime(format!("{WHAT}: {error}")))
}

/// `intl.decimalFormatter(locale, options?)`.
fn new_decimal_formatter(
    call: &Call<'_>,
    locale: ValueView<'_>,
    options: Option<ValueView<'_>>,
) -> Result<Owned<FormatterHandle>> {
    const WHAT: &str = "intl.decimalFormatter";
    let locale = locale_arg(WHAT, locale)?;
    let mut resolved = DecimalOptions::default();
    if let Some(options) = options.filter(|view| !view.is_nil()) {
        Options::read(call, options, WHAT, |o| {
            let grouping = o.optional_str("grouping", |name| {
                Grouping::parse(name)
                    .ok_or_else(|| Error::runtime(format!("{WHAT}: unknown grouping '{name}' (auto, never or min2)")))
            })?;
            resolved.grouping = grouping.unwrap_or_default();
            for (key, slot) in [
                ("minFractionDigits", &mut resolved.min_fraction_digits),
                ("maxFractionDigits", &mut resolved.max_fraction_digits),
            ] {
                *slot = match o.optional::<Exact<i64>>(key)? {
                    Some(digits) => Some(
                        decimal::fraction_digits(key, digits.0)
                            .map_err(|error| Error::runtime(format!("{WHAT}: {error}")))?,
                    ),
                    None => None,
                };
            }
            Ok(())
        })?;
    }
    let formatter =
        DecimalFormatter::new(&locale, resolved).map_err(|error| Error::runtime(format!("{WHAT}: {error}")))?;
    Ok(Owned(FormatterHandle { formatter, scratch: RefCell::new(String::new()) }))
}

/// `text` as the call's one result.
fn push_str(call: &Call<'_>, text: &str) -> Result<StackResults> {
    call.push(text)?;
    Ok(StackResults)
}

/// `items` as a sequence table, the call's one result.
fn push_list<'a>(call: &Call<'_>, items: impl ExactSizeIterator<Item = &'a str>) -> Result<StackResults> {
    let mut frame: Frame<'_> = call.frame();
    let table = frame.push_table(items.len(), 0)?;
    for (index, item) in items.enumerate() {
        frame.push(item)?;
        table.raw_set_index(&frame, index as i64 + 1)?;
    }
    frame.release();
    Ok(StackResults)
}

fn describe_locale(d: &mut ExtensionDescriptor) {
    let mut locale = d.userdata::<Locale>("dream.intl.Locale");
    locale.tag(TagPolicy::Preferred).doc("A BCP 47 language tag, validated and spelled canonically.");
    locale
        .method("tag", |locale: &Locale, call: &Call<'_>| push_str(call, locale.as_str()))
        .signature("(self): string")
        .doc("The canonical tag: the locale's identity.");
    locale
        .method("baseName", |locale: &Locale, call: &Call<'_>| push_str(call, locale.base_name()))
        .signature("(self): string")
        .doc("The tag without its extensions.");
    locale
        .method("language", |locale: &Locale, call: &Call<'_>| push_str(call, locale.language()))
        .signature("(self): string")
        .doc("The language subtag; und when undetermined.");
    locale
        .method("script", |locale: &Locale, call: &Call<'_>| -> Result<StackResults> {
            call.push(&locale.script())?;
            Ok(StackResults)
        })
        .signature("(self): string?")
        .doc("The script subtag, or nil.");
    locale
        .method("region", |locale: &Locale, call: &Call<'_>| -> Result<StackResults> {
            call.push(&locale.region())?;
            Ok(StackResults)
        })
        .signature("(self): string?")
        .doc("The region subtag, or nil.");
    locale
        .method("variants", |locale: &Locale, call: &Call<'_>| {
            push_list(call, locale.variants().collect::<Vec<_>>().into_iter())
        })
        .signature("(self): { string }")
        .doc("The variant subtags in canonical order.");
    locale.metamethod("__tostring", |locale: &Locale, call: &Call<'_>| push_str(call, locale.as_str()));
    locale.metamethod("__eq", |locale: &Locale, other: ValueView<'_>| {
        crate::userdata::receiver::<Locale>(other).is_some_and(|other| other == locale)
    });
}

fn describe_plural_rules(d: &mut ExtensionDescriptor) {
    let mut rules = d.userdata::<PluralRules>("dream.intl.PluralRules");
    rules.tag(TagPolicy::Preferred).doc("One locale's CLDR cardinal or ordinal plural rules.");
    rules
        .method("category", |rules: &PluralRules, value: Operand<'_>| -> Result<&'static str> {
            rules
                .category(value)
                .map(Category::name)
                .map_err(|error| Error::runtime(format!("PluralRules:category: {error}")))
        })
        .signature("(self, value: number | integer | string): dream_intl_PluralCategory")
        .doc("The category of a number; a decimal string keeps its visible fraction digits.");
    rules
        .method("categories", |rules: &PluralRules, call: &Call<'_>| {
            push_list(call, rules.categories().map(Category::name))
        })
        .signature("(self): { dream_intl_PluralCategory }")
        .doc("The categories these rules can select, in CLDR order, ending with other.");
    rules
        .method("locale", |rules: &PluralRules, call: &Call<'_>| push_str(call, rules.locale().as_str()))
        .signature("(self): string")
        .doc("The canonical tag of the rules' locale.");
    rules
        .method("type", |rules: &PluralRules| rules.kind().name())
        .signature("(self): dream_intl_PluralType")
        .doc("cardinal or ordinal.");
}

fn describe_decimal_formatter(d: &mut ExtensionDescriptor) {
    let mut decimal = d.userdata::<FormatterHandle>("dream.intl.DecimalFormatter");
    decimal.tag(TagPolicy::Preferred).doc("One locale's CLDR decimal format under fixed options.");
    decimal
        .method("format", |handle: &FormatterHandle, call: &Call<'_>, value: Operand<'_>| -> Result<StackResults> {
            // Formatting calls nothing in Lua, so the buffer is never borrowed twice.
            let mut scratch = handle.scratch.borrow_mut();
            scratch.clear();
            handle
                .formatter
                .format_to(value, &mut scratch)
                .map_err(|error| Error::runtime(format!("DecimalFormatter:format: {error}")))?;
            push_str(call, &scratch)
        })
        .signature("(self, value: number | integer | string): string")
        .doc("A number in the locale's digits and separators; a decimal string formats exactly.");
    decimal
        .method("locale", |handle: &FormatterHandle, call: &Call<'_>| {
            push_str(call, handle.formatter.locale().as_str())
        })
        .signature("(self): string")
        .doc("The canonical tag of the formatter's locale.");
    decimal
        .method("resolvedOptions", |handle: &FormatterHandle, call: &Call<'_>| -> Result<StackResults> {
            let formatter = &handle.formatter;
            let mut frame: Frame<'_> = call.frame();
            let table = frame.push_table(0, 4)?;
            table.raw_set_value(&frame, "locale", formatter.locale().as_str())?;
            table.raw_set_value(&frame, "grouping", formatter.grouping().name())?;
            table.raw_set_value(&frame, "minFractionDigits", &f64::from(formatter.min_fraction_digits()))?;
            table.raw_set_value(&frame, "maxFractionDigits", &f64::from(formatter.max_fraction_digits()))?;
            frame.release();
            Ok(StackResults)
        })
        .signature(
            "(self): { locale: string, grouping: dream_intl_Grouping, minFractionDigits: number, maxFractionDigits: number }",
        )
        .doc("The options the formatter resolved, defaults filled in.");
}

/// The `dream.intl` extension.
#[derive(Clone, Copy, Debug, Default)]
pub struct IntlExtension;

impl Extension for IntlExtension {
    fn id(&self) -> &'static str {
        EXTENSION_ID
    }

    fn describe(&self, d: &mut ExtensionDescriptor) -> Result<()> {
        d.type_alias("dream_intl_PluralCategory", "\"zero\" | \"one\" | \"two\" | \"few\" | \"many\" | \"other\"");
        d.type_alias("dream_intl_PluralType", "\"cardinal\" | \"ordinal\"");
        d.type_alias("dream_intl_Grouping", "\"auto\" | \"never\" | \"min2\"");
        d.type_alias(
            "dream_intl_DecimalOptions",
            "{ grouping: dream_intl_Grouping?, minFractionDigits: number?, maxFractionDigits: number? }",
        );
        describe_locale(d);
        describe_plural_rules(d);
        describe_decimal_formatter(d);

        d.module(MODULE)
            .doc("Internationalization primitives: BCP 47 locale identity, CLDR plural rules and decimal formatting.")
            .function("locale", |tag: &str| -> Result<Owned<Locale>> {
                Locale::parse(tag).map(Owned).map_err(|error| Error::runtime(format!("intl.locale: {error}")))
            })
            .signature("(tag: string) -> dream_intl_Locale")
            .doc("The locale a BCP 47 tag names; underscores read as hyphens.")
            .function("canonicalize", |call: &Call<'_>, tag: &str| -> Result<StackResults> {
                with_canonical(tag, |canonical| push_str(call, canonical))
                    .map_err(|error| Error::runtime(format!("intl.canonicalize: {error}")))?
            })
            .signature("(tag: string) -> string")
            .doc("The canonical spelling of a BCP 47 tag, without making a locale.")
            .function("pluralRules", new_plural_rules)
            .signature("(locale: string | dream_intl_Locale, type: dream_intl_PluralType?) -> dream_intl_PluralRules")
            .doc("A locale's cardinal (the default) or ordinal plural rules, built once.")
            .function("decimalFormatter", new_decimal_formatter)
            .signature(
                "(locale: string | dream_intl_Locale, options: dream_intl_DecimalOptions?) -> dream_intl_DecimalFormatter",
            )
            .doc("A locale's decimal format under options, built once.");
        Ok(())
    }
}
