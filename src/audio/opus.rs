//! Ogg Opus (RFC 7845) through libopus, the reference decoder, built from the `opus` submodule.
//!
//! Opus always decodes at 48 kHz. A packet's duration is in its TOC byte, so the timeline is
//! exact without decoding; the stream's pre-skip is trimmed from the start and the last
//! granule position trims the end. A seek restarts the decoder 400 ms ahead of its target, so the
//! output has converged by the target frame (see `PRE_ROLL`).

use std::ffi::{CStr, c_char, c_int, c_uchar};
use std::ptr::NonNull;

use super::DecodeError;
use super::ogg::Packets;
use super::packeted::{PacketDecoder, PacketStream, Timeline};

/// OpusHead and OpusTags.
const HEADERS: usize = 2;
/// Opus's only output rate here.
pub(crate) const RATE: u32 = 48_000;
/// The most one packet holds: 120 ms.
const MAX_PACKET: usize = 5760;
/// How far ahead of a seek target decoding restarts. RFC 7845 (section 4.6) asks for at least
/// 80 ms, after which a cold decoder still runs as much as 15% quiet: CELT codes band energies
/// against the previous frame's, so the error decays a few frames at a time. 400 ms brings it
/// under a millionth of full scale, for about 20 packets more per seek.
const PRE_ROLL: u64 = 19_200;

/// libopus's opaque decoder states.
#[repr(C)]
struct OpusDecoder {
    _private: [u8; 0],
}

#[repr(C)]
struct OpusMsDecoder {
    _private: [u8; 0],
}

// The decode half of libopus's API (include/opus.h, include/opus_multistream.h), linked from
// the l3iopus archive build.rs compiles.
unsafe extern "C" {
    fn opus_decoder_create(rate: i32, channels: c_int, error: *mut c_int) -> *mut OpusDecoder;
    fn opus_decode_float(
        decoder: *mut OpusDecoder,
        data: *const c_uchar,
        len: i32,
        pcm: *mut f32,
        frame_size: c_int,
        decode_fec: c_int,
    ) -> c_int;
    fn opus_decoder_destroy(decoder: *mut OpusDecoder);
    fn opus_multistream_decoder_create(
        rate: i32,
        channels: c_int,
        streams: c_int,
        coupled_streams: c_int,
        mapping: *const c_uchar,
        error: *mut c_int,
    ) -> *mut OpusMsDecoder;
    fn opus_multistream_decode_float(
        decoder: *mut OpusMsDecoder,
        data: *const c_uchar,
        len: i32,
        pcm: *mut f32,
        frame_size: c_int,
        decode_fec: c_int,
    ) -> c_int;
    fn opus_multistream_decoder_destroy(decoder: *mut OpusMsDecoder);
    fn opus_strerror(error: c_int) -> *const c_char;
}

/// What libopus says an error code means.
fn strerror(code: c_int) -> String {
    // SAFETY: opus_strerror returns a pointer to a static, NUL-terminated string for any code.
    unsafe { CStr::from_ptr(opus_strerror(code)) }.to_string_lossy().into_owned()
}

/// A libopus decoder this module owns: created by `Layout::decoder`, destroyed on drop.
enum Inner {
    Single(NonNull<OpusDecoder>),
    Multi(NonNull<OpusMsDecoder>),
}

impl Drop for Inner {
    fn drop(&mut self) {
        // SAFETY: each pointer came from its create function, is owned by this value alone, and
        // is destroyed once, here.
        unsafe {
            match self {
                Inner::Single(decoder) => opus_decoder_destroy(decoder.as_ptr()),
                Inner::Multi(decoder) => opus_multistream_decoder_destroy(decoder.as_ptr()),
            }
        }
    }
}

pub(crate) struct Opus {
    inner: Inner,
    layout: Layout,
    scratch: Vec<f32>,
}

// SAFETY: a libopus decoder state is plain memory with no thread affinity, owned and used
// through `&mut` only.
unsafe impl Send for Opus {}

