//! Regular expressions over bytes (feature `bytes-regex`): `bytes.regex(pattern)` compiles a
//! pattern once, and the [`Regex`] it returns matches it against a range of a buffer or string,
//! or against many ranges of one buffer in a single call, `matchSpans`. On text it also answers
//! the first match and its groups, iterates every match, replaces matches by template or
//! function, and splits; `bytes.regexEscape` makes text match literally.
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

use std::cell::Cell;

use regex::bytes::Captures;

use crate::bind::{ArgView, Call, StackResults};
use crate::convert::{BytesView, Exact, NewBuffer};
use crate::error::{Error, Result};
use crate::extension::{ExtensionDescriptor, ModuleDecl, TagPolicy};
use crate::options::Options;
use crate::stack::{Scope, ValueView};
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

/// The text the pattern first matches in the range, or nil.
fn match_text(
    re: &Regex,
    source: BytesView<'_>,
    offset: Option<Exact<i64>>,
    length: Option<Exact<i64>>,
) -> Result<Option<Vec<u8>>> {
    // SAFETY: as `is_match`.
    let data = unsafe { super::bytes(&source) };
    let text = range("bytes.Regex:match", data, offset, length)?;
    Ok(re.regex.find(text).map(|found| found.as_bytes().to_vec()))
}

/// Pushes a match's groups as a table: `[0]` the whole match, `[n]` group `n`, and each named
/// group under its name; a group that took no part is absent.
fn push_captures(scope: &impl Scope, regex: &regex::bytes::Regex, captures: &Captures<'_>) -> Result<()> {
    let mut frame = scope.frame();
    let table = frame.push_table(captures.len(), 0)?;
    for (index, group) in captures.iter().enumerate() {
        if let Some(group) = group {
            frame.push(group.as_bytes())?;
            table.raw_set_index(&frame, index as i64)?;
        }
    }
    for name in regex.capture_names().flatten() {
        if let Some(group) = captures.name(name) {
            table.raw_set_value(&frame, name, group.as_bytes())?;
        }
    }
    frame.release();
    Ok(())
}

/// The first match's groups in the range, or nil.
fn captures(
    re: &Regex,
    call: &Call<'_>,
    source: BytesView<'_>,
    offset: Option<Exact<i64>>,
    length: Option<Exact<i64>>,
) -> Result<StackResults> {
    // SAFETY: as `is_match`; the table is built from copies.
    let data = unsafe { super::bytes(&source) };
    let text = range("bytes.Regex:captures", data, offset, length)?;
    match re.regex.captures(text) {
        Some(found) => push_captures(call, &re.regex, &found)?,
        None => {
            call.push(&())?;
        }
    }
    Ok(StackResults)
}

/// An iterator over the matches in the range: each step answers the text, its start and its end
/// (0-based, end exclusive, as `find`), then nothing. The range is copied when the iterator is
/// made, so a buffer changed afterwards doesn't change what it yields.
fn matches(
    re: &Regex,
    call: &Call<'_>,
    source: BytesView<'_>,
    offset: Option<Exact<i64>>,
    length: Option<Exact<i64>>,
) -> Result<crate::value::Function> {
    // SAFETY: as `is_match`; the range is copied out.
    let data = unsafe { super::bytes(&source) };
    let text = range("bytes.Regex:matches", data, offset, length)?.to_vec();
    let base = offset.map_or(0, |offset| offset.0) as f64;
    let regex = re.regex.clone();
    let position = Cell::new(Some(0usize));
    crate::bind::function(
        call.stack(),
        &["dream"],
        "dream.bytes.Regex.matches",
        move |call: &Call<'_>, _: ArgView<'_>| -> Result<StackResults> {
            let Some(at) = position.get() else { return Ok(StackResults) };
            let Some(found) = regex.find_at(&text, at) else {
                position.set(None);
                return Ok(StackResults);
            };
            // An empty match moves on a byte, so iteration always ends.
            let next = if found.is_empty() { found.end() + 1 } else { found.end() };
            position.set((next <= text.len()).then_some(next));
            call.push(found.as_bytes())?;
            call.push(&(base + found.start() as f64))?;
            call.push(&(base + found.end() as f64))?;
            Ok(StackResults)
        },
    )
}

/// What a match is replaced with: a string, in which `$1` and `${name}` expand to groups (`$$` is
/// a dollar), or a function given the match's groups table, whose string replaces the match;
/// `nil` or `false` keeps the match, which then counts neither as replaced nor toward a limit.
enum Replacement {
    Text(Vec<u8>),
    Function(crate::value::Function),
}

impl Replacement {
    fn read(what: &str, value: ValueView<'_>) -> Result<Replacement> {
        if value.is_string() {
            return Ok(Replacement::Text(value.read::<&[u8]>()?.to_vec()));
        }
        if value.is_function() {
            return Ok(Replacement::Function(crate::value::Function::from_value(crate::value::Value::store(value)?)?));
        }
        Err(Error::runtime(format!(
            "{what}: a replacement is a string or a function, not {}",
            crate::diagnostics::object_type_name(value)
        )))
    }

