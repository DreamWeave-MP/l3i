//! Bytes for scripts that parse foreign file formats: the `dream.bytes` extension, module
//! `@dream/bytes`.
//!
//! Luau's own `buffer` library already reads and writes every little-endian width, 64-bit
//! integers and bit fields, and its code generator lowers all of it to native loads and stores.
//! This module adds what a script cannot do fast, or at all, on top of that:
//!
//! - **Searching and comparing**: `find`, `rfind`, `count`, `equals`, `compare`, `startsWith`,
//!   `slice`, `toHex`, `fromHex`, `toBase64`, `fromBase64`, over a `buffer` or a string without
//!   copying.
//! - **Strings inside records**: `readCString` and `writeCString` for NUL-terminated and
//!   fixed-width fields, and `readVarint`, `readSignedVarint`, `writeVarint`,
//!   `writeSignedVarint` for LEB128.
//! - **The widths and orders `buffer` lacks**: big-endian 16, 32 and 64-bit integers and
//!   floats, 24-bit integers in either order, and IEEE half floats, as module functions and as
//!   methods on the [`Math`] receiver (`bytes.math()`), whose integer forms lower to native
//!   code under `jit` when the script annotates it (`local B: dream_bytes_Math = bytes.math()`).
//!   A big-endian float in native code is two lowered steps in script: read the integer form,
//!   write it into a scratch buffer with `buffer.writeu32`, read it back with `buffer.readf32`.
//! - **Codecs** (`bytes-codecs`): `inflate` and `deflate` in zlib, raw and gzip framing, LZ4
//!   blocks and frames, zstd and LZMA/XZ decoding.
//! - **Digests** (`bytes-digests`): CRC-32, Adler-32, FNV-1a, xxHash and the cryptographic
//!   digests, one-shot or through a `Hasher` for data that arrives in pieces.
//! - **Text** (`bytes-text`): decoding and encoding every WHATWG label, for the codepages old
//!   formats were written in.
//! - **Framing**: `frame` walks a run of tag-length-payload chunks natively and returns where
//!   each one is ([`framing`]).
//! - **Regular expressions** (`bytes-regex`): `regex` compiles a pattern once; its `matchSpans`
//!   tests many ranges of one buffer in one call ([`regex`]).
//!
//! Every input that is "some bytes" is a `buffer | string` ([`crate::convert::BytesView`]);
//! every output that is bytes is a new `buffer` ([`crate::convert::NewBuffer`]). Offsets are
//! zero-based, as the `buffer` library's are. Sizes and offsets are numbers; 64-bit values are
//! Luau integers. Nothing here allocates on a read; a produced buffer is allocated once, at its
//! final size.
//!
//! ```lua
//! local bytes = require('@dream/bytes')
//! local header = file:readRange(0, 64)
//! assert(bytes.equals(bytes.slice(header, 0, 4), "FORM"))
//! local size = bytes.readu32be(header, 4)
//! local name, next = bytes.readCString(header, 8, 32)
//! local B: dream_bytes_Math = bytes.math()
//! for i = 0, count - 1 do
//!     local id = B:readu16be(table, i * 2)   -- native under jit
//! end
//! ```

#[cfg(feature = "bytes-codecs")]
pub mod codecs;
#[cfg(feature = "bytes-digests")]
pub mod digests;
pub mod framing;
#[cfg(feature = "jit")]
pub mod lowering;
pub mod numeric;
#[cfg(feature = "bytes-regex")]
pub mod regex;
#[cfg(feature = "bytes-text")]
pub mod text;

use crate::bind::{Call, StackResults};
use crate::byte_rules::{ByteRules, one_byte};
use crate::convert::{BufferView, BytesView, Exact, Integer, NewBuffer};
use crate::error::{Error, Result};
use crate::extension::{Extension, ExtensionDescriptor, ModuleDecl};
use crate::options::Options;
use crate::stack::{Scope, ValueView};

/// The extension id.
pub const EXTENSION_ID: &str = "dream.bytes";
/// The module path.
pub const MODULE: &str = "@dream/bytes";

/// The `dream.bytes` extension.
#[derive(Clone, Copy, Debug, Default)]
pub struct BytesExtension;

/// The numeric receiver (`dream.bytes.Math`, from `bytes.math()`): the big-endian, 24-bit and
/// half-float reads and writes as methods, lowered to native code under `jit`; see
/// [`numeric`] for the list and [`lowering::ByteMath`] for the native version.
#[derive(Clone, Copy, Debug, Default)]
pub struct Math;