/// A multistream layout (RFC 7845 section 5.1.1): elementary streams, the first `coupled` of
/// them stereo, and which decoded channel each output channel is (255 for silence).
#[derive(Clone)]
struct Table {
    streams: usize,
    coupled: usize,
    mapping: Vec<u8>,
}

/// What OpusHead says about the channels.
#[derive(Clone)]
struct Layout {
    channels: usize,
    table: Option<Table>,
    /// The output gain as a factor.
    gain: f32,
}

impl Layout {
    /// A fresh decoder.
    fn decoder(&self) -> Result<Inner, DecodeError> {
        let mut code: c_int = 0;
        let as_int = |value: usize| c_int::try_from(value).unwrap_or(c_int::MAX);
        // SAFETY: `code` outlives the call, and a mapping holds exactly `channels` entries, the
        // length libopus reads.
        let inner = unsafe {
            match &self.table {
                None => NonNull::new(opus_decoder_create(RATE as i32, as_int(self.channels), &raw mut code))
                    .map(Inner::Single),
                Some(table) => NonNull::new(opus_multistream_decoder_create(
                    RATE as i32,
                    as_int(self.channels),
                    as_int(table.streams),
                    as_int(table.coupled),
                    table.mapping.as_ptr(),
                    &raw mut code,
                ))
                .map(Inner::Multi),
            }
        };
        inner
            .ok_or_else(|| DecodeError::unsupported(format!("libopus refused the stream's layout: {}", strerror(code))))
    }
}

/// Reads OpusHead: the layout and the pre-skip.
fn head(what: &str, packet: &[u8]) -> Result<(Layout, u64), DecodeError> {
    let malformed = |reason: &str| DecodeError::malformed(format!("{what}: bad OpusHead: {reason}"));
    if packet.len() < 19 || !packet.starts_with(b"OpusHead") {
        return Err(malformed("not an Opus stream"));
    }
    // Version 0 and 1 are this layout; RFC 7845 keeps the low four bits compatible.
    if packet[8] >> 4 != 0 {
        return Err(DecodeError::unsupported(format!("{what}: Opus version {} is not supported", packet[8])));
    }
    let channels = packet[9];
    let pre_skip = u64::from(u16::from_le_bytes([packet[10], packet[11]]));
    let gain_q8 = i16::from_le_bytes([packet[16], packet[17]]);
    let family = packet[18];
    if channels == 0 {
        return Err(malformed("no channels"));
    }
    let table = match family {
        0 if channels <= 2 => None,
        0 => return Err(malformed("mapping family 0 has one or two channels")),
        // Family 1 is Vorbis order for up to 8 channels; 255 is unordered. Both are plain
        // multistream decoding. Families 2 and 3 are ambisonics and need a renderer.
        1 | 255 => {
            let (Some(&streams), Some(&coupled), Some(mapping)) =
                (packet.get(19), packet.get(20), packet.get(21..21 + usize::from(channels)))
            else {
                return Err(malformed("the channel mapping table is cut short"));
            };
            let (streams, coupled) = (usize::from(streams), usize::from(coupled));
            if streams == 0 || coupled > streams || streams + coupled > 255 {
                return Err(malformed("impossible stream counts"));
            }
            if mapping.iter().any(|&index| index != 255 && usize::from(index) >= streams + coupled) {
                return Err(malformed("a channel maps to a stream that is not there"));
            }
            if family == 1 && channels > 8 {
                return Err(malformed("mapping family 1 has at most eight channels"));
            }
            Some(Table { streams, coupled, mapping: mapping.to_vec() })
        }
        other => {
            return Err(DecodeError::unsupported(format!(
                "{what}: Opus channel mapping family {other} is not supported"
            )));
        }
    };
    let gain = 10f32.powf(f32::from(gain_q8) / (20.0 * 256.0));
    Ok((Layout { channels: usize::from(channels), table, gain }, pre_skip))
}

