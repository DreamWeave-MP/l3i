//! An Ogg file's first logical stream (RFC 3533), read once into its packets.
//!
//! Vorbis and Opus both want packets, not pages, and both seek by packet: indexing every packet
//! when the file opens makes a seek a binary search instead of a bisection over pages, and the
//! decoders never see Ogg framing again. The packets are copied out of their pages into one
//! allocation, so the index costs about the file's own size.
//!
//! Pages are checked against their CRC. A damaged one is skipped by scanning for the next
//! capture pattern, and a packet that spanned it, or a gap in the page sequence, is dropped whole
//! rather than decoded half.

use std::ops::Range;

use super::DecodeError;

/// Ogg's granule position for "no packet ends on this page".
const NO_GRANULE: u64 = u64::MAX;
const CAPTURE: &[u8; 4] = b"OggS";
/// A page header without its segment table.
const HEADER: usize = 27;

/// Ogg's CRC-32: polynomial 0x04c11db7, unreflected, from zero, no final xor.
const CRC_TABLE: [u32; 256] = {
    let mut table = [0u32; 256];
    let mut index = 0;
    while index < 256 {
        let mut value = (index as u32) << 24;
        let mut bit = 0;
        while bit < 8 {
            value = if value & 0x8000_0000 != 0 { (value << 1) ^ 0x04c1_1db7 } else { value << 1 };
            bit += 1;
        }
        table[index] = value;
        index += 1;
    }
    table
};

/// `crc` carried on over `bytes`.
fn crc_update(crc: u32, bytes: &[u8]) -> u32 {
    bytes.iter().fold(crc, |crc, &byte| (crc << 8) ^ CRC_TABLE[usize::from((crc >> 24) as u8 ^ byte)])
}

fn crc(bytes: &[u8]) -> u32 {
    crc_update(0, bytes)
}

/// One page: its flags, position, stream, and the lacing values and body.
struct Page<'a> {
    continued: bool,
    last: bool,
    granule: u64,
    serial: u32,
    sequence: u32,
    lacing: &'a [u8],
    body: &'a [u8],
}

/// The page at the start of `bytes` and its length, if one is whole there and its CRC holds.
fn page(bytes: &[u8]) -> Option<(Page<'_>, usize)> {
    if bytes.len() < HEADER || &bytes[..4] != CAPTURE || bytes[4] != 0 {
        return None;
    }
    let segments = usize::from(bytes[26]);
    let lacing = bytes.get(HEADER..HEADER + segments)?;
    let length = HEADER + segments + lacing.iter().map(|&value| usize::from(value)).sum::<usize>();
    let whole = bytes.get(..length)?;
    let stated = u32::from_le_bytes([whole[22], whole[23], whole[24], whole[25]]);
    let mut header = [0u8; HEADER];
    header.copy_from_slice(&whole[..HEADER]);
    header[22..26].fill(0);
    if crc_update(crc(&header), &whole[HEADER..]) != stated {
        return None;
    }
    let field = |at: usize| u32::from_le_bytes([whole[at], whole[at + 1], whole[at + 2], whole[at + 3]]);
    let page = Page {
        continued: whole[5] & 1 != 0,
        last: whole[5] & 4 != 0,
        granule: u64::from_le_bytes(whole[6..14].try_into().ok()?),
        serial: field(14),
        sequence: field(18),
        lacing,
        body: &whole[HEADER + segments..],
    };
    Some((page, length))
}

/// The pages of `bytes` in order, skipping over damage to the next page that checks out.
fn pages(bytes: &[u8]) -> impl Iterator<Item = Page<'_>> {
    let mut at = 0;
    std::iter::from_fn(move || {
        while at < bytes.len() {
            if let Some((page, length)) = page(&bytes[at..]) {
                at += length;
                return Some(page);
            }
            at += 1;
            at += bytes[at.min(bytes.len())..].windows(4).position(|window| window == CAPTURE).unwrap_or(bytes.len());
        }
        None
    })
}

/// One logical stream's packets, in order.
pub(crate) struct Packets {
    data: Vec<u8>,
    ranges: Vec<Range<usize>>,
    /// For each packet, the granule position of the page it ends, if it is the last packet to
    /// end there: the position just after it, in the codec's own units.
    granules: Vec<Option<u64>>,
}

