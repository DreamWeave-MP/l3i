//! MPEG audio (layers I to III) through nanomp3.
//!
//! nanomp3 skips ID3 and APE tags, trims the encoder delay and padding a LAME or Xing header
//! records (so a loop cut in an encoder plays without a gap), scans the stream for its length
//! when it opens, and seeks to an exact frame by decoding the frames the bit reservoir needs
//! first.

use std::io::Cursor;
use std::panic::{AssertUnwindSafe, catch_unwind};

use nanomp3::Reader;

use super::DecodeError;

pub(crate) struct Mp3 {
    reader: Reader<Cursor<Vec<u8>>, f32>,
    rate: u32,
    channels: usize,
    frames: Option<u64>,
    position: u64,
    failed: bool,
}

fn reader_error(what: &str, error: &nanomp3::Error) -> DecodeError {
    match error {
        nanomp3::Error::FormatChanged => {
            DecodeError::unsupported(format!("{what}: the stream's rate, layer or channels change partway"))
        }
        other => DecodeError::malformed(format!("{what}: {other}")),
    }
}

impl Mp3 {
    pub(crate) fn open(what: &str, bytes: Vec<u8>) -> Result<Mp3, DecodeError> {
        let reader = catch_unwind(AssertUnwindSafe(|| Reader::<_, f32>::new(Cursor::new(bytes))))
            .map_err(|_| DecodeError::failed(format!("{what}: the decoder failed reading the stream")))?
            .map_err(|error| reader_error(what, &error))?;
        let Some(channels) = reader.channels() else {
            return Err(DecodeError::malformed(format!("{what}: no MPEG audio frames")));
        };
        Ok(Mp3 {
            rate: reader.sample_rate(),
            channels: usize::from(channels.num()),
            frames: reader.total_samples(),
            reader,
            position: 0,
            failed: false,
        })
    }

    pub(crate) fn rate(&self) -> u32 {
        self.rate
    }

    pub(crate) fn channels(&self) -> usize {
        self.channels
    }

    pub(crate) fn frames(&self) -> Option<u64> {
        self.frames
    }

    pub(crate) fn position(&self) -> u64 {
        self.position
    }

    pub(crate) fn read(&mut self, what: &str, out: &mut [f32]) -> Result<usize, DecodeError> {
        if self.failed {
            return Err(DecodeError::failed(format!("{what}: the decoder failed earlier; seek to start again")));
        }
        let whole = out.len() / self.channels * self.channels;
        let reader = &mut self.reader;
        let samples = match catch_unwind(AssertUnwindSafe(|| reader.read(&mut out[..whole]))) {
            Ok(result) => result.map_err(|error| reader_error(what, &error))?,
            Err(_) => {
                self.failed = true;
                return Err(DecodeError::failed(format!("{what}: the decoder failed at frame {}", self.position)));
            }
        };
        let frames = samples / self.channels;
        self.position += frames as u64;
        Ok(frames)
    }

    pub(crate) fn seek(&mut self, what: &str, frame: u64) -> Result<(), DecodeError> {
        let frame = self.frames.map_or(frame, |frames| frame.min(frames));
        let reader = &mut self.reader;
        match catch_unwind(AssertUnwindSafe(|| reader.seek(frame))) {
            Ok(result) => result.map_err(|error| reader_error(what, &error))?,
            Err(_) => {
                self.failed = true;
                return Err(DecodeError::failed(format!("{what}: the decoder failed seeking to frame {frame}")));
            }
        }
        self.failed = false;
        self.position = frame;
        Ok(())
    }
}
