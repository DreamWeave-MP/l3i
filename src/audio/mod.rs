//! Audio decoders: the `dream.audio` extension, module `@dream/audio`.
//!
//! ```lua
//! local audio = require('@dream/audio')
//! local decoder = audio.decoder(fs.readFile('theme.ogg'))
//! print(decoder:format(), decoder:rate(), decoder:channels(), decoder:frames())  -- vorbis 44100 2 ...
//! local block = buffer.create(4096 * decoder:channels() * 4)
//! while true do
//!   local frames = decoder:read(block)   -- interleaved f32, little-endian
//!   if frames == 0 then break end
//!   -- hand block's first frames * channels samples to a mixer, a stream, a file...
//! end
//! decoder:seek(44100)                     -- one second in, exactly
//! local whole = audio.decode(fs.readFile('step.mp3'))
//! print(whole.frames, buffer.len(whole.data))
//! ```
//!
//! # Formats
//!
//! - **MP3** (and MPEG layers I and II) through nanomp3, a safe port of minimp3: ID3 and APE
//!   tags are skipped, the encoder delay and padding a LAME or Xing header records are trimmed,
//!   and the length is that header's, or found by scanning the stream when it has none.
//! - **Ogg Vorbis** through lewton's packet decoder.
//! - **Ogg Opus** through libopus, the reference decoder, built from the `opus` submodule. Opus always decodes at 48 kHz; the
//!   stream's pre-skip is trimmed and its output gain applied. Mapping families 0, 1 and 255
//!   decode; the ambisonic families 2 and 3 are refused.
//!
//! An Ogg file is indexed into its packets when it opens, so its length comes from its last
//! granule position, the end is trimmed to it, and a seek is a binary search. Only the first
//! logical stream is read: a chained file plays its first link. A damaged page is skipped, and
//! a file cut short plays up to the cut.
//!
//! # Frames
//!
//! Every decoder produces interleaved 32-bit floats, nominally in -1 to 1, written into a
//! caller's buffer little-endian, in the file's own channel order (Vorbis order for both Ogg
//! formats). Positions and lengths count frames: one sample per channel. A seek is exact for
//! MP3 and Vorbis, which decode what the target depends on first; for Opus, decoding restarts
//! 400 ms ahead of the target, five times the minimum RFC 7845 recommends: CELT predicts each
//! frame's band energies from the last, and a decoder started cold takes that long to agree
//! with a straight run to within a millionth.
//!
//! # Ownership
//!
//! A decoder copies the bytes it is given (a buffer may change after the call) and holds them,
//! with an Ogg file's packet index, until it is collected. Decoding reads nothing but that copy:
//! no file, no capability. A decoder belongs to the runtime that made it; a worker decodes with
//! its own.
//!
//! The decoders are third-party Rust fed whatever a file holds. A panic in one is caught at
//! the call, reported as an error, and the decoder refuses to read again until it seeks.

mod mp3;
mod ogg;
mod opus;
mod packeted;
mod vorbis;

use std::cell::RefCell;
use std::fmt;

use crate::bind::{Call, StackResults};
use crate::convert::{BufferView, BytesView, Exact, new_buffer};
use crate::error::{Error, Result};
use crate::extension::{Extension, ExtensionDescriptor, TagPolicy};
use crate::options::Options;
use crate::stack::{Frame, Scope, ValueView};
use crate::userdata::Owned;

/// The extension id.
pub const EXTENSION_ID: &str = "dream.audio";
/// The module path.
pub const MODULE: &str = "@dream/audio";
/// The default cap on what `audio.decode` produces, in bytes of samples.
pub const DEFAULT_MAX_SIZE: usize = 1 << 30;

/// A compressed audio format.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Format {
    /// MPEG audio, layers I to III.
    Mp3,
    /// Vorbis in Ogg.
    Vorbis,
    /// Opus in Ogg.
    Opus,
}

