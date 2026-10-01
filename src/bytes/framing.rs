//! Framing length-prefixed chunks (`bytes.frame`): one native walk over a run of
//! tag-length-payload chunks, the shape most binary container formats share (RIFF, IFF, PNG,
//! Bethesda's records and subrecords, glTF's GLB), returning where every chunk is.
//!
//! The layout is data, not a format: how long a chunk's header is, where in it the payload's
//! length sits and how wide it is, and in which byte order. A chunk is its header followed by
//! `length` payload bytes. An `inner` layout frames each chunk's payload in turn and checks that
//! its chunks tile it exactly, without recording them: the walk a parser does to validate a
//! file before it trusts any offset in it.
//!
//! The result is a buffer of `u32` pairs, each chunk's start and end, a count, and, when the
//! walk stopped early, why and where: `"truncated"` (a header that doesn't fit), `"overrun"` (a
//! payload past the end), or `"innerTruncated"`/`"innerOverrun"` for a chunk inside one. The
//! chunks before the problem are still in the result, so a caller can report it in its own
//! words and carry on with what framed.

use crate::bind::Call;
use crate::convert::{BytesView, Exact, NewBuffer};
use crate::error::{Error, Result};
use crate::extension::ModuleDecl;
use crate::options::Options;
use crate::stack::ValueView;

const WHAT: &str = "bytes.frame";

/// How one level of chunks is laid out.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Layout {
    /// Bytes before the payload, the length field included.
    pub header: usize,
    /// Where the payload length sits in the header.
    pub length_at: usize,
    /// The length field's width: 1, 2, 4 or 8 bytes.
    pub length_size: usize,
    /// Whether the length field is big-endian.
    pub big_endian: bool,
}

impl Layout {
    fn read(options: &mut Options<'_, '_>) -> Result<Layout> {
        let header = super::count(WHAT, options.required::<Exact<i64>>("header")?)?;
        let length_at = super::count(WHAT, options.required::<Exact<i64>>("lengthAt")?)?;
        let length_size = super::count(WHAT, options.optional::<Exact<i64>>("lengthSize")?.unwrap_or(Exact(4)))?;
        let big_endian = options.optional::<bool>("bigEndian")?.unwrap_or(false);
        if !matches!(length_size, 1 | 2 | 4 | 8) {
            return Err(Error::runtime(format!("{WHAT}: lengthSize {length_size} is not 1, 2, 4 or 8")));
        }
        if length_at + length_size > header {
            return Err(Error::runtime(format!(
                "{WHAT}: the length field ({length_size} bytes at {length_at}) doesn't fit a {header}-byte header"
            )));
        }
        Ok(Layout { header, length_at, length_size, big_endian })
    }

    /// The payload length of the chunk at `at`, whose header is known to fit.
    #[inline]
    fn length(&self, data: &[u8], at: usize) -> u64 {
        let field = &data[at + self.length_at..at + self.length_at + self.length_size];
        let mut bytes = [0u8; 8];
        if self.big_endian {
            bytes[8 - self.length_size..].copy_from_slice(field);
            u64::from_be_bytes(bytes)
        } else {
            bytes[..self.length_size].copy_from_slice(field);
            u64::from_le_bytes(bytes)
        }
    }

    /// The end of the chunk at `at` inside `[at, finish)`, or the problem.
    #[inline]
    fn chunk(&self, data: &[u8], at: usize, finish: usize) -> std::result::Result<usize, Stop> {
        if at + self.header > finish {
            return Err(Stop::Truncated);
        }
        let end = (at + self.header) as u64 + self.length(data, at);
        if end > finish as u64 {
            return Err(Stop::Overrun);
        }
        // Checked against `finish` above, which is a usize.
        #[allow(clippy::cast_possible_truncation)]
        Ok(end as usize)
    }
}

/// Why a walk stopped before the end.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stop {
    /// A header that doesn't fit.
    Truncated,
    /// A payload that runs past the end.
    Overrun,
}

/// The chunks a walk framed, and where and why it stopped early, if it did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Frames {
    /// Each chunk's start and end.
    pub spans: Vec<(usize, usize)>,
    /// The problem, whether it was inside a chunk (`true`), and its offset.
    pub stop: Option<(Stop, bool, usize)>,
}

/// Frames `data[offset..finish]` with `outer`, checking each chunk's payload with `inner`.
#[must_use]
pub fn frame(data: &[u8], offset: usize, finish: usize, outer: Layout, inner: Option<Layout>) -> Frames {
    let mut frames = Frames::default();
    let mut at = offset;
    while at < finish {
        let end = match outer.chunk(data, at, finish) {
            Ok(end) => end,
            Err(stop) => {
                frames.stop = Some((stop, false, at));
                return frames;
            }
        };
        if let Some(inner) = inner {
            let mut p = at + outer.header;
            while p < end {
                match inner.chunk(data, p, end) {
                    Ok(next) => p = next,
                    Err(stop) => {
                        frames.stop = Some((stop, true, p));
                        return frames;
                    }
                }
            }
        }
        frames.spans.push((at, end));
        at = end;
    }
    frames
}

