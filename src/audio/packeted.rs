//! What Vorbis and Opus share: a stream of packets laid on a timeline, read into interleaved
//! frames, trimmed at both ends, and sought by packet.
//!
//! Positions here are "computed": frames counted from the first audio packet's output, before
//! any trimming. The stream's granule positions place that count in its own time, which says
//! how much to trim from the start and where the last frame is.

use std::ops::Range;
use std::panic::{AssertUnwindSafe, catch_unwind};

use super::DecodeError;
use super::ogg::Packets;

/// A codec's packet decoder.
pub(crate) trait PacketDecoder {
    /// Decodes audio packet `packet` (the packet's bytes are `data`), appending interleaved
    /// frames to `out`; returns how many frames it appended.
    fn decode(&mut self, packet: usize, data: &[u8], out: &mut Vec<f32>) -> Result<usize, DecodeError>;

    /// Forgets everything decoded so far, as if the next packet were the stream's first.
    fn reset(&mut self) -> Result<(), DecodeError>;

    /// The packet to start decoding from so that the output is right from computed frame
    /// `target` on, given the packet that holds it.
    fn restart(&self, timeline: &Timeline, target: u64, holder: usize) -> usize;
}

/// Where each audio packet's output lies, and which part of it is the stream.
pub(crate) struct Timeline {
    /// `starts[i]` is the computed frame packet `i`'s output begins at; one more entry than
    /// packets, ending with the total.
    starts: Vec<u64>,
    /// The computed frames that are the stream: before `first` is trimmed (pre-skip, a granule
    /// that starts late), from `end` on is padding.
    span: Range<u64>,
}

impl Timeline {
    /// `counts` are each audio packet's output in frames; `skip` is what the codec trims from
    /// the start whatever the granules say (Opus's pre-skip); `start_trim` is whether a first
    /// granule smaller than the output so far trims the start (Vorbis) or is ignored (Opus,
    /// where RFC 7845 makes it an error except on the last page).
    pub(crate) fn new(counts: &[u64], packets: &Packets, audio: usize, skip: u64, start_trim: bool) -> Timeline {
        let mut starts = Vec::with_capacity(counts.len() + 1);
        let mut total = 0u64;
        starts.push(0);
        for count in counts {
            total += count;
            starts.push(total);
        }
        let (Some((first_packet, first_granule)), Some((last_packet, last_granule))) =
            (packets.first_granule(audio), packets.last_granule(audio))
        else {
            return Timeline { starts, span: skip.min(total)..total };
        };
        let computed = starts[first_packet - audio + 1];
        // Granule = computed + offset. A single-page stream's granule only ever trims the end.
        let offset = if first_packet == last_packet { 0 } else { i128::from(first_granule) - i128::from(computed) };
        let offset = if start_trim { offset } else { offset.max(0) };
        let first = u64::try_from(-offset).unwrap_or(0).max(skip).min(total);
        let end = u64::try_from(i128::from(last_granule) - offset).unwrap_or(0).clamp(first, total);
        Timeline { starts, span: first..end }
    }

    pub(crate) fn frames(&self) -> u64 {
        self.span.end - self.span.start
    }

    /// The computed frame of stream frame 0.
    pub(crate) fn first(&self) -> u64 {
        self.span.start
    }

    /// The packet whose output holds computed frame `target`, or the packet count at the end.
    pub(crate) fn holder(&self, target: u64) -> usize {
        // The last start at or before the target; packets with no output share a start, and the
        // last of them is the one that outputs it.
        self.starts.partition_point(|&start| start <= target).saturating_sub(1).min(self.starts.len() - 1)
    }

    pub(crate) fn start(&self, packet: usize) -> u64 {
        self.starts[packet]
    }
}

/// A packet stream being read.
pub(crate) struct PacketStream<D> {
    decoder: D,
    packets: Packets,
    /// The index in `packets` of the first audio packet.
    audio: usize,
    channels: usize,
    timeline: Timeline,
    /// The next audio packet to decode.
    next: usize,
    /// The computed frame the next read returns.
    cursor: u64,
    pending: Vec<f32>,
    /// The computed frame of `pending`'s first frame.
    pending_start: u64,
    /// A decoder that panicked is not trusted again.
    failed: bool,
}