impl Format {
    /// The format's name in Luau: `mp3`, `vorbis` or `opus`.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Format::Mp3 => "mp3",
            Format::Vorbis => "vorbis",
            Format::Opus => "opus",
        }
    }

    /// The format a name gives.
    #[must_use]
    pub fn parse(name: &str) -> Option<Format> {
        match name {
            "mp3" => Some(Format::Mp3),
            "vorbis" => Some(Format::Vorbis),
            "opus" => Some(Format::Opus),
            _ => None,
        }
    }

    /// The format of a file from its first bytes: Ogg by its first packet's signature, MPEG
    /// audio by an ID3 tag or frames that follow each other.
    #[must_use]
    pub fn detect(bytes: &[u8]) -> Option<Format> {
        if bytes.starts_with(b"OggS") {
            // The first page holds the first packet whole: 27 header bytes, the segment table,
            // then the packet.
            let segments = usize::from(*bytes.get(26)?);
            let packet = bytes.get(27 + segments..)?;
            if packet.starts_with(b"\x01vorbis") {
                return Some(Format::Vorbis);
            }
            return packet.starts_with(b"OpusHead").then_some(Format::Opus);
        }
        nanomp3::detect(bytes).then_some(Format::Mp3)
    }
}

impl fmt::Display for Format {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// What kind of failure a [`DecodeError`] is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum DecodeErrorKind {
    /// The bytes are not a format this module reads.
    Unrecognized,
    /// The bytes are the format, but broken.
    Malformed,
    /// The stream is valid but uses something this module does not decode.
    Unsupported,
    /// A decoder failed (panicked) on the stream.
    Failed,
}

/// A stream that could not be opened, read or sought.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DecodeError {
    kind: DecodeErrorKind,
    message: String,
}

impl DecodeError {
    fn new(kind: DecodeErrorKind, message: String) -> DecodeError {
        DecodeError { kind, message }
    }

    pub(crate) fn malformed(message: String) -> DecodeError {
        DecodeError::new(DecodeErrorKind::Malformed, message)
    }

    pub(crate) fn unsupported(message: String) -> DecodeError {
        DecodeError::new(DecodeErrorKind::Unsupported, message)
    }

    pub(crate) fn failed(message: String) -> DecodeError {
        DecodeError::new(DecodeErrorKind::Failed, message)
    }

    /// Prefixes a codec's message with the call it surfaced in.
    pub(crate) fn within(self, what: &str) -> DecodeError {
        DecodeError { kind: self.kind, message: format!("{what}: {}", self.message) }
    }

    /// The kind of failure.
    #[must_use]
    pub fn kind(&self) -> DecodeErrorKind {
        self.kind
    }
}

impl fmt::Display for DecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for DecodeError {}

enum Codec {
    Mp3(Box<mp3::Mp3>),
    Vorbis(Box<packeted::PacketStream<vorbis::Vorbis>>),
    Opus(Box<packeted::PacketStream<opus::Opus>>),
}

/// A decoder over one file's bytes, read frame by frame into interleaved `f32`.
pub struct Decoder {
    codec: Codec,
    format: Format,
    rate: u32,
    channels: usize,
}

impl fmt::Debug for Decoder {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Decoder")
            .field("format", &self.format)
            .field("rate", &self.rate)
            .field("channels", &self.channels)
            .field("frames", &self.frames())
            .field("position", &self.position())
            .finish_non_exhaustive()
    }
}

impl Decoder {
    /// Opens `bytes` as `format`, or as the format [`Format::detect`] finds.
    ///
    /// # Errors
    ///
    /// The bytes are not the format, are broken before any audio, or use something the format's
    /// decoder does not support.
    pub fn new(bytes: Vec<u8>, format: Option<Format>) -> std::result::Result<Decoder, DecodeError> {
        const WHAT: &str = "audio.decoder";
        let Some(format) = format.or_else(|| Format::detect(&bytes)) else {
            return Err(DecodeError::new(
                DecodeErrorKind::Unrecognized,
                format!("{WHAT}: not MP3, Ogg Vorbis or Ogg Opus"),
            ));
        };
        let (codec, rate, channels) = match format {
            Format::Mp3 => {
                let mp3 = mp3::Mp3::open(WHAT, bytes)?;
                let (rate, channels) = (mp3.rate(), mp3.channels());
                (Codec::Mp3(Box::new(mp3)), rate, channels)
            }
            Format::Vorbis => {
                let (stream, rate, channels) = vorbis::open(WHAT, &bytes)?;
                (Codec::Vorbis(Box::new(stream)), rate, channels)
            }
            Format::Opus => {
                let (stream, channels) = opus::open(WHAT, &bytes)?;
                (Codec::Opus(Box::new(stream)), opus::RATE, channels)
            }
        };
        if rate == 0 || channels == 0 {
            return Err(DecodeError::malformed(format!("{WHAT}: a {format} stream with no rate or no channels")));
        }
        Ok(Decoder { codec, format, rate, channels })
    }