// SAFETY: no payload, no Lua references.
unsafe impl crate::userdata::Userdata for Math {
    const NAME: &'static str = "dream.bytes.Math";
}

/// A checked range `offset..offset + len` inside `total` bytes, for `what`.
pub(crate) fn span(what: &str, total: usize, offset: Exact<i64>, len: usize) -> Result<usize> {
    let start =
        usize::try_from(offset.0).map_err(|_| Error::runtime(format!("{what}: negative offset {}", offset.0)))?;
    match start.checked_add(len) {
        Some(end) if end <= total => Ok(start),
        _ => Err(Error::runtime(format!(
            "{what}: {len} byte{} at offset {start} past the end (length {total})",
            if len == 1 { "" } else { "s" }
        ))),
    }
}

/// A non-negative count or length.
pub(crate) fn count(what: &str, value: Exact<i64>) -> Result<usize> {
    usize::try_from(value.0).map_err(|_| Error::runtime(format!("{what}: negative length {}", value.0)))
}

/// The bytes of a view for the duration of a call that never touches Lua.
///
/// # Safety
///
/// The caller neither calls into Lua nor writes any buffer while the slice lives.
#[inline]
unsafe fn bytes<'a>(view: &'a BytesView<'a>) -> &'a [u8] {
    // SAFETY: forwarded contract.
    unsafe { view.bytes_unchecked() }
}

fn find(haystack: BytesView<'_>, needle: BytesView<'_>, start: Option<Exact<i64>>) -> Result<Option<f64>> {
    let start = start.map_or(Ok(0), |s| count("bytes.find start", s))?;
    // SAFETY: a pure search over two immutable views; nothing writes or calls Lua meanwhile.
    let (hay, needle) = unsafe { (bytes(&haystack), bytes(&needle)) };
    if start > hay.len() {
        return Ok(None);
    }
    Ok(memchr::memmem::find(&hay[start..], needle).map(|at| (at + start) as f64))
}

fn rfind(haystack: BytesView<'_>, needle: BytesView<'_>, end: Option<Exact<i64>>) -> Result<Option<f64>> {
    // SAFETY: as `find`.
    let (hay, needle) = unsafe { (bytes(&haystack), bytes(&needle)) };
    let end = end.map_or(Ok(hay.len()), |e| count("bytes.rfind end", e))?.min(hay.len());
    Ok(memchr::memmem::rfind(&hay[..end], needle).map(|at| at as f64))
}

fn count_matches(haystack: BytesView<'_>, needle: BytesView<'_>) -> f64 {
    // SAFETY: as `find`.
    let (hay, needle) = unsafe { (bytes(&haystack), bytes(&needle)) };
    if needle.is_empty() {
        return 0.0;
    }
    memchr::memmem::find_iter(hay, needle).count() as f64
}

fn equals(a: BytesView<'_>, b: BytesView<'_>) -> bool {
    // SAFETY: as `find`.
    unsafe { bytes(&a) == bytes(&b) }
}

fn compare(
    a: BytesView<'_>,
    a_offset: Exact<i64>,
    b: BytesView<'_>,
    b_offset: Exact<i64>,
    len: Exact<i64>,
) -> Result<f64> {
    let len = count("bytes.compare", len)?;
    let a_start = span("bytes.compare a", a.len(), a_offset, len)?;
    let b_start = span("bytes.compare b", b.len(), b_offset, len)?;
    // SAFETY: as `find`.
    let (a, b) = unsafe { (bytes(&a), bytes(&b)) };
    Ok(match a[a_start..a_start + len].cmp(&b[b_start..b_start + len]) {
        std::cmp::Ordering::Less => -1.0,
        std::cmp::Ordering::Equal => 0.0,
        std::cmp::Ordering::Greater => 1.0,
    })
}

fn starts_with(haystack: BytesView<'_>, prefix: BytesView<'_>, offset: Option<Exact<i64>>) -> Result<bool> {
    let start = offset.map_or(Ok(0), |o| count("bytes.startsWith offset", o))?;
    // SAFETY: as `find`.
    let (hay, prefix) = unsafe { (bytes(&haystack), bytes(&prefix)) };
    Ok(start <= hay.len() && hay[start..].starts_with(prefix))
}