impl<D: PacketDecoder> PacketStream<D> {
    pub(crate) fn new(decoder: D, packets: Packets, audio: usize, channels: usize, timeline: Timeline) -> Self {
        let cursor = timeline.first();
        let mut stream = PacketStream {
            decoder,
            packets,
            audio,
            channels,
            timeline,
            next: 0,
            cursor,
            pending: Vec::new(),
            pending_start: 0,
            failed: false,
        };
        stream.next = stream.decoder.restart(&stream.timeline, cursor, stream.timeline.holder(cursor));
        stream
    }

    pub(crate) fn frames(&self) -> u64 {
        self.timeline.frames()
    }

    pub(crate) fn position(&self) -> u64 {
        self.cursor - self.timeline.first()
    }

    fn audio_packets(&self) -> usize {
        self.packets.len() - self.audio
    }

    /// Fills `out` (whole frames) from the stream; returns the frames written, 0 at the end.
    pub(crate) fn read(&mut self, what: &str, out: &mut [f32]) -> Result<usize, DecodeError> {
        if self.failed {
            return Err(DecodeError::failed(format!("{what}: the decoder failed earlier; seek to start again")));
        }
        let channels = self.channels;
        let want = out.len() / channels;
        let mut written = 0;
        while written < want && self.cursor < self.timeline.span.end {
            let held = (self.pending.len() / channels) as u64;
            if self.cursor >= self.pending_start && self.cursor < self.pending_start + held {
                let from = usize::try_from(self.cursor - self.pending_start).unwrap_or(usize::MAX);
                let left = self.timeline.span.end - self.cursor;
                let count = (held as usize - from).min(want - written).min(usize::try_from(left).unwrap_or(usize::MAX));
                out[written * channels..(written + count) * channels]
                    .copy_from_slice(&self.pending[from * channels..(from + count) * channels]);
                written += count;
                self.cursor += count as u64;
                continue;
            }
            if self.next >= self.audio_packets() {
                break;
            }
            self.decode_next(what)?;
        }
        Ok(written)
    }

    fn decode_next(&mut self, what: &str) -> Result<(), DecodeError> {
        let packet = self.next;
        self.next += 1;
        self.pending.clear();
        self.pending_start = self.timeline.start(packet);
        let data = self.packets.get(self.audio + packet);
        let (decoder, pending) = (&mut self.decoder, &mut self.pending);
        // The decoders are third-party Rust fed whatever the file holds; a panic in one is a
        // malformed stream, not a reason to take the host down. Nothing here calls Lua.
        match catch_unwind(AssertUnwindSafe(|| decoder.decode(packet, data, pending))) {
            Ok(Ok(_)) => {
                // The timeline is the stream's length, so a packet's output is held to its
                // place on it: a decoder that has just been reset gives nothing for the packet
                // it primes from (frames before the cursor anyway), and a packet that decodes
                // short is padded rather than shifting everything after it.
                let expected = self.timeline.start(packet + 1) - self.timeline.start(packet);
                self.pending.resize(usize::try_from(expected).unwrap_or(usize::MAX) * self.channels, 0.0);
                Ok(())
            }
            Ok(Err(error)) => {
                self.pending.clear();
                Err(error.within(what))
            }
            Err(_) => {
                self.failed = true;
                self.pending.clear();
                Err(DecodeError::failed(format!("{what}: the decoder failed on packet {packet}")))
            }
        }
    }

    /// Moves to stream frame `frame` (clamped to the end).
    pub(crate) fn seek(&mut self, what: &str, frame: u64) -> Result<(), DecodeError> {
        let target = self.timeline.first().saturating_add(frame).min(self.timeline.span.end);
        let decoder = &mut self.decoder;
        match catch_unwind(AssertUnwindSafe(|| decoder.reset())) {
            Ok(Ok(())) => {}
            Ok(Err(error)) => return Err(error.within(what)),
            Err(_) => return Err(DecodeError::failed(format!("{what}: the decoder failed to reset"))),
        }
        self.failed = false;
        self.pending.clear();
        self.cursor = target;
        self.next = self.decoder.restart(&self.timeline, target, self.timeline.holder(target));
        Ok(())
    }
}