    /// The stream's format.
    #[must_use]
    pub fn format(&self) -> Format {
        self.format
    }

    /// Frames per second.
    #[must_use]
    pub fn rate(&self) -> u32 {
        self.rate
    }

    /// Samples per frame.
    #[must_use]
    pub fn channels(&self) -> usize {
        self.channels
    }

    /// The stream's length in frames, when the stream says.
    #[must_use]
    pub fn frames(&self) -> Option<u64> {
        match &self.codec {
            Codec::Mp3(mp3) => mp3.frames(),
            Codec::Vorbis(stream) => Some(stream.frames()),
            Codec::Opus(stream) => Some(stream.frames()),
        }
    }

    /// The frame the next read starts at.
    #[must_use]
    pub fn position(&self) -> u64 {
        match &self.codec {
            Codec::Mp3(mp3) => mp3.position(),
            Codec::Vorbis(stream) => stream.position(),
            Codec::Opus(stream) => stream.position(),
        }
    }

    /// Fills `out` with as many whole frames as it holds and the stream has left; returns how
    /// many frames it wrote, `0` at the end.
    ///
    /// # Errors
    ///
    /// The stream is broken at this point, or the decoder failed on it earlier.
    pub fn read(&mut self, out: &mut [f32]) -> std::result::Result<usize, DecodeError> {
        const WHAT: &str = "Decoder:read";
        match &mut self.codec {
            Codec::Mp3(mp3) => mp3.read(WHAT, out),
            Codec::Vorbis(stream) => stream.read(WHAT, out),
            Codec::Opus(stream) => stream.read(WHAT, out),
        }
    }

    /// Moves to frame `frame`; past the end is the end.
    ///
    /// # Errors
    ///
    /// The stream is broken where the seek lands.
    pub fn seek(&mut self, frame: u64) -> std::result::Result<(), DecodeError> {
        const WHAT: &str = "Decoder:seek";
        match &mut self.codec {
            Codec::Mp3(mp3) => mp3.seek(WHAT, frame),
            Codec::Vorbis(stream) => stream.seek(WHAT, frame),
            Codec::Opus(stream) => stream.seek(WHAT, frame),
        }
    }
}

/// `dream.audio.Decoder`: a decoder and the samples it writes through.
struct DecoderHandle {
    decoder: RefCell<Decoder>,
    scratch: RefCell<Vec<f32>>,
}

// SAFETY: the payload holds no Lua references.
unsafe impl crate::userdata::Userdata for DecoderHandle {
    const NAME: &'static str = "dream.audio.Decoder";
}

fn runtime_error(error: &DecodeError) -> Error {
    Error::runtime(error.to_string())
}

/// `options.format`, when given.
fn format_option(call: &Call<'_>, options: Option<ValueView<'_>>, what: &str) -> Result<(Option<Format>, usize)> {
    let Some(options) = options.filter(|view| !view.is_nil()) else { return Ok((None, DEFAULT_MAX_SIZE)) };
    Options::read(call, options, what, |o| {
        let format = match o.optional::<String>("format")? {
            Some(name) => Some(
                Format::parse(&name)
                    .ok_or_else(|| Error::runtime(format!("{what}: format '{name}' is not mp3, vorbis or opus")))?,
            ),
            None => None,
        };
        let max_size = match o.optional::<Exact<i64>>("maxSize")? {
            Some(max) => usize::try_from(max.0).map_err(|_| Error::runtime(format!("{what}: negative maxSize")))?,
            None => DEFAULT_MAX_SIZE,
        };
        Ok((format, max_size))
    })
}