/// A packet's duration at 48 kHz from its TOC byte (RFC 6716 section 3.1), or 0 when it has
/// none to give.
fn duration(packet: &[u8]) -> u64 {
    let Some(&toc) = packet.first() else { return 0 };
    let config = toc >> 3;
    let frame: u64 = match config {
        0..=11 => [480, 960, 1920, 2880][usize::from(config % 4)],
        12..=15 => [480, 960][usize::from(config % 2)],
        _ => [120, 240, 480, 960][usize::from(config % 4)],
    };
    let frames = match toc & 3 {
        0 => 1,
        1 | 2 => 2,
        _ => packet.get(1).map_or(0, |count| u64::from(count & 0x3f)),
    };
    let total = frame * frames;
    if total > MAX_PACKET as u64 { 0 } else { total }
}

/// Opens an Ogg Opus stream.
pub(crate) fn open(what: &str, bytes: &[u8]) -> Result<(PacketStream<Opus>, usize), DecodeError> {
    let packets = Packets::read(what, bytes)?;
    if packets.len() < HEADERS {
        return Err(DecodeError::malformed(format!("{what}: the Opus headers are missing")));
    }
    let (layout, pre_skip) = head(what, packets.get(0))?;
    if !packets.get(1).starts_with(b"OpusTags") {
        return Err(DecodeError::malformed(format!("{what}: OpusTags is missing")));
    }
    let counts: Vec<u64> = (HEADERS..packets.len()).map(|index| duration(packets.get(index))).collect();
    let timeline = Timeline::new(&counts, &packets, HEADERS, pre_skip, false);
    let channels = layout.channels;
    let opus = Opus { inner: layout.decoder()?, layout, scratch: vec![0.0; MAX_PACKET * channels] };
    Ok((PacketStream::new(opus, packets, HEADERS, channels, timeline), channels))
}

impl PacketDecoder for Opus {
    fn decode(&mut self, packet: usize, data: &[u8], out: &mut Vec<f32>) -> Result<usize, DecodeError> {
        if data.is_empty() {
            return Ok(0);
        }
        let len = i32::try_from(data.len())
            .map_err(|_| DecodeError::malformed(format!("Opus packet {packet} is too large")))?;
        let room = MAX_PACKET as c_int;
        // SAFETY: the decoder is live and owned by `self`; `data` holds `len` bytes; `scratch`
        // holds MAX_PACKET frames of `channels` samples, the `frame_size` given.
        let decoded = unsafe {
            match &self.inner {
                Inner::Single(decoder) => {
                    opus_decode_float(decoder.as_ptr(), data.as_ptr(), len, self.scratch.as_mut_ptr(), room, 0)
                }
                Inner::Multi(decoder) => opus_multistream_decode_float(
                    decoder.as_ptr(),
                    data.as_ptr(),
                    len,
                    self.scratch.as_mut_ptr(),
                    room,
                    0,
                ),
            }
        };
        let Ok(frames) = usize::try_from(decoded) else {
            return Err(DecodeError::malformed(format!("bad Opus packet {packet}: {}", strerror(decoded))));
        };
        let samples = &self.scratch[..frames * self.layout.channels];
        if (self.layout.gain - 1.0).abs() > f32::EPSILON {
            out.extend(samples.iter().map(|sample| sample * self.layout.gain));
        } else {
            out.extend_from_slice(samples);
        }
        Ok(frames)
    }

    fn reset(&mut self) -> Result<(), DecodeError> {
        self.inner = self.layout.decoder()?;
        Ok(())
    }

    fn restart(&self, timeline: &Timeline, target: u64, _holder: usize) -> usize {
        timeline.holder(target.saturating_sub(PRE_ROLL))
    }
}

#[cfg(test)]
mod tests {
    use super::duration;

    #[test]
    fn durations_come_from_the_toc() {
        // SILK 20 ms, one frame; CELT 2.5 ms, two frames; hybrid 10 ms, three frames (code 3).
        assert_eq!(duration(&[(1 << 3), 0]), 960);
        assert_eq!(duration(&[(16 << 3) | 1, 0]), 240);
        assert_eq!(duration(&[(12 << 3) | 3, 3]), 1440);
        // CELT 20 ms times 63 frames is more than a packet can hold.
        assert_eq!(duration(&[(31 << 3) | 3, 63]), 0);
        assert_eq!(duration(&[]), 0);
    }
}