impl Packets {
    /// Reads every packet of the first logical stream in `bytes`. Pages of other streams
    /// (a multiplexed video track, a chained stream after this one ends) are skipped.
    pub(crate) fn read(what: &str, bytes: &[u8]) -> Result<Packets, DecodeError> {
        let mut packets = Packets { data: Vec::with_capacity(bytes.len()), ranges: Vec::new(), granules: Vec::new() };
        let mut serial = None;
        let mut sequence: Option<u32> = None;
        // Where the packet still being assembled starts in `data`, if one runs on to the next page.
        let mut partial: Option<usize> = None;
        for page in pages(bytes) {
            let stream = *serial.get_or_insert(page.serial);
            if page.serial != stream {
                continue;
            }
            let gap = sequence.is_some_and(|last| page.sequence != last.wrapping_add(1));
            sequence = Some(page.sequence);
            if let Some(start) = partial.take_if(|_| gap || !page.continued) {
                // The rest of that packet is lost: drop what there is of it.
                packets.data.truncate(start);
            }
            let mut skipping = page.continued && partial.is_none();
            let ends = page.lacing.iter().filter(|&&value| value < 255).count();
            let (mut offset, mut ended) = (0, 0);
            for &value in page.lacing {
                let piece = &page.body[offset..offset + usize::from(value)];
                offset += usize::from(value);
                if skipping {
                    // The tail of a packet whose start was lost.
                    skipping = value == 255;
                    if !skipping {
                        ended += 1;
                    }
                    continue;
                }
                let start = *partial.get_or_insert(packets.data.len());
                packets.data.extend_from_slice(piece);
                if value == 255 {
                    continue;
                }
                partial = None;
                ended += 1;
                packets.ranges.push(start..packets.data.len());
                let granule = (ended == ends && page.granule != NO_GRANULE).then_some(page.granule);
                packets.granules.push(granule);
            }
            if page.last {
                break;
            }
        }
        if let Some(start) = partial {
            packets.data.truncate(start);
        }
        if packets.ranges.is_empty() && serial.is_none() {
            return Err(DecodeError::malformed(format!("{what}: not an Ogg stream: no page")));
        }
        Ok(packets)
    }

    pub(crate) fn len(&self) -> usize {
        self.ranges.len()
    }

    pub(crate) fn get(&self, index: usize) -> &[u8] {
        &self.data[self.ranges[index].clone()]
    }

    /// The last granule position the stream states at or after packet `first`, and the packet
    /// whose page states it.
    pub(crate) fn last_granule(&self, first: usize) -> Option<(usize, u64)> {
        (first..self.len()).rev().find_map(|index| self.granules[index].map(|granule| (index, granule)))
    }

    /// The first granule position the stream states at or after packet `first`, and the packet
    /// whose page states it.
    pub(crate) fn first_granule(&self, first: usize) -> Option<(usize, u64)> {
        (first..self.len()).find_map(|index| self.granules[index].map(|granule| (index, granule)))
    }
}

#[cfg(test)]
mod tests {
    use super::{Packets, crc};

    /// A page of `packets` (each under 255 bytes) for stream 7, sequence `sequence`.
    fn page(sequence: u32, granule: u64, flags: u8, lacing: &[u8], body: &[u8]) -> Vec<u8> {
        let mut page = b"OggS\0".to_vec();
        page.push(flags);
        page.extend_from_slice(&granule.to_le_bytes());
        page.extend_from_slice(&7u32.to_le_bytes());
        page.extend_from_slice(&sequence.to_le_bytes());
        page.extend_from_slice(&[0; 4]);
        page.push(u8::try_from(lacing.len()).unwrap());
        page.extend_from_slice(lacing);
        page.extend_from_slice(body);
        let sum = crc(&page);
        page[22..26].copy_from_slice(&sum.to_le_bytes());
        page
    }

    #[test]
    fn the_crc_is_oggs() {
        // CRC-32/POSIX's check value for "123456789" before its final inversion: the same
        // polynomial and direction, from zero.
        assert_eq!(crc(b"123456789"), 0x89a1_897f);
    }

    #[test]
    fn packets_span_pages_and_damage_drops_only_what_it_touches() {
        let long = [9u8; 300];
        let mut file = page(0, 0, 2, &[3], b"abc");
        // A 300-byte packet over two pages, then a short one.
        file.extend(page(1, u64::MAX, 0, &[255], &long[..255]));
        file.extend(page(2, 40, 1, &[45, 2], &[&long[255..], &b"de"[..]].concat()));
        let packets = Packets::read("test", &file).unwrap();
        assert_eq!(packets.len(), 3);
        assert_eq!((packets.get(1).len(), packets.get(2)), (300, &b"de"[..]));
        // The long packet ends on the last page but is not its last packet: no granule of its own.
        assert_eq!(packets.first_granule(1), Some((2, 40)));

        // Garbage over the middle page: the long packet goes, the rest stays.
        let mut damaged = file.clone();
        let middle = page(0, 0, 2, &[3], b"abc").len() + 10;
        damaged[middle] ^= 0xff;
        let packets = Packets::read("test", &damaged).unwrap();
        assert_eq!(packets.len(), 2);
        assert_eq!((packets.get(0), packets.get(1)), (&b"abc"[..], &b"de"[..]));
    }
}
