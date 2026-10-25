//! Ogg Vorbis through lewton's packet decoder.
//!
//! Each packet's output length is known without decoding it (its block size and its
//! neighbour's), so the timeline is exact, and a seek is exact too: decoding packet `i - 1` with
//! a fresh decoder gives nothing but the window packet `i` overlaps, after which packet `i`
//! decodes exactly as it does in a straight run.

use lewton::audio::{PreviousWindowRight, get_decoded_sample_count, read_audio_packet_generic};
use lewton::header::{IdentHeader, SetupHeader, read_header_ident, read_header_setup};
use lewton::samples::InterleavedSamples;

use super::DecodeError;
use super::ogg::Packets;
use super::packeted::{PacketDecoder, PacketStream, Timeline};

/// The identification, comment and setup headers.
const HEADERS: usize = 3;

pub(crate) struct Vorbis {
    ident: IdentHeader,
    setup: SetupHeader,
    window: PreviousWindowRight,
}

/// Opens an Ogg Vorbis stream.
pub(crate) fn open(what: &str, bytes: &[u8]) -> Result<(PacketStream<Vorbis>, u32, usize), DecodeError> {
    let packets = Packets::read(what, bytes)?;
    if packets.len() < HEADERS {
        return Err(DecodeError::malformed(format!("{what}: the Vorbis headers are missing")));
    }
    let ident = read_header_ident(packets.get(0))
        .map_err(|error| DecodeError::malformed(format!("{what}: bad Vorbis identification header: {error}")))?;
    let setup = read_header_setup(packets.get(2), ident.audio_channels, (ident.blocksize_0, ident.blocksize_1))
        .map_err(|error| DecodeError::malformed(format!("{what}: bad Vorbis setup header: {error}")))?;
    let mut counts = Vec::with_capacity(packets.len() - HEADERS);
    for index in HEADERS..packets.len() {
        // The first audio packet only primes the window: it outputs nothing.
        let count = if index == HEADERS {
            0
        } else {
            get_decoded_sample_count(&ident, &setup, packets.get(index)).map_or(0, |count| count as u64)
        };
        counts.push(count);
    }
    let timeline = Timeline::new(&counts, &packets, HEADERS, 0, true);
    let (rate, channels) = (ident.audio_sample_rate, usize::from(ident.audio_channels));
    let vorbis = Vorbis { ident, setup, window: PreviousWindowRight::new() };
    Ok((PacketStream::new(vorbis, packets, HEADERS, channels, timeline), rate, channels))
}

impl PacketDecoder for Vorbis {
    fn decode(&mut self, packet: usize, data: &[u8], out: &mut Vec<f32>) -> Result<usize, DecodeError> {
        let decoded: InterleavedSamples<f32> =
            read_audio_packet_generic(&self.ident, &self.setup, data, &mut self.window)
                .map_err(|error| DecodeError::malformed(format!("bad Vorbis audio packet {packet}: {error}")))?;
        out.extend_from_slice(&decoded.samples);
        Ok(decoded.samples.len() / usize::from(self.ident.audio_channels))
    }

    fn reset(&mut self) -> Result<(), DecodeError> {
        self.window = PreviousWindowRight::new();
        Ok(())
    }

    fn restart(&self, _timeline: &Timeline, _target: u64, holder: usize) -> usize {
        holder.saturating_sub(1)
    }
}