/// `audio.decoder(data, options?)`.
fn new_decoder(call: &Call<'_>, source: BytesView<'_>, options: Option<ValueView<'_>>) -> Result<Owned<DecoderHandle>> {
    let (format, _) = format_option(call, options, "audio.decoder")?;
    let decoder = Decoder::new(source.to_vec(), format).map_err(|error| runtime_error(&error))?;
    Ok(Owned(DecoderHandle { decoder: RefCell::new(decoder), scratch: RefCell::new(Vec::new()) }))
}

/// Writes `samples` into `out` little-endian.
fn write_samples(out: &mut [u8], samples: &[f32]) {
    for (bytes, sample) in out.as_chunks_mut::<4>().0.iter_mut().zip(samples) {
        *bytes = sample.to_le_bytes();
    }
}

/// `audio.decode(data, options?)`: the whole stream into one buffer.
fn decode(call: &Call<'_>, source: BytesView<'_>, options: Option<ValueView<'_>>) -> Result<StackResults> {
    const WHAT: &str = "audio.decode";
    let (format, max_size) = format_option(call, options, WHAT)?;
    let mut decoder = Decoder::new(source.to_vec(), format).map_err(|error| runtime_error(&error))?;
    let channels = decoder.channels();
    let max_samples = max_size / 4;
    let mut samples: Vec<f32> = Vec::new();
    if let Some(frames) = decoder.frames() {
        samples.reserve(usize::try_from(frames).unwrap_or(usize::MAX).saturating_mul(channels).min(max_samples));
    }
    let mut block = vec![0.0f32; 4096 * channels];
    loop {
        let frames = decoder.read(&mut block).map_err(|error| runtime_error(&error))?;
        if frames == 0 {
            break;
        }
        if samples.len() + frames * channels > max_samples {
            return Err(Error::runtime(format!("{WHAT}: output exceeds maxSize ({max_size} bytes)")));
        }
        samples.extend_from_slice(&block[..frames * channels]);
    }
    let mut frame: Frame<'_> = call.frame();
    let table = frame.push_table(0, 5)?;
    let mut data = new_buffer(&frame, samples.len() * 4)?;
    // SAFETY: the buffer was created by this call and no other view of it exists.
    write_samples(unsafe { data.bytes_mut_unchecked() }, &samples);
    table.raw_set(&frame, "data")?;
    table.raw_set_value(&frame, "format", decoder.format().name())?;
    table.raw_set_value(&frame, "rate", &f64::from(decoder.rate()))?;
    table.raw_set_value(&frame, "channels", &(channels as f64))?;
    table.raw_set_value(&frame, "frames", &((samples.len() / channels) as f64))?;
    frame.release();
    Ok(StackResults)
}

/// `Decoder:read(target, offset?, frames?)`.
fn read_into(
    handle: &DecoderHandle,
    mut target: BufferView<'_>,
    offset: Option<Exact<i64>>,
    frames: Option<Exact<i64>>,
) -> Result<f64> {
    const WHAT: &str = "Decoder:read";
    let mut decoder = handle.decoder.borrow_mut();
    let channels = decoder.channels();
    let offset = match offset {
        Some(offset) => {
            usize::try_from(offset.0).map_err(|_| Error::runtime(format!("{WHAT}: offset {} is negative", offset.0)))?
        }
        None => 0,
    };
    let len = target.len();
    if offset > len {
        return Err(Error::runtime(format!("{WHAT}: offset {offset} past the end of the buffer (size {len})")));
    }
    let room = (len - offset) / (4 * channels);
    let want = match frames {
        Some(frames) => {
            let frames = usize::try_from(frames.0)
                .map_err(|_| Error::runtime(format!("{WHAT}: frames {} is negative", frames.0)))?;
            if frames > room {
                return Err(Error::runtime(format!(
                    "{WHAT}: {frames} frames do not fit the buffer (room for {room} after offset {offset})"
                )));
            }
            frames
        }
        None => room,
    };
    let mut scratch = handle.scratch.borrow_mut();
    scratch.resize(want * channels, 0.0);
    let written = decoder.read(&mut scratch).map_err(|error| runtime_error(&error))?;
    // SAFETY: the window is inside the buffer, this call holds the only view of it, and nothing
    // here calls into Lua while the slice lives.
    let out = unsafe { &mut target.bytes_mut_unchecked()[offset..offset + written * channels * 4] };
    write_samples(out, &scratch[..written * channels]);
    Ok(written as f64)
}