fn slice(source: BytesView<'_>, offset: Exact<i64>, len: Exact<i64>) -> Result<NewBuffer> {
    let len = count("bytes.slice", len)?;
    let start = span("bytes.slice", source.len(), offset, len)?;
    // SAFETY: as `find`; the copy completes before anything else runs.
    Ok(NewBuffer(unsafe { bytes(&source) }[start..start + len].to_vec()))
}

/// `bytes.translate(text, from, to, { collapse?, trimStart?, trimEnd? }?)`: every byte found in
/// `from` replaced by the byte at the same position in `to`, like `tr`; then runs of `collapse`
/// made one, and `trimStart` and `trimEnd` removed from the ends. Nothing to change returns
/// `text` itself, with no copy, so a key already in its normal spelling costs one scan.
fn translate(
    call: &Call<'_>,
    text: ValueView<'_>,
    from: &[u8],
    to: &[u8],
    options: Option<ValueView<'_>>,
) -> Result<StackResults> {
    const WHAT: &str = "bytes.translate";
    if from.len() != to.len() {
        return Err(Error::runtime(format!(
            "{WHAT}: from and to must be the same length, got {} and {}",
            from.len(),
            to.len()
        )));
    }
    if text.type_of() != crate::stack::Type::String {
        return Err(Error::runtime(format!("{WHAT}: text must be a string, got {}", text.type_of().name())));
    }
    let mut rules = ByteRules::identity();
    // A byte named twice in from maps as its first appearance says.
    for (&old, &new) in from.iter().zip(to).rev() {
        rules.map[usize::from(old)] = new;
    }
    if let Some(options) = options.filter(|view| !view.is_nil()) {
        Options::read(call, options, WHAT, |o| {
            rules.collapse = o.optional_bytes("collapse", |text| one_byte(WHAT, "collapse", text))?.flatten();
            rules.trim_start = o.optional_bytes("trimStart", |text| one_byte(WHAT, "trimStart", text))?.flatten();
            rules.trim_end = o.optional_bytes("trimEnd", |text| one_byte(WHAT, "trimEnd", text))?.flatten();
            Ok(())
        })?;
    }
    let source = text.read::<&[u8]>()?;
    if rules.is_normal(source) {
        let mut frame = call.frame();
        frame.push_value(text)?;
        frame.release();
        return Ok(StackResults);
    }
    let mut out = Vec::new();
    rules.normalize_into(source, &mut out);
    call.push(out.as_slice())?;
    Ok(StackResults)
}

const HEX: &[u8; 16] = b"0123456789abcdef";

/// Lower-case hex of `data`.
#[must_use]
pub fn to_hex(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len() * 2);
    for byte in data {
        out.push(HEX[usize::from(byte >> 4)] as char);
        out.push(HEX[usize::from(byte & 15)] as char);
    }
    out
}

fn from_hex(text: &str) -> Result<NewBuffer> {
    let digits: Vec<u8> = text.bytes().filter(|b| !b.is_ascii_whitespace()).collect();
    if !digits.len().is_multiple_of(2) {
        return Err(Error::runtime("bytes.fromHex: odd number of hex digits"));
    }
    let nibble = |d: u8| -> Result<u8> {
        (d as char)
            .to_digit(16)
            .map(|v| v as u8)
            .ok_or_else(|| Error::runtime(format!("bytes.fromHex: '{}' is not a hex digit", d as char)))
    };
    let mut out = Vec::with_capacity(digits.len() / 2);
    for pair in digits.as_chunks::<2>().0 {
        out.push((nibble(pair[0])? << 4) | nibble(pair[1])?);
    }
    Ok(NewBuffer(out))
}

const BASE64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Standard, padded base64 of `data` (RFC 4648 section 4).
#[must_use]
pub fn to_base64(data: &[u8]) -> String {
    let mut out = Vec::with_capacity(data.len().div_ceil(3) * 4);
    let (chunks, rest) = data.as_chunks::<3>();
    for chunk in chunks {
        let n = (u32::from(chunk[0]) << 16) | (u32::from(chunk[1]) << 8) | u32::from(chunk[2]);
        out.extend_from_slice(&[
            BASE64[(n >> 18) as usize],
            BASE64[((n >> 12) & 63) as usize],
            BASE64[((n >> 6) & 63) as usize],
            BASE64[(n & 63) as usize],
        ]);
    }
    match *rest {
        [a] => {
            let n = u32::from(a) << 16;
            out.extend_from_slice(&[BASE64[(n >> 18) as usize], BASE64[((n >> 12) & 63) as usize], b'=', b'=']);
        }
        [a, b] => {
            let n = (u32::from(a) << 16) | (u32::from(b) << 8);
            out.extend_from_slice(&[
                BASE64[(n >> 18) as usize],
                BASE64[((n >> 12) & 63) as usize],
                BASE64[((n >> 6) & 63) as usize],
                b'=',
            ]);
        }
        _ => {}
    }
    String::from_utf8(out).expect("base64 is ASCII")
}

