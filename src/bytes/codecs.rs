//! Compression codecs (feature `bytes-codecs`): DEFLATE in zlib, raw and gzip framing, LZ4
//! blocks and frames, and zstd and LZMA/XZ decoding, all pure Rust.
//!
//! Decoders take an optional `maxSize`, the most they will produce (default 1 GiB), so a
//! hostile stream cannot grow memory without bound; a stream that exceeds it is an error, not
//! a truncated result. Encoders take a `level` where the format has one.

use std::io::Read;

use crate::bind::Call;
use crate::convert::{BytesView, Exact, NewBuffer};
use crate::error::{Error, Result};
use crate::extension::ModuleDecl;
use crate::options::Options;
use crate::stack::ValueView;

/// The default output cap for a decoder, in bytes.
pub const DEFAULT_MAX_SIZE: usize = 1 << 30;

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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DeflateFormat {
    Zlib,
    Raw,
    Gzip,
}

impl DeflateFormat {
    fn parse(what: &str, name: Option<&str>) -> Result<Self> {
        match name {
            None | Some("zlib") => Ok(Self::Zlib),
            Some("raw") => Ok(Self::Raw),
            Some("gzip") => Ok(Self::Gzip),
            Some(other) => Err(Error::runtime(format!("{what}: format '{other}' is not zlib, raw or gzip"))),
        }
    }
}

struct DecodeOptions {
    format: DeflateFormat,
    max_size: usize,
}

fn decode_options(call: &Call<'_>, options: Option<ValueView<'_>>, what: &str) -> Result<DecodeOptions> {
    let Some(options) = options else {
        return Ok(DecodeOptions { format: DeflateFormat::Zlib, max_size: DEFAULT_MAX_SIZE });
    };
    Options::read(call, options, what, |o| {
        let format = o.optional::<String>("format")?;
        let max_size = match o.optional::<Exact<i64>>("maxSize")? {
            Some(max) => usize::try_from(max.0).map_err(|_| Error::runtime(format!("{what}: negative maxSize")))?,
            None => DEFAULT_MAX_SIZE,
        };
        Ok(DecodeOptions { format: DeflateFormat::parse(what, format.as_deref())?, max_size })
    })
}

fn max_size_option(call: &Call<'_>, options: Option<ValueView<'_>>, what: &str) -> Result<usize> {
    let Some(options) = options else { return Ok(DEFAULT_MAX_SIZE) };
    Options::read(call, options, what, |o| match o.optional::<Exact<i64>>("maxSize")? {
        Some(max) => usize::try_from(max.0).map_err(|_| Error::runtime(format!("{what}: negative maxSize"))),
        None => Ok(DEFAULT_MAX_SIZE),
    })
}

fn too_large(what: &str, max: usize) -> Error {
    Error::runtime(format!("{what}: output exceeds maxSize ({max} bytes)"))
}

/// The DEFLATE payload inside a gzip member, and its trailer.
fn gzip_member<'a>(what: &str, data: &'a [u8]) -> Result<(&'a [u8], u32, u32)> {
    let header = |cond: bool, message: &str| -> Result<()> {
        if cond { Ok(()) } else { Err(Error::runtime(format!("{what}: {message}"))) }
    };
    header(data.len() >= 18 && data[0] == 0x1F && data[1] == 0x8B, "not a gzip stream")?;
    header(data[2] == 8, "gzip member is not DEFLATE compressed")?;
    let flags = data[3];
    let mut at = 10;
    if flags & 0x04 != 0 {
        header(data.len() >= at + 2, "truncated gzip extra field")?;
        let extra = usize::from(u16::from_le_bytes([data[at], data[at + 1]]));
        at += 2 + extra;
    }
    for bit in [0x08, 0x10] {
        if flags & bit != 0 {
            let end = memchr::memchr(0, data.get(at..).unwrap_or(&[]))
                .ok_or_else(|| Error::runtime(format!("{what}: truncated gzip header string")))?;
            at += end + 1;
        }
    }
    if flags & 0x02 != 0 {
        at += 2;
    }
    header(data.len() >= at + 8, "truncated gzip header")?;
    let trailer = &data[data.len() - 8..];
    let crc = u32::from_le_bytes([trailer[0], trailer[1], trailer[2], trailer[3]]);
    let size = u32::from_le_bytes([trailer[4], trailer[5], trailer[6], trailer[7]]);
    Ok((&data[at..data.len() - 8], crc, size))
}