fn frame_binding(
    call: &Call<'_>,
    source: BytesView<'_>,
    layout: ValueView<'_>,
    offset: Option<Exact<i64>>,
    finish: Option<Exact<i64>>,
) -> Result<(NewBuffer, f64, Option<&'static str>, Option<f64>)> {
    let (outer, inner) = Options::read(call, layout, WHAT, |o| {
        let outer = Layout::read(o)?;
        let inner = o.nested("inner", Layout::read)?;
        Ok((outer, inner))
    })?;
    // SAFETY: the walk reads the view and calls nothing; the result is owned.
    let data = unsafe { super::bytes(&source) };
    let offset = offset.map_or(Ok(0), |value| super::count(WHAT, value))?;
    let finish = finish.map_or(Ok(data.len()), |value| super::count(WHAT, value))?;
    if offset > finish || finish > data.len() {
        return Err(Error::runtime(format!(
            "{WHAT}: range {offset}..{finish} is outside the source (length {})",
            data.len()
        )));
    }
    if u32::try_from(finish).is_err() {
        return Err(Error::runtime(format!("{WHAT}: offsets past 4 GiB don't fit the u32 spans")));
    }
    let frames = frame(data, offset, finish, outer, inner);
    let mut spans = Vec::with_capacity(frames.spans.len() * 8);
    for (start, end) in &frames.spans {
        // Both fit a u32: checked against `finish` above.
        #[allow(clippy::cast_possible_truncation)]
        {
            spans.extend_from_slice(&(*start as u32).to_le_bytes());
            spans.extend_from_slice(&(*end as u32).to_le_bytes());
        }
    }
    let (problem, at) = match frames.stop {
        None => (None, None),
        Some((stop, nested, at)) => (
            Some(match (stop, nested) {
                (Stop::Truncated, false) => "truncated",
                (Stop::Overrun, false) => "overrun",
                (Stop::Truncated, true) => "innerTruncated",
                (Stop::Overrun, true) => "innerOverrun",
            }),
            Some(at as f64),
        ),
    };
    Ok((NewBuffer(spans), frames.spans.len() as f64, problem, problem.and(at)))
}

pub(crate) fn describe(module: &mut ModuleDecl) {
    module
        .function("frame", frame_binding)
        .signature(
            "(source: buffer | string, layout: { header: number, lengthAt: number, lengthSize: number?, bigEndian: boolean?, inner: { header: number, lengthAt: number, lengthSize: number?, bigEndian: boolean? }? }, offset: number?, finish: number?) -> (buffer, number, string?, number?)",
        )
        .doc("Frames the tag-length-payload chunks in [offset, finish): a buffer of u32 start/end pairs, their count, and why and where the walk stopped early, if it did. `inner` checks each payload's chunks.");
}

#[cfg(test)]
mod tests {
    use super::*;

    const RECORD: Layout = Layout { header: 16, length_at: 4, length_size: 4, big_endian: false };
    const FIELD: Layout = Layout { header: 8, length_at: 4, length_size: 4, big_endian: false };

    fn chunk(tag: [u8; 4], header: usize, payload: &[u8]) -> Vec<u8> {
        let mut out = tag.to_vec();
        out.extend_from_slice(&u32::try_from(payload.len()).unwrap().to_le_bytes());
        out.resize(header, 0);
        out.extend_from_slice(payload);
        out
    }

    #[test]
    fn frames_records_and_checks_their_fields() {
        let fields = [chunk(*b"NAME", 8, b"id\0"), chunk(*b"DATA", 8, &[1, 2, 3, 4])].concat();
        let data = [chunk(*b"STAT", 16, &fields), chunk(*b"MISC", 16, &[])].concat();
        let frames = frame(&data, 0, data.len(), RECORD, Some(FIELD));
        assert_eq!(frames.stop, None);
        assert_eq!(frames.spans, vec![(0, 16 + fields.len()), (16 + fields.len(), data.len())]);
    }

    #[test]
    fn says_where_and_why_it_stopped() {
        let good = chunk(*b"STAT", 16, &chunk(*b"NAME", 8, b"a"));
        // The second record's payload is too short for even one field header.
        let data = [good.clone(), chunk(*b"MISC", 16, &[0; 4])].concat();
        let frames = frame(&data, 0, data.len(), RECORD, Some(FIELD));
        assert_eq!(frames.spans, vec![(0, good.len())]);
        assert_eq!(frames.stop, Some((Stop::Truncated, true, good.len() + 16)));

        let short = &data[..good.len() + 10];
        let frames = frame(short, 0, short.len(), RECORD, None);
        assert_eq!(frames.stop, Some((Stop::Truncated, false, good.len())));

        let mut long = good.clone();
        long[4] = 200;
        let frames = frame(&long, 0, long.len(), RECORD, None);
        assert_eq!(frames.stop, Some((Stop::Overrun, false, 0)));
    }

    #[test]
    fn reads_big_endian_and_narrow_lengths() {
        // A PNG-like chunk: 4-byte big-endian length first, then the tag; the CRC is left out.
        let layout = Layout { header: 8, length_at: 0, length_size: 4, big_endian: true };
        let data = [&[0, 0, 0, 3][..], b"IHDR", b"abc"].concat();
        assert_eq!(frame(&data, 0, data.len(), layout, None).spans, vec![(0, 11)]);
        let narrow = Layout { header: 3, length_at: 2, length_size: 1, big_endian: false };
        assert_eq!(frame(&[7, 7, 2, 9, 9], 0, 5, narrow, None).spans, vec![(0, 5)]);
    }
}
