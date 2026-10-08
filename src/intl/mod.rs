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

mod locale;

pub use locale::{Locale, LocaleError, canonicalize, with_canonical};

use crate::bind::{Call, StackResults};
use crate::error::{Error, Result};
use crate::extension::{Extension, ExtensionDescriptor, TagPolicy};
use crate::stack::{Frame, Scope, ValueView};
use crate::userdata::Owned;

/// The extension id.
pub const EXTENSION_ID: &str = "dream.intl";
/// The module path.
pub const MODULE: &str = "@dream/intl";

// SAFETY: the payload holds no Lua references.
unsafe impl crate::userdata::Userdata for Locale {
    const NAME: &'static str = "dream.intl.Locale";
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

        d.module(MODULE)
            .doc("Internationalization primitives: BCP 47 locale identity.")
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
            .doc("The canonical spelling of a BCP 47 tag, without making a locale.");
        Ok(())
    }
}