/// The bytes standard, padded base64 `text` holds; anything else (a stray character, a length
/// that isn't a multiple of four, padding before the end) is an error naming where.
fn from_base64(text: BytesView<'_>) -> Result<NewBuffer> {
    const WHAT: &str = "bytes.fromBase64";
    // SAFETY: as `find`; the decoded bytes are copied out before returning.
    let text = unsafe { bytes(&text) };
    if !text.len().is_multiple_of(4) {
        return Err(Error::runtime(format!("{WHAT}: {} characters is not a multiple of four", text.len())));
    }
    let padding = text.iter().rev().take(2).take_while(|byte| **byte == b'=').count();
    let mut out = Vec::with_capacity(text.len() / 4 * 3);
    let value = |at: usize| -> Result<u32> {
        let byte = text[at];
        let digit = match byte {
            b'A'..=b'Z' => byte - b'A',
            b'a'..=b'z' => byte - b'a' + 26,
            b'0'..=b'9' => byte - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            b'=' if at + padding >= text.len() => 0,
            _ => return Err(Error::runtime(format!("{WHAT}: byte {byte} at {at} is not base64"))),
        };
        Ok(u32::from(digit))
    };
    for index in 0..text.len() / 4 {
        let at = index * 4;
        let n = (value(at)? << 18) | (value(at + 1)? << 12) | (value(at + 2)? << 6) | value(at + 3)?;
        out.extend_from_slice(&[(n >> 16) as u8, (n >> 8) as u8, n as u8]);
    }
    out.truncate(out.len() - padding);
    Ok(NewBuffer(out))
}

/// `toBase64(source, offset?, length?)`: base64 of `length` bytes from `offset`, the whole of the
/// source by default.
fn to_base64_range(source: BytesView<'_>, offset: Option<Exact<i64>>, length: Option<Exact<i64>>) -> Result<String> {
    let offset = offset.unwrap_or(Exact(0));
    let start = count("bytes.toBase64 offset", offset)?;
    let len = match length {
        Some(length) => count("bytes.toBase64 length", length)?,
        None => source.len().saturating_sub(start),
    };
    let start = span("bytes.toBase64", source.len(), offset, len)?;
    // SAFETY: as `find`; the encoding is built before anything else runs.
    Ok(to_base64(&unsafe { bytes(&source) }[start..start + len]))
}

/// `readCString(bytes, offset, fieldLength?)`: the text up to the first NUL, and the offset
/// after the terminator, or after the field when a width is given.
fn read_cstring(source: BytesView<'_>, offset: Exact<i64>, field: Option<Exact<i64>>) -> Result<(Vec<u8>, f64)> {
    let start = span("bytes.readCString", source.len(), offset, 0)?;
    // SAFETY: as `find`; the text is copied out before returning.
    let data = unsafe { bytes(&source) };
    let (region, next_after) = match field {
        Some(width) => {
            let width = count("bytes.readCString fieldLength", width)?;
            let start = span("bytes.readCString", data.len(), offset, width)?;
            (&data[start..start + width], start + width)
        }
        None => (&data[start..], usize::MAX),
    };
    let (text, next) = match memchr::memchr(0, region) {
        Some(nul) => (&region[..nul], if next_after == usize::MAX { start + nul + 1 } else { next_after }),
        None => (region, if next_after == usize::MAX { data.len() } else { next_after }),
    };
    Ok((text.to_vec(), next as f64))
}