fn inflate(call: &Call<'_>, source: BytesView<'_>, options: Option<ValueView<'_>>) -> Result<NewBuffer> {
    const WHAT: &str = "bytes.inflate";
    let options = decode_options(call, options, WHAT)?;
    // SAFETY: the decoder reads the view and calls nothing; the output is a fresh Vec.
    let data = unsafe { bytes(&source) };
    let failed = |error: miniz_oxide::inflate::DecompressError| {
        if error.status == miniz_oxide::inflate::TINFLStatus::HasMoreOutput {
            too_large(WHAT, options.max_size)
        } else {
            Error::runtime(format!("{WHAT}: corrupt DEFLATE stream ({:?})", error.status))
        }
    };
    let out = match options.format {
        DeflateFormat::Zlib => {
            miniz_oxide::inflate::decompress_to_vec_zlib_with_limit(data, options.max_size).map_err(failed)?
        }
        DeflateFormat::Raw => {
            miniz_oxide::inflate::decompress_to_vec_with_limit(data, options.max_size).map_err(failed)?
        }
        DeflateFormat::Gzip => {
            let (payload, crc, size) = gzip_member(WHAT, data)?;
            let out = miniz_oxide::inflate::decompress_to_vec_with_limit(payload, options.max_size).map_err(failed)?;
            if out.len() as u32 != size || crc32fast::hash(&out) != crc {
                return Err(Error::runtime(format!("{WHAT}: gzip trailer does not match the data")));
            }
            out
        }
    };
    Ok(NewBuffer(out))
}

fn deflate(call: &Call<'_>, source: BytesView<'_>, options: Option<ValueView<'_>>) -> Result<NewBuffer> {
    const WHAT: &str = "bytes.deflate";
    let (format, level) = match options {
        None => (DeflateFormat::Zlib, 6u8),
        Some(options) => Options::read(call, options, WHAT, |o| {
            let format = DeflateFormat::parse(WHAT, o.optional::<String>("format")?.as_deref())?;
            let level = match o.optional::<Exact<i64>>("level")? {
                Some(level) => u8::try_from(level.0)
                    .ok()
                    .filter(|l| *l <= 10)
                    .ok_or_else(|| Error::runtime(format!("{WHAT}: level {} is outside 0..=10", level.0)))?,
                None => 6,
            };
            Ok((format, level))
        })?,
    };
    // SAFETY: as `inflate`.
    let data = unsafe { bytes(&source) };
    let out = match format {
        DeflateFormat::Zlib => miniz_oxide::deflate::compress_to_vec_zlib(data, level),
        DeflateFormat::Raw => miniz_oxide::deflate::compress_to_vec(data, level),
        DeflateFormat::Gzip => {
            let body = miniz_oxide::deflate::compress_to_vec(data, level);
            let mut out = Vec::with_capacity(body.len() + 18);
            out.extend_from_slice(&[0x1F, 0x8B, 8, 0, 0, 0, 0, 0, 0, 255]);
            out.extend_from_slice(&body);
            out.extend_from_slice(&crc32fast::hash(data).to_le_bytes());
            out.extend_from_slice(&(data.len() as u32).to_le_bytes());
            out
        }
    };
    Ok(NewBuffer(out))
}

fn lz4_decompress(source: BytesView<'_>, size: Exact<i64>) -> Result<NewBuffer> {
    const WHAT: &str = "bytes.lz4Decompress";
    let size = usize::try_from(size.0).map_err(|_| Error::runtime(format!("{WHAT}: negative decompressedSize")))?;
    // SAFETY: as `inflate`.
    let data = unsafe { bytes(&source) };
    lz4_flex::block::decompress(data, size).map(NewBuffer).map_err(|error| Error::runtime(format!("{WHAT}: {error}")))
}

fn lz4_compress(source: BytesView<'_>) -> NewBuffer {
    // SAFETY: as `inflate`.
    NewBuffer(lz4_flex::block::compress(unsafe { bytes(&source) }))
}

fn lz4_frame_decompress(call: &Call<'_>, source: BytesView<'_>, options: Option<ValueView<'_>>) -> Result<NewBuffer> {
    const WHAT: &str = "bytes.lz4FrameDecompress";
    let max = max_size_option(call, options, WHAT)?;
    // SAFETY: as `inflate`.
    let data = unsafe { bytes(&source) };
    let mut out = Vec::new();
    lz4_flex::frame::FrameDecoder::new(data)
        .take(max as u64 + 1)
        .read_to_end(&mut out)
        .map_err(|error| Error::runtime(format!("{WHAT}: {error}")))?;
    if out.len() > max {
        return Err(too_large(WHAT, max));
    }
    Ok(NewBuffer(out))
}

