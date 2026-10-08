//! Locale identity: BCP 47 language tags, validated and spelled canonically.

use std::fmt::{self, Write as _};

use writeable::Writeable;

/// The longest tag whose canonical spelling is made without a heap allocation.
const INLINE: usize = 64;

/// A validated BCP 47 language tag in its canonical spelling: a language, an optional script
/// and region, variants, and extensions.
///
/// Parsing is ICU4X's. Each subtag is checked against BCP 47's syntax and spelled canonically:
/// the language lowercase, the script title case, the region uppercase, variants lowercase and
/// sorted, extension keys and values lowercase and sorted, so `DE-de-u-NU-latn-CA-gregory` is
/// `de-DE-u-ca-gregory-nu-latn`. Underscores separate subtags as hyphens do (`zh_Hant_TW` is
/// `zh-Hant-TW`), the spelling ICU4C and POSIX locale names use.
///
/// Canonicalization is syntactic. CLDR's alias replacement (`iw` to `he`, `sh` to `sr-Latn`) and
/// likely subtags are not applied, so tags that differ only there are different locales.
///
/// Two locales are equal when their canonical spellings are, which makes [`Locale::as_str`]
/// the identity to key a cache with.
#[derive(Clone, Debug)]
pub struct Locale {
    tag: icu_locale_core::Locale,
    text: Box<str>,
    /// The length of the language, script, region and variants: the tag without extensions.
    base: usize,
}

/// Why a string is not a language tag.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum LocaleError {
    /// The string is empty.
    Empty,
    /// The string breaks BCP 47's syntax.
    Invalid {
        /// The string as given.
        input: String,
        /// ICU4X's reason.
        reason: String,
    },
}

impl fmt::Display for LocaleError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LocaleError::Empty => f.write_str("an empty string is not a language tag"),
            LocaleError::Invalid { input, reason } => write!(f, "'{input}' is not a BCP 47 language tag: {reason}"),
        }
    }
}

impl std::error::Error for LocaleError {}

/// The tag ICU4X parses from `input`, underscores read as hyphens.
fn parse_tag(input: &str) -> Result<icu_locale_core::Locale, LocaleError> {
    if input.is_empty() {
        return Err(LocaleError::Empty);
    }
    let parsed = if !input.contains('_') {
        icu_locale_core::Locale::try_from_str(input)
    } else if input.len() <= INLINE {
        let mut hyphenated = [0u8; INLINE];
        let hyphenated = &mut hyphenated[..input.len()];
        hyphenated.copy_from_slice(input.as_bytes());
        for byte in hyphenated.iter_mut().filter(|byte| **byte == b'_') {
            *byte = b'-';
        }
        icu_locale_core::Locale::try_from_utf8(hyphenated)
    } else {
        icu_locale_core::Locale::try_from_str(&input.replace('_', "-"))
    };
    parsed.map_err(|error| {
        let mut reason = error.to_string();
        if let Some(first) = reason.get_mut(..1) {
            first.make_ascii_lowercase();
        }
        LocaleError::Invalid { input: input.to_owned(), reason }
    })
}

/// A canonical spelling: inline up to [`INLINE`] bytes, on the heap past that.
enum Spelling {
    Inline([u8; INLINE], usize),
    Heap(String),
}

impl Spelling {
    fn as_str(&self) -> &str {
        match self {
            // Only whole `&str`s are ever written, so the bytes are UTF-8.
            Spelling::Inline(bytes, len) => std::str::from_utf8(&bytes[..*len]).unwrap_or_default(),
            Spelling::Heap(text) => text,
        }
    }
}

impl fmt::Write for Spelling {
    fn write_str(&mut self, piece: &str) -> fmt::Result {
        match self {
            Spelling::Inline(bytes, len) if *len + piece.len() <= INLINE => {
                bytes[*len..*len + piece.len()].copy_from_slice(piece.as_bytes());
                *len += piece.len();
            }
            Spelling::Inline(bytes, len) => {
                let mut text = String::with_capacity(*len + piece.len());
                text.push_str(std::str::from_utf8(&bytes[..*len]).unwrap_or_default());
                text.push_str(piece);
                *self = Spelling::Heap(text);
            }
            Spelling::Heap(text) => text.push_str(piece),
        }
        Ok(())
    }
}

/// The canonical spelling of `input`, handed to `f`; no heap allocation for a tag of up to 64
/// bytes without extensions.
pub fn with_canonical<R>(input: &str, f: impl FnOnce(&str) -> R) -> Result<R, LocaleError> {
    let tag = parse_tag(input)?;
    let mut spelling = Spelling::Inline([0; INLINE], 0);
    // Writing into a `Spelling` cannot fail.
    let _ = tag.write_to(&mut spelling);
    Ok(f(spelling.as_str()))
}

/// The canonical spelling of `input`.
pub fn canonicalize(input: &str) -> Result<String, LocaleError> {
    with_canonical(input, str::to_owned)
}

impl Locale {
    /// Parses and canonicalizes a BCP 47 tag (`en`, `pt-BR`, `zh-Hant-TW`, `de-DE-1996`,
    /// `ar-EG-u-nu-latn`).
    pub fn parse(input: &str) -> Result<Locale, LocaleError> {
        let tag = parse_tag(input)?;
        let mut text = String::with_capacity(input.len());
        // Writing into a `String` cannot fail.
        let _ = tag.id.write_to(&mut text);
        let base = text.len();
        if !tag.extensions.is_empty() {
            let _ = write!(text, "-{}", tag.extensions);
        }
        Ok(Locale { tag, text: text.into_boxed_str(), base })
    }