/// `writeCString(buffer, offset, text, fieldLength?)`: the text and a NUL, padded to the
/// field's width when given; the offset after what was written.
fn write_cstring(buffer: BufferView<'_>, offset: Exact<i64>, text: &[u8], field: Option<Exact<i64>>) -> Result<f64> {
    if let Some(nul) = memchr::memchr(0, text) {
        return Err(Error::runtime(format!("bytes.writeCString: text contains a NUL at {nul}")));
    }
    let width = match field {
        Some(width) => {
            let width = count("bytes.writeCString fieldLength", width)?;
            if text.len() >= width {
                return Err(Error::runtime(format!(
                    "bytes.writeCString: {} bytes of text do not fit a {width} byte field with its terminator",
                    text.len()
                )));
            }
            width
        }
        None => text.len() + 1,
    };
    let start = span("bytes.writeCString", buffer.len(), offset, width)?;
    buffer.write(start, text)?;
    buffer.fill(start + text.len(), width - text.len(), 0)?;
    Ok((start + width) as f64)
}

/// Unsigned LEB128: at most ten bytes; the value and the offset after it.
fn read_varint(source: BytesView<'_>, offset: Exact<i64>) -> Result<(Integer, f64)> {
    let start = span("bytes.readVarint", source.len(), offset, 0)?;
    // SAFETY: as `find`.
    let data = unsafe { bytes(&source) };
    let mut value: u64 = 0;
    for (index, byte) in data[start..].iter().enumerate().take(10) {
        let payload = u64::from(byte & 0x7F);
        if index == 9 && payload > 1 {
            return Err(Error::runtime(format!("bytes.readVarint: value at offset {start} overflows 64 bits")));
        }
        value |= payload << (7 * index);
        if byte & 0x80 == 0 {
            return Ok((Integer(value as i64), (start + index + 1) as f64));
        }
    }
    Err(Error::runtime(format!("bytes.readVarint: unterminated varint at offset {start}")))
}

/// Signed LEB128.
fn read_signed_varint(source: BytesView<'_>, offset: Exact<i64>) -> Result<(Integer, f64)> {
    let start = span("bytes.readSignedVarint", source.len(), offset, 0)?;
    // SAFETY: as `find`.
    let data = unsafe { bytes(&source) };
    let mut value: i64 = 0;
    let mut shift = 0u32;
    for (index, byte) in data[start..].iter().enumerate().take(10) {
        let payload = i64::from(byte & 0x7F);
        if shift < 64 {
            value |= payload << shift;
        }
        shift += 7;
        if byte & 0x80 == 0 {
            if shift < 64 && byte & 0x40 != 0 {
                value |= -1i64 << shift;
            }
            return Ok((Integer(value), (start + index + 1) as f64));
        }
    }
    Err(Error::runtime(format!("bytes.readSignedVarint: unterminated varint at offset {start}")))
}

fn write_varint(buffer: BufferView<'_>, offset: Exact<i64>, value: Exact<i64>) -> Result<f64> {
    let mut remaining = value.0 as u64;
    let mut encoded = [0u8; 10];
    let mut len = 0;
    loop {
        let byte = (remaining & 0x7F) as u8;
        remaining >>= 7;
        encoded[len] = if remaining == 0 { byte } else { byte | 0x80 };
        len += 1;
        if remaining == 0 {
            break;
        }
    }
    let start = span("bytes.writeVarint", buffer.len(), offset, len)?;
    buffer.write(start, &encoded[..len])?;
    Ok((start + len) as f64)
}

fn write_signed_varint(buffer: BufferView<'_>, offset: Exact<i64>, value: Exact<i64>) -> Result<f64> {
    let mut remaining = value.0;
    let mut encoded = [0u8; 10];
    let mut len = 0;
    loop {
        let byte = (remaining & 0x7F) as u8;
        remaining >>= 7;
        let done = (remaining == 0 && byte & 0x40 == 0) || (remaining == -1 && byte & 0x40 != 0);
        encoded[len] = if done { byte } else { byte | 0x80 };
        len += 1;
        if done {
            break;
        }
    }
    let start = span("bytes.writeSignedVarint", buffer.len(), offset, len)?;
    buffer.write(start, &encoded[..len])?;
    Ok((start + len) as f64)
}

