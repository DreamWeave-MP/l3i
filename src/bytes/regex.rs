//! Regular expressions over bytes (feature `bytes-regex`): `bytes.regex(pattern)` compiles a
//! pattern once, and the [`Regex`] it returns matches it against a range of a buffer or string,
//! or against many ranges of one buffer in a single call, `matchSpans`.
//!
//! `matchSpans` answers with a buffer of one byte per range, 1 where the pattern matches. It is
//! for the script that has a thousand small texts to test, the fields of every
//! record in a file, say: laid out in one buffer with their `(start, end)` pairs in another (the
//! layout `bytes.frame` returns), they cost one call instead of a thousand, and no string is made
//! for any of them. Each range is matched on its own, as if it were the whole input: `^` and `$`
//! anchor to its ends.
//!
//! The syntax is the regex crate's, matched over bytes: Unicode-aware by default, so on UTF-8 it
//! behaves as a string regex would, and `unicode = false` (or `(?-u)`) for byte-level matching of
//! data in another encoding. No look-around and no backreferences; matching is linear in the
//! input.

use crate::bind::Call;
use crate::convert::{BytesView, Exact, NewBuffer};
use crate::error::{Error, Result};
use crate::extension::{ExtensionDescriptor, ModuleDecl, TagPolicy};
use crate::options::Options;
use crate::stack::ValueView;
use crate::userdata::Owned;

/// A compiled pattern (`dream.bytes.Regex`, from `bytes.regex(pattern, options?)`).
pub struct Regex {
    regex: regex::bytes::Regex,
}

// SAFETY: plain Rust data; the destructor never touches the Lua API.
unsafe impl crate::userdata::Userdata for Regex {
    const NAME: &'static str = "dream.bytes.Regex";
}

/// The bytes `[offset, offset + length)` of `data`, the rest of it by default.
fn range<'a>(what: &str, data: &'a [u8], offset: Option<Exact<i64>>, length: Option<Exact<i64>>) -> Result<&'a [u8]> {
    let offset = offset.unwrap_or(Exact(0));
    let start = super::count(what, offset)?;
    let length = match length {
        Some(length) => super::count(what, length)?,
        None => data.len().saturating_sub(start),
    };
    let start = super::span(what, data.len(), offset, length)?;
    Ok(&data[start..start + length])
}

fn compile(call: &Call<'_>, pattern: &str, options: Option<ValueView<'_>>) -> Result<Owned<Regex>> {
    const WHAT: &str = "bytes.regex";
    let mut builder = regex::bytes::RegexBuilder::new(pattern);
    if let Some(options) = options {
        Options::read(call, options, WHAT, |o| {
            builder.case_insensitive(o.optional::<bool>("caseInsensitive")?.unwrap_or(false));
            builder.unicode(o.optional::<bool>("unicode")?.unwrap_or(true));
            builder.multi_line(o.optional::<bool>("multiLine")?.unwrap_or(false));
            builder.dot_matches_new_line(o.optional::<bool>("dotAll")?.unwrap_or(false));
            Ok(())
        })?;
    }
    let regex =
        builder.build().map_err(|error| Error::runtime(format!("{WHAT}: invalid pattern '{pattern}': {error}")))?;
    Ok(Owned(Regex { regex }))
}

fn is_match(re: &Regex, source: BytesView<'_>, offset: Option<Exact<i64>>, length: Option<Exact<i64>>) -> Result<bool> {
    // SAFETY: matching reads the view and calls nothing.
    let data = unsafe { super::bytes(&source) };
    Ok(re.regex.is_match(range("bytes.Regex:isMatch", data, offset, length)?))
}

fn find(
    re: &Regex,
    source: BytesView<'_>,
    offset: Option<Exact<i64>>,
    length: Option<Exact<i64>>,
) -> Result<(Option<f64>, Option<f64>)> {
    // SAFETY: as `is_match`.
    let data = unsafe { super::bytes(&source) };
    let text = range("bytes.Regex:find", data, offset, length)?;
    let base = offset.map_or(0, |offset| offset.0) as f64;
    Ok(match re.regex.find(text) {
        Some(found) => (Some(base + found.start() as f64), Some(base + found.end() as f64)),
        None => (None, None),
    })
}

/// One byte per `(start, end)` `u32` pair in `spans`: 1 where the pattern matches that range of
/// `source`, 0 where it doesn't.
fn match_spans(
    re: &Regex,
    source: BytesView<'_>,
    spans: BytesView<'_>,
    count: Option<Exact<i64>>,
) -> Result<NewBuffer> {
    const WHAT: &str = "bytes.Regex:matchSpans";
    // SAFETY: as `is_match`; both views are only read.
    let (data, pairs) = unsafe { (super::bytes(&source), super::bytes(&spans)) };
    let available = pairs.len() / 8;
    let count = match count {
        Some(count) => super::count(WHAT, count)?,
        None => available,
    };
    if count > available {
        return Err(Error::runtime(format!("{WHAT}: {count} spans asked for, but the span buffer holds {available}")));
    }
    let mut out = vec![0u8; count];
    for index in 0..count {
        let pair = &pairs[index * 8..index * 8 + 8];
        let start = u32::from_le_bytes([pair[0], pair[1], pair[2], pair[3]]) as usize;
        let end = u32::from_le_bytes([pair[4], pair[5], pair[6], pair[7]]) as usize;
        if start > end || end > data.len() {
            return Err(Error::runtime(format!(
                "{WHAT}: span {index} ({start}..{end}) is outside the source (length {})",
                data.len()
            )));
        }
        out[index] = u8::from(re.regex.is_match(&data[start..end]));
    }
    Ok(NewBuffer(out))
}

pub(crate) fn describe_regex(d: &mut ExtensionDescriptor) {
    let mut regex = d.userdata::<Regex>("dream.bytes.Regex");
    regex.tag(TagPolicy::Never).doc("A compiled regular expression over bytes.");
    regex
        .method("isMatch", is_match)
        .signature("(self, source: buffer | string, offset: number?, length: number?): boolean");
    regex
        .method("find", find)
        .signature("(self, source: buffer | string, offset: number?, length: number?): (number?, number?)");
    regex
        .method("matchSpans", match_spans)
        .signature("(self, source: buffer | string, spans: buffer | string, count: number?): buffer");
    regex.method("pattern", |re: &Regex| re.regex.as_str().to_owned()).signature("(self): string");
}

pub(crate) fn describe(module: &mut ModuleDecl) {
    module
        .function("regex", compile)
        .signature(
            "(pattern: string, options: { caseInsensitive: boolean?, unicode: boolean?, multiLine: boolean?, dotAll: boolean? }?) -> dream_bytes_Regex",
        )
        .doc("A pattern compiled once, to match over bytes: one range, or many ranges of one buffer in one call.");
}