fn lz4_frame_compress(source: BytesView<'_>) -> Result<NewBuffer> {
    const WHAT: &str = "bytes.lz4FrameCompress";
    // SAFETY: as `inflate`.
    let data = unsafe { bytes(&source) };
    let mut encoder = lz4_flex::frame::FrameEncoder::new(Vec::with_capacity(data.len() / 2 + 64));
    std::io::Write::write_all(&mut encoder, data).map_err(|error| Error::runtime(format!("{WHAT}: {error}")))?;
    encoder.finish().map(NewBuffer).map_err(|error| Error::runtime(format!("{WHAT}: {error}")))
}

fn zstd_decompress(call: &Call<'_>, source: BytesView<'_>, options: Option<ValueView<'_>>) -> Result<NewBuffer> {
    const WHAT: &str = "bytes.zstdDecompress";
    let max = max_size_option(call, options, WHAT)?;
    // SAFETY: as `inflate`.
    let mut data = unsafe { bytes(&source) };
    let mut decoder = ruzstd::decoding::StreamingDecoder::new(&mut data)
        .map_err(|error| Error::runtime(format!("{WHAT}: {error}")))?;
    let mut out = Vec::new();
    (&mut decoder)
        .take(max as u64 + 1)
        .read_to_end(&mut out)
        .map_err(|error| Error::runtime(format!("{WHAT}: {error}")))?;
    if out.len() > max {
        return Err(too_large(WHAT, max));
    }
    Ok(NewBuffer(out))
}

fn lzma_decompress(call: &Call<'_>, source: BytesView<'_>, options: Option<ValueView<'_>>) -> Result<NewBuffer> {
    const WHAT: &str = "bytes.lzmaDecompress";
    let (xz, max) = match options {
        None => (false, DEFAULT_MAX_SIZE),
        Some(options) => Options::read(call, options, WHAT, |o| {
            let xz = match o.optional::<String>("format")?.as_deref() {
                None | Some("lzma") => false,
                Some("xz") => true,
                Some(other) => return Err(Error::runtime(format!("{WHAT}: format '{other}' is not lzma or xz"))),
            };
            let max = match o.optional::<Exact<i64>>("maxSize")? {
                Some(max) => usize::try_from(max.0).map_err(|_| Error::runtime(format!("{WHAT}: negative maxSize")))?,
                None => DEFAULT_MAX_SIZE,
            };
            Ok((xz, max))
        })?,
    };
    // SAFETY: as `inflate`.
    let mut data = unsafe { bytes(&source) };
    let mut out = Vec::new();
    let options = lzma_rs::decompress::Options {
        memlimit: Some(max),
        unpacked_size: lzma_rs::decompress::UnpackedSize::ReadFromHeader,
        allow_incomplete: false,
    };
    let result = if xz {
        lzma_rs::xz_decompress(&mut data, &mut out)
    } else {
        lzma_rs::lzma_decompress_with_options(&mut data, &mut out, &options)
    };
    result.map_err(|error| Error::runtime(format!("{WHAT}: {error}")))?;
    if out.len() > max {
        return Err(too_large(WHAT, max));
    }
    Ok(NewBuffer(out))
}

pub(crate) fn describe(module: &mut ModuleDecl) {
    module
        .function("inflate", inflate)
        .signature("(source: buffer | string, options: { format: string?, maxSize: number? }?) -> buffer")
        .doc("DEFLATE decoding: format zlib (default), raw or gzip; maxSize caps the output.")
        .function("deflate", deflate)
        .signature("(source: buffer | string, options: { format: string?, level: number? }?) -> buffer")
        .doc("DEFLATE encoding: format zlib (default), raw or gzip; level 0 to 10, default 6.")
        .function("lz4Decompress", lz4_decompress)
        .signature("(source: buffer | string, decompressedSize: number) -> buffer")
        .doc("An LZ4 block; the format does not record its size, so the caller supplies it.")
        .function("lz4Compress", lz4_compress)
        .signature("(source: buffer | string) -> buffer")
        .doc("An LZ4 block, without a size prefix.")
        .function("lz4FrameDecompress", lz4_frame_decompress)
        .signature("(source: buffer | string, options: { maxSize: number? }?) -> buffer")
        .function("lz4FrameCompress", lz4_frame_compress)
        .signature("(source: buffer | string) -> buffer")
        .function("zstdDecompress", zstd_decompress)
        .signature("(source: buffer | string, options: { maxSize: number? }?) -> buffer")
        .function("lzmaDecompress", lzma_decompress)
        .signature("(source: buffer | string, options: { format: string?, maxSize: number? }?) -> buffer")
        .doc("LZMA (.lzma, default) or XZ (format = 'xz') decoding.");
}