    /// The canonical tag (`pt-BR`).
    pub fn as_str(&self) -> &str {
        &self.text
    }

    /// The tag without its extensions (`ar-EG` of `ar-EG-u-nu-latn`).
    pub fn base_name(&self) -> &str {
        &self.text[..self.base]
    }

    /// The language subtag (`pt` of `pt-BR`; `und` when the language is undetermined).
    pub fn language(&self) -> &str {
        self.tag.id.language.as_str()
    }

    /// The script subtag (`Hant` of `zh-Hant-TW`).
    pub fn script(&self) -> Option<&str> {
        self.tag.id.script.as_ref().map(icu_locale_core::subtags::Script::as_str)
    }

    /// The region subtag (`BR` of `pt-BR`).
    pub fn region(&self) -> Option<&str> {
        self.tag.id.region.as_ref().map(icu_locale_core::subtags::Region::as_str)
    }

    /// The variant subtags in canonical order (`1996` of `de-DE-1996`).
    pub fn variants(&self) -> impl Iterator<Item = &str> {
        self.tag.id.variants.iter().map(icu_locale_core::subtags::Variant::as_str)
    }
}

impl PartialEq for Locale {
    fn eq(&self, other: &Locale) -> bool {
        self.text == other.text
    }
}

impl Eq for Locale {}

impl std::hash::Hash for Locale {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.text.hash(state);
    }
}

impl fmt::Display for Locale {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.text)
    }
}

impl std::str::FromStr for Locale {
    type Err = LocaleError;

    fn from_str(input: &str) -> Result<Locale, LocaleError> {
        Locale::parse(input)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn canonical(input: &str) -> String {
        Locale::parse(input).unwrap().as_str().to_owned()
    }

    #[test]
    fn casing_separators_and_order_are_canonical() {
        assert_eq!(canonical("pt-br"), "pt-BR");
        assert_eq!(canonical("ZH_hant_tw"), "zh-Hant-TW");
        assert_eq!(canonical("de-DE-1996"), "de-DE-1996");
        assert_eq!(canonical("EN-us-POSIX"), "en-US-posix");
        assert_eq!(canonical("sl-rozaj-biske-1994"), "sl-1994-biske-rozaj");
        assert_eq!(canonical("DE-de-u-NU-latn-CA-gregory"), "de-DE-u-ca-gregory-nu-latn");
        assert_eq!(canonical("en-x-Private"), "en-x-private");
        assert_eq!(canonical("und"), "und");
        assert_eq!(Locale::parse("EN").unwrap(), Locale::parse("en").unwrap());
    }

    #[test]
    fn the_canonical_spelling_needs_no_locale() {
        for input in ["pt-br", "ZH_hant_tw", "DE-de-u-NU-latn-CA-gregory", "en-x-private", "en-a-bbb-c-ddd"] {
            assert_eq!(canonicalize(input).unwrap(), canonical(input), "{input}");
        }
        // Twelve distinct eight-letter variants, already in canonical order.
        let variants: Vec<String> = (b'a'..=b'l').map(|letter| char::from(letter).to_string().repeat(8)).collect();
        let long = format!("en_{}", variants.join("_"));
        assert!(long.len() > INLINE);
        assert_eq!(canonicalize(&long).unwrap(), long.replace('_', "-"));
        assert_eq!(canonical(&long), long.replace('_', "-"));
    }

    #[test]
    fn parts_and_the_base_name() {
        let locale = Locale::parse("de-latn-de-1996-u-nu-latn").unwrap();
        assert_eq!(locale.language(), "de");
        assert_eq!(locale.script(), Some("Latn"));
        assert_eq!(locale.region(), Some("DE"));
        assert_eq!(locale.variants().collect::<Vec<_>>(), ["1996"]);
        assert_eq!(locale.base_name(), "de-Latn-DE-1996");
        assert_eq!(locale.as_str(), "de-Latn-DE-1996-u-nu-latn");
        let bare = Locale::parse("fr").unwrap();
        assert_eq!((bare.script(), bare.region(), bare.base_name()), (None, None, "fr"));
        assert_eq!(bare.variants().count(), 0);
    }

    #[test]
    fn malformed_tags_say_why() {
        assert_eq!(Locale::parse(""), Err(LocaleError::Empty));
        for input in
            ["en-US-", "en--US", "x2", "3", "english", "root", "i-klingon", "zh-cmn-Hans", " en", "en US", "en-ü"]
        {
            match Locale::parse(input) {
                Err(LocaleError::Invalid { input: given, reason }) => {
                    assert_eq!(given, input);
                    assert!(reason.chars().next().is_some_and(char::is_lowercase), "{reason}");
                }
                other => panic!("{input}: {other:?}"),
            }
            assert!(canonicalize(input).is_err(), "{input}");
        }
        assert_eq!(
            Locale::parse("english").unwrap_err().to_string(),
            "'english' is not a BCP 47 language tag: the given language subtag is invalid"
        );
    }
}