    /// `text` with up to `limit` matches replaced (0 for all), and how many were.
    fn apply(
        &self,
        what: &str,
        call: &Call<'_>,
        regex: &regex::bytes::Regex,
        text: &[u8],
        limit: usize,
    ) -> Result<(Vec<u8>, f64)> {
        let mut out = Vec::with_capacity(text.len());
        let mut last = 0;
        let mut count = 0usize;
        for found in regex.captures_iter(text) {
            if limit != 0 && count == limit {
                break;
            }
            let whole = found.get(0).expect("group 0 is the whole match");
            out.extend_from_slice(&text[last..whole.start()]);
            match self {
                Replacement::Text(replacement) => {
                    found.expand(replacement, &mut out);
                    count += 1;
                }
                Replacement::Function(function) => {
                    let replaced = call.with_frame(|frame| {
                        push_captures(frame, regex, &found)?;
                        let groups = frame.top_value();
                        function.invoke_with(frame, (groups,), |_, returned| {
                            if returned.is_nil() || (returned.is_boolean() && !returned.read::<bool>()?) {
                                return Ok(None);
                            }
                            if returned.is_string() {
                                return Ok(Some(returned.read::<&[u8]>()?.to_vec()));
                            }
                            Err(Error::runtime(format!(
                                "{what}: a replacement function returns a string, nil or false, not {}",
                                crate::diagnostics::object_type_name(returned)
                            )))
                        })
                    })?;
                    match replaced {
                        Some(replacement) => {
                            out.extend_from_slice(&replacement);
                            count += 1;
                        }
                        None => out.extend_from_slice(whole.as_bytes()),
                    }
                }
            }
            last = whole.end();
        }
        out.extend_from_slice(&text[last..]);
        Ok((out, count as f64))
    }
}

fn replace(re: &Regex, call: &Call<'_>, source: BytesView<'_>, replacement: ValueView<'_>) -> Result<(Vec<u8>, f64)> {
    const WHAT: &str = "bytes.Regex:replace";
    let replacement = Replacement::read(WHAT, replacement)?;
    // SAFETY: copied out before the replacement function can run and change a buffer.
    let text = unsafe { super::bytes(&source) }.to_vec();
    replacement.apply(WHAT, call, &re.regex, &text, 1)
}

fn replace_all(
    re: &Regex,
    call: &Call<'_>,
    source: BytesView<'_>,
    replacement: ValueView<'_>,
    limit: Option<Exact<i64>>,
) -> Result<(Vec<u8>, f64)> {
    const WHAT: &str = "bytes.Regex:replaceAll";
    let replacement = Replacement::read(WHAT, replacement)?;
    let limit = match limit {
        Some(limit) => super::count(WHAT, limit)?,
        None => 0,
    };
    // SAFETY: as `replace`.
    let text = unsafe { super::bytes(&source) }.to_vec();
    replacement.apply(WHAT, call, &re.regex, &text, limit)
}

/// The pieces between matches; with `limit`, at most that many, the last holding the rest.
fn split(re: &Regex, call: &Call<'_>, source: BytesView<'_>, limit: Option<Exact<i64>>) -> Result<StackResults> {
    const WHAT: &str = "bytes.Regex:split";
    // SAFETY: the pieces are copied into strings before anything else runs.
    let text = unsafe { super::bytes(&source) };
    let pieces: Vec<&[u8]> = match limit {
        Some(limit) if limit.0 != 0 => re.regex.splitn(text, super::count(WHAT, limit)?).collect(),
        _ => re.regex.split(text).collect(),
    };
    let mut frame = call.frame();
    let table = frame.push_table(pieces.len(), 0)?;
    for (index, piece) in pieces.into_iter().enumerate() {
        frame.push(piece)?;
        table.raw_set_index(&frame, index as i64 + 1)?;
    }
    frame.release();
    Ok(StackResults)
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
    regex
        .method("match", match_text)
        .signature("(self, source: buffer | string, offset: number?, length: number?): string?")
        .doc("The text the pattern first matches in the range, or nil.");
    regex
        .method("captures", captures)
        .signature(
            "(self, source: buffer | string, offset: number?, length: number?): { [number | string]: string }?",
        )
        .doc("The first match's groups: [0] the whole match, [n] group n, named groups by name; nil when nothing matches.");
    regex
        .method("matches", matches)
        .signature(
            "(self, source: buffer | string, offset: number?, length: number?): () -> (string?, number?, number?)",
        )
        .doc("An iterator over every match in the range: its text, start and end (0-based, end exclusive).");
    regex
        .method("replace", replace)
        .signature("(self, source: buffer | string, replacement: string | ({ [number | string]: string }) -> string?): (string, number)")
        .doc("source with the first match replaced, and how many were (0 or 1). In a string,  and  expand to groups; a function gets the groups and returns the replacement, or nil or false to keep the match.");
    regex
        .method("replaceAll", replace_all)
        .signature("(self, source: buffer | string, replacement: string | ({ [number | string]: string }) -> string?, limit: number?): (string, number)")
        .doc("source with every match replaced (at most limit when given), and how many were.");
    regex
        .method("split", split)
        .signature("(self, source: buffer | string, limit: number?): { string }")
        .doc("The pieces between matches; with limit, at most that many, the last holding the rest.");
    regex.method("pattern", |re: &Regex| re.regex.as_str().to_owned()).signature("(self): string");
    regex.metamethod("__tostring", |re: &Regex| re.regex.as_str().to_owned());
}

pub(crate) fn describe(module: &mut ModuleDecl) {
    module
        .function("regex", compile)
        .signature(
            "(pattern: string, options: { caseInsensitive: boolean?, unicode: boolean?, multiLine: boolean?, dotAll: boolean? }?) -> dream_bytes_Regex",
        )
        .doc("A pattern compiled once, to match over bytes: one range, or many ranges of one buffer in one call.");
    module
        .function("regexEscape", |text: &str| regex::escape(text))
        .signature("(text: string) -> string")
        .doc("text with every character a pattern treats specially escaped, so it matches literally.");
}
