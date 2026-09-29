//! Text codecs (feature `bytes-text`): decoding bytes in any WHATWG-labelled encoding to a
//! Luau (UTF-8) string, and encoding back, for the codepages foreign formats carry their text
//! in: `windows-1252`, `shift_jis`, `euc-kr`, `gbk`, `koi8-r`, `macintosh`, `utf-16le`,
//! `utf-16be` and the rest of encoding_rs's table. Labels are case-insensitive and include the
//! usual aliases (`latin1`, `cp1252`, `sjis`).

use crate::bind::Call;
use crate::convert::{BytesView, NewBuffer};
use crate::error::{Error, Result};
use crate::extension::ModuleDecl;
use crate::options::Options;
use crate::stack::ValueView;

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

fn encoding(what: &str, label: &str) -> Result<&'static encoding_rs::Encoding> {
    encoding_rs::Encoding::for_label(label.as_bytes())
        .ok_or_else(|| Error::runtime(format!("{what}: '{label}' is not a known encoding label")))
}

fn decode(call: &Call<'_>, source: BytesView<'_>, label: &str, options: Option<ValueView<'_>>) -> Result<String> {
    const WHAT: &str = "bytes.decode";
    let strict = match options {
        None => false,
        Some(options) => Options::read(call, options, WHAT, |o| Ok(o.optional::<bool>("strict")?.unwrap_or(false)))?,
    };
    let encoding = encoding(WHAT, label)?;
    // SAFETY: the decoder reads the view and calls nothing; the result is an owned String.
    let data = unsafe { bytes(&source) };
    let (text, had_errors) = encoding.decode_without_bom_handling(data);
    if strict && had_errors {
        return Err(Error::runtime(format!("{WHAT}: the data is not valid {}", encoding.name())));
    }
    Ok(text.into_owned())
}

fn encode(text: &str, label: &str) -> Result<NewBuffer> {
    const WHAT: &str = "bytes.encode";
    let encoding = encoding(WHAT, label)?;
    // encoding_rs encodes to UTF-8 for the UTF-16 labels, as the WHATWG spec says; a format
    // that stores UTF-16 wants the real thing.
    if encoding == encoding_rs::UTF_16LE || encoding == encoding_rs::UTF_16BE {
        let mut out = Vec::with_capacity(text.len() * 2);
        for unit in text.encode_utf16() {
            out.extend_from_slice(&if encoding == encoding_rs::UTF_16LE {
                unit.to_le_bytes()
            } else {
                unit.to_be_bytes()
            });
        }
        return Ok(NewBuffer(out));
    }
    let (out, _, unmappable) = encoding.encode(text);
    if unmappable {
        return Err(Error::runtime(format!("{WHAT}: the text has characters {} cannot represent", encoding.name())));
    }
    Ok(NewBuffer(out.into_owned()))
}

pub(crate) fn describe(module: &mut ModuleDecl) {
    module
        .function("decode", decode)
        .signature("(source: buffer | string, encoding: string, options: { strict: boolean? }?) -> string")
        .doc("Text in the named encoding as a string; malformed input becomes U+FFFD unless strict.")
        .function("encode", encode)
        .signature("(text: string, encoding: string) -> buffer")
        .doc("A string in the named encoding; a character the encoding lacks is an error.")
        .function("encodingName", |label: &str| -> Result<String> {
            Ok(encoding("bytes.encodingName", label)?.name().to_owned())
        })
        .signature("(label: string) -> string")
        .doc("The canonical name a label resolves to, or an error for an unknown label.")
        .function("isUtf8", |data: BytesView<'_>| -> bool {
            // SAFETY: the check reads the view and calls nothing.
            std::str::from_utf8(unsafe { bytes(&data) }).is_ok()
        })
        .signature("(data: buffer | string) -> boolean");
}