impl Extension for BytesExtension {
    fn id(&self) -> &'static str {
        EXTENSION_ID
    }

    fn describe(&self, d: &mut ExtensionDescriptor) -> Result<()> {
        let mut math = d.userdata::<Math>("dream.bytes.Math");
        math.tag(crate::extension::TagPolicy::Required)
            .compiler_type(crate::extension::CompilerTypePolicy::Required)
            .doc("Big-endian, 24-bit and half-float reads and writes; the integer forms lower natively under jit.");
        numeric::describe_receiver(&mut math);
        #[cfg(feature = "jit")]
        d.native_hooks(lowering::ByteMath);
        #[cfg(feature = "bytes-digests")]
        digests::describe_hasher(d);
        #[cfg(feature = "bytes-regex")]
        regex::describe_regex(d);

        let module = d.module(MODULE);
        module.doc("Searching, comparing, record strings, varints, the byte widths and orders buffer lacks, codecs, digests and text.");
        describe_core(module);
        numeric::describe_module(module);
        #[cfg(feature = "bytes-codecs")]
        codecs::describe(module);
        #[cfg(feature = "bytes-digests")]
        digests::describe(module);
        #[cfg(feature = "bytes-text")]
        text::describe(module);
        #[cfg(feature = "bytes-regex")]
        regex::describe(module);
        framing::describe(module);
        Ok(())
    }
}

fn describe_core(module: &mut ModuleDecl) {
    module
        .function("math", || crate::userdata::Owned(Math))
        .signature("() -> dream_bytes_Math")
        .doc("The receiver whose integer methods lower to native code under jit.")
        .function("find", find)
        .signature("(haystack: buffer | string, needle: buffer | string, start: number?) -> number?")
        .doc("The first offset of needle at or after start, or nil.")
        .function("rfind", rfind)
        .signature("(haystack: buffer | string, needle: buffer | string, endOffset: number?) -> number?")
        .doc("The last offset of needle that ends at or before endOffset, or nil.")
        .function("count", count_matches)
        .signature("(haystack: buffer | string, needle: buffer | string) -> number")
        .doc("Non-overlapping occurrences of needle.")
        .function("equals", equals)
        .signature("(a: buffer | string, b: buffer | string) -> boolean")
        .function("compare", compare)
        .signature("(a: buffer | string, aOffset: number, b: buffer | string, bOffset: number, length: number) -> number")
        .doc("-1, 0 or 1, comparing length bytes of each from its offset.")
        .function("startsWith", starts_with)
        .signature("(haystack: buffer | string, prefix: buffer | string, offset: number?) -> boolean")
        .function("translate", translate)
        .signature("(text: string, from: string, to: string, options: { collapse: string?, trimStart: string?, trimEnd: string? }?) -> string")
        .doc("text with each byte found in from replaced by the byte at the same position in to, then runs of collapse made one and trimStart and trimEnd removed from the ends; text itself when nothing changes.")
        .function("slice", slice)
        .signature("(source: buffer | string, offset: number, length: number) -> buffer")
        .doc("A new buffer holding length bytes from offset.")
        .function("toHex", |data: BytesView<'_>| -> String {
            // SAFETY: as `find`.
            to_hex(unsafe { bytes(&data) })
        })
        .signature("(data: buffer | string) -> string")
        .function("fromHex", from_hex)
        .signature("(text: string) -> buffer")
        .doc("Bytes from hex digits; whitespace between them is ignored.")
        .function("toBase64", to_base64_range)
        .signature("(source: buffer | string, offset: number?, length: number?) -> string")
        .doc("Standard padded base64 of length bytes from offset; the whole source by default.")
        .function("fromBase64", from_base64)
        .signature("(text: buffer | string) -> buffer")
        .doc("The bytes standard padded base64 holds; anything else is an error.")
        .function("readCString", read_cstring)
        .signature("(source: buffer | string, offset: number, fieldLength: number?) -> (string, number)")
        .doc("The text up to the first NUL and the offset after the terminator, or after the field when fieldLength is given.")
        .function("writeCString", write_cstring)
        .signature("(target: buffer, offset: number, text: string, fieldLength: number?) -> number")
        .doc("The text and a NUL, padded with NUL to fieldLength when given; returns the offset after the field.")
        .function("readVarint", read_varint)
        .signature("(source: buffer | string, offset: number) -> (integer, number)")
        .doc("Unsigned LEB128: the value and the offset after it.")
        .function("readSignedVarint", read_signed_varint)
        .signature("(source: buffer | string, offset: number) -> (integer, number)")
        .doc("Signed LEB128: the value and the offset after it.")
        .function("writeVarint", write_varint)
        .signature("(target: buffer, offset: number, value: integer | number) -> number")
        .doc("Unsigned LEB128 of the value's 64 bits; returns the offset after it.")
        .function("writeSignedVarint", write_signed_varint)
        .signature("(target: buffer, offset: number, value: integer | number) -> number")
        .doc("Signed LEB128; returns the offset after it.");
}