fn describe_decoder(d: &mut ExtensionDescriptor) {
    let mut decoder = d.userdata::<DecoderHandle>("dream.audio.Decoder");
    decoder.tag(TagPolicy::Preferred).doc("A decoder over one file's bytes, read into interleaved f32 frames.");
    decoder
        .method("format", |handle: &DecoderHandle| handle.decoder.borrow().format().name())
        .signature("(self): dream_audio_Format")
        .doc("mp3, vorbis or opus.");
    decoder
        .method("rate", |handle: &DecoderHandle| f64::from(handle.decoder.borrow().rate()))
        .signature("(self): number")
        .doc("Frames per second; always 48000 for Opus.");
    decoder
        .method("channels", |handle: &DecoderHandle| handle.decoder.borrow().channels() as f64)
        .signature("(self): number")
        .doc("Samples per frame, in the file's channel order.");
    decoder
        .method("frames", |handle: &DecoderHandle, call: &Call<'_>| -> Result<StackResults> {
            call.push(&handle.decoder.borrow().frames().map(|frames| frames as f64))?;
            Ok(StackResults)
        })
        .signature("(self): number?")
        .doc("The stream's length in frames, after gapless trimming; nil when the stream does not say.");
    decoder
        .method("position", |handle: &DecoderHandle| handle.decoder.borrow().position() as f64)
        .signature("(self): number")
        .doc("The frame the next read starts at.");
    decoder
        .method("read", read_into)
        .signature("(self, target: buffer, offset: number?, frames: number?): number")
        .doc(
            "Decodes up to `frames` frames (default: as many as fit) into `target` from byte `offset`, as \
             interleaved little-endian f32; returns the frames written, 0 at the end.",
        );
    decoder
        .method("seek", |handle: &DecoderHandle, frame: Exact<i64>| -> Result<()> {
            let frame = u64::try_from(frame.0)
                .map_err(|_| Error::runtime(format!("Decoder:seek: frame {} is negative", frame.0)))?;
            handle.decoder.borrow_mut().seek(frame).map_err(|error| runtime_error(&error))
        })
        .signature("(self, frame: number): ()")
        .doc(
            "Moves to a frame: exactly for MP3 and Vorbis, after a 400 ms pre-roll for Opus; past the end is the end.",
        );
}

/// The `dream.audio` extension.
#[derive(Clone, Copy, Debug, Default)]
pub struct AudioExtension;

impl Extension for AudioExtension {
    fn id(&self) -> &'static str {
        EXTENSION_ID
    }

    fn describe(&self, d: &mut ExtensionDescriptor) -> Result<()> {
        d.type_alias("dream_audio_Format", "\"mp3\" | \"vorbis\" | \"opus\"");
        d.type_alias("dream_audio_DecoderOptions", "{ format: dream_audio_Format? }");
        d.type_alias("dream_audio_DecodeOptions", "{ format: dream_audio_Format?, maxSize: number? }");
        d.type_alias(
            "dream_audio_Decoded",
            "{ data: buffer, format: dream_audio_Format, rate: number, channels: number, frames: number }",
        );
        describe_decoder(d);

        d.module(MODULE)
            .doc("Audio decoders: MP3, Ogg Vorbis and Ogg Opus into interleaved f32 frames.")
            .function("detect", |call: &Call<'_>, source: BytesView<'_>| -> Result<StackResults> {
                // SAFETY: the slice lives only for this call, which neither calls Lua nor writes
                // the buffer.
                let format = Format::detect(unsafe { source.bytes_unchecked() });
                call.push(&format.map(Format::name))?;
                Ok(StackResults)
            })
            .signature("(data: buffer | string) -> dream_audio_Format?")
            .doc("The format a file's first bytes say it is, or nil.")
            .function("decoder", new_decoder)
            .signature("(data: buffer | string, options: dream_audio_DecoderOptions?) -> dream_audio_Decoder")
            .doc("A decoder over a copy of the bytes; the format is detected unless named.")
            .function("decode", decode)
            .signature("(data: buffer | string, options: dream_audio_DecodeOptions?) -> dream_audio_Decoded")
            .doc("The whole stream as one buffer of interleaved f32 frames, at most maxSize bytes (default 1 GiB).");
        Ok(())
    }
}
