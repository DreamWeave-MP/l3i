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

mod locale;
mod operand;
mod plural;

pub use locale::{Locale, LocaleError, canonicalize, with_canonical};
pub use operand::{NumberError, Operand};
pub use plural::{Category, PluralKind, PluralRules};

use std::fmt;

use crate::bind::{Call, StackResults};
use crate::error::{Error, Result};
use crate::extension::{Extension, ExtensionDescriptor, TagPolicy};
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

        d.module(MODULE)
            .doc("Internationalization primitives: BCP 47 locale identity and CLDR plural rules.")
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
            .doc("A locale's cardinal (the default) or ordinal plural rules, built once.");
        Ok(())
    }
}
