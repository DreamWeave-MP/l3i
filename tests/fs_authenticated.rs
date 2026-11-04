//! Authenticated ranged reads: every valid range byte-for-byte against the object, and the same
//! reader against forged manifests, damaged chunks and sources that misbehave on purpose.

use std::cell::{Cell, RefCell};
use std::io;
use std::path::{Path, PathBuf};

use l3i::fs::authenticated::{
    AuthError, AuthenticatedReader, Commitment, MAX_CHUNK_SIZE, MAX_MANIFEST_LEN, MIN_CHUNK_SIZE, RangeSource,
    VerifiedManifest, build_manifest,
};
use sha2::{Digest as _, Sha256};

const CHUNK: u32 = 512;
const HEADER: usize = 32;

fn object(length: usize) -> Vec<u8> {
    (0..length).map(|i| (i % 251) as u8).collect()
}

/// What the caller holds after publishing `bytes`: the manifest, and the commitment it trusts.
fn published(bytes: &[u8], chunk_size: u32) -> (Vec<u8>, Commitment) {
    let published = build_manifest(bytes, chunk_size).unwrap();
    (published.bytes, published.commitment)
}

fn reader(bytes: &[u8], chunk_size: u32) -> AuthenticatedReader<&[u8]> {
    let (manifest, commitment) = published(bytes, chunk_size);
    AuthenticatedReader::open(bytes, VerifiedManifest::verify(&manifest, &commitment).unwrap()).unwrap()
}

/// The commitment an attacker could compute for bytes of their own, for the tests of what happens
/// when a forged manifest is somehow trusted.
fn commitment_of(bytes: &[u8]) -> Commitment {
    Commitment::from_bytes(Sha256::digest(bytes).into())
}

/// A small reproducible stream of numbers; a failing seed reproduces the failing case.
struct Random(u64);

impl Random {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, bound: u64) -> u64 {
        self.next() % bound
    }
}

// ---------------------------------------------------------------------------------------------
// Functional: every shape of object and of range
// ---------------------------------------------------------------------------------------------

#[test]
fn every_range_of_every_interesting_object_shape_matches_the_object() {
    let chunk = CHUNK as usize;
    for length in [0, 1, 100, chunk - 1, chunk, chunk + 1, chunk * 2, chunk * 3 - 1, chunk * 3 + 1, chunk * 4] {
        let bytes = object(length);
        let mut reader = reader(&bytes, CHUNK);
        assert_eq!(reader.manifest().object_length(), length as u64);
        assert_eq!(reader.manifest().chunk_count(), (length as u64).div_ceil(u64::from(CHUNK)));
        // The offsets around every chunk boundary and the object's end, to every such end.
        let mut marks: Vec<usize> = vec![0, length];
        for boundary in (0..=length).step_by(chunk) {
            marks.extend([boundary.saturating_sub(1), boundary, (boundary + 1).min(length)]);
        }
        marks.sort_unstable();
        marks.dedup();
        for &start in &marks {
            for &end in marks.iter().filter(|&&end| end >= start) {
                let got = reader.read_at(start as u64, end - start).unwrap();
                assert_eq!(got, bytes[start..end], "object {length}, range {start}..{end}");
            }
        }
    }
}

#[test]
fn randomized_reads_agree_with_a_reader_that_verifies_the_whole_object_first() {
    for seed in [0x5EED_0001_u64, 0x5EED_0002, 0x5EED_0003, 0x5EED_0004] {
        let mut random = Random(seed);
        let length = random.below(40_000) as usize;
        let bytes: Vec<u8> = (0..length).map(|_| random.next() as u8).collect();
        let chunk_size = MIN_CHUNK_SIZE << random.below(4);
        let (manifest, commitment) = published(&bytes, chunk_size);
        let mut authenticated =
            AuthenticatedReader::open(&bytes[..], VerifiedManifest::verify(&manifest, &commitment).unwrap()).unwrap();
        // The reference: the whole object checked once, then sliced for every range.
        assert_eq!(build_manifest(&bytes[..], chunk_size).unwrap().commitment, commitment, "seed {seed:#x}");
        for _ in 0..500 {
            let offset = random.below(length as u64 + 1) as usize;
            let count = random.below((length - offset) as u64 + 1) as usize;
            let got = authenticated.read_at(offset as u64, count).unwrap();
            assert_eq!(got, bytes[offset..offset + count], "seed {seed:#x}, {count} at {offset}");
        }
    }
}

#[test]
fn reads_repeat_in_any_order_and_overlap_without_disturbing_each_other() {
    let bytes = object(CHUNK as usize * 6 + 17);
    let mut reader = reader(&bytes, CHUNK);
    for (offset, count) in [(2000, 700), (0, 3), (2000, 700), (2500, 1), (1, 2600), (0, 2577), (2576, 1)] {
        assert_eq!(reader.read_at(offset, count).unwrap(), bytes[offset as usize..offset as usize + count]);
    }
}

#[test]
fn read_into_fills_exactly_the_target() {
    let bytes = object(CHUNK as usize * 3);
    let mut reader = reader(&bytes, CHUNK);
    let mut target = [7; 700];
    reader.read_into(300, &mut target).unwrap();
    assert_eq!(target, bytes[300..1000]);
}

#[test]
fn empty_reads_succeed_up_to_the_end_and_touch_nothing() {
    let bytes = object(1000);
    let source = Scripted::new(bytes.clone());
    let (manifest, commitment) = published(&bytes, CHUNK);
    let mut reader =
        AuthenticatedReader::open(&source, VerifiedManifest::verify(&manifest, &commitment).unwrap()).unwrap();
    assert_eq!(reader.read_at(0, 0).unwrap(), Vec::<u8>::new());
    assert_eq!(reader.read_at(1000, 0).unwrap(), Vec::<u8>::new());
    assert_eq!(source.calls.get(), 0, "an empty read needs no bytes");

    let empty: &[u8] = &[];
    let mut nothing = self::reader(empty, CHUNK);
    assert_eq!(nothing.read_at(0, 0).unwrap(), Vec::<u8>::new());
    assert!(matches!(nothing.read_at(0, 1), Err(AuthError::OutOfRange { .. })));
}

#[test]
fn a_range_leaving_the_object_is_refused_before_any_read_or_allocation() {
    let bytes = object(1000);
    let source = Scripted::new(bytes.clone());
    let (manifest, commitment) = published(&bytes, CHUNK);
    let mut reader =
        AuthenticatedReader::open(&source, VerifiedManifest::verify(&manifest, &commitment).unwrap()).unwrap();
    for (offset, count) in [
        (1001, 0),
        (1000, 1),
        (999, 2),
        (0, 1001),
        (u64::MAX, 0),
        (u64::MAX, 1),
        (u64::MAX - 1, 3),
        (1, usize::MAX),
        (0, usize::MAX),
    ] {
        assert!(
            matches!(reader.read_at(offset, count), Err(AuthError::OutOfRange { .. })),
            "{count} bytes at {offset}"
        );
    }
    assert_eq!(source.calls.get(), 0);
}

// ---------------------------------------------------------------------------------------------
// The format: fixed vectors, computed by an independent implementation
// ---------------------------------------------------------------------------------------------

fn from_hex(text: &str) -> Vec<u8> {
    (0..text.len()).step_by(2).map(|at| u8::from_str_radix(&text[at..at + 2], 16).unwrap()).collect()
}

#[test]
fn the_format_matches_vectors_computed_elsewhere() {
    let manifest = from_hex(concat!(
        "4c3349415554480a0100012000020000010400000000000003000000000000",
        "004b3097c5ccdf7d167a8f0059c2b6805bdc5f335a78e6792ae3d50e2876b15896",
        "f828c850d614bc2e19f96513b3d4cf5d5a5ae6063775bac0c9be7d1f0a05a40c",
        "2f92cd2d5c4ae106a841a640a947a4457c188a25478f0495a5f5a15e44433c77",
    ));
    let bytes = object(1025);
    let built = build_manifest(&bytes[..], 512).unwrap();
    assert_eq!(built.bytes, manifest);
    assert_eq!(built.commitment.to_string(), "468e23fcc792358e3d899eb54977d648ee7b1be66ad4184d152c57c15d9da1ca");

    let empty: &[u8] = &[];
    let built = build_manifest(empty, 512).unwrap();
    assert_eq!(built.bytes, from_hex("4c3349415554480a010001200002000000000000000000000000000000000000"));
    assert_eq!(built.commitment.to_string(), "7d34d5a1af2d46797b9c7caed4cd90763a236f2f6714e12f06bac92eb8950eba");
}

#[test]
fn commitments_read_hex_of_either_case_and_nothing_else() {
    let hex = "468e23fcc792358e3d899eb54977d648ee7b1be66ad4184d152c57c15d9da1ca";
    let commitment = Commitment::from_hex(hex.as_bytes()).unwrap();
    assert_eq!(commitment.to_string(), hex);
    assert_eq!(Commitment::from_hex(hex.to_uppercase().as_bytes()).unwrap(), commitment);
    assert_eq!(Commitment::from_bytes(*commitment.as_bytes()), commitment);
    for bad in [&hex[..63], &hex[..62], "", &format!("{hex}0"), &format!("g{}", &hex[1..]), &format!("é{}", &hex[2..])]
    {
        assert!(matches!(Commitment::from_hex(bad.as_bytes()), Err(AuthError::InvalidCommitment)), "{bad}");
    }
}

#[test]
fn only_power_of_two_chunk_sizes_in_bounds_build() {
    let bytes = object(10);
    for bad in [0, 1, 256, MIN_CHUNK_SIZE - 1, MIN_CHUNK_SIZE + 1, 3000, MAX_CHUNK_SIZE * 2, u32::MAX] {
        assert!(matches!(build_manifest(&bytes[..], bad), Err(AuthError::InvalidChunkSize(_))), "{bad}");
    }
    for good in [MIN_CHUNK_SIZE, 4096, MAX_CHUNK_SIZE] {
        assert_eq!(reader(&bytes, good).read_at(0, 10).unwrap(), bytes);
    }
}

// ---------------------------------------------------------------------------------------------
// The commitment: nothing is believed before it
// ---------------------------------------------------------------------------------------------

#[test]
fn a_single_flipped_bit_anywhere_in_a_manifest_fails_its_commitment() {
    let (manifest, commitment) = published(&object(1300), CHUNK);
    for byte in 0..manifest.len() {
        let mut forged = manifest.clone();
        forged[byte] ^= 1;
        assert!(matches!(VerifiedManifest::verify(&forged, &commitment), Err(AuthError::ManifestMismatch)), "{byte}");
    }
    let mut extended = manifest.clone();
    extended.push(0);
    assert!(matches!(VerifiedManifest::verify(&extended, &commitment), Err(AuthError::ManifestMismatch)));
    assert!(matches!(
        VerifiedManifest::verify(&manifest[..manifest.len() - 1], &commitment),
        Err(AuthError::ManifestMismatch)
    ));
    assert!(matches!(VerifiedManifest::verify(&[], &commitment), Err(AuthError::ManifestMismatch)));
}

#[test]
fn a_flipped_commitment_rejects_the_right_manifest() {
    let (manifest, commitment) = published(&object(1300), CHUNK);
    let mut wrong = *commitment.as_bytes();
    wrong[31] ^= 0x80;
    assert!(matches!(
        VerifiedManifest::verify(&manifest, &Commitment::from_bytes(wrong)),
        Err(AuthError::ManifestMismatch)
    ));
}

#[test]
fn replacing_both_object_and_manifest_is_caught_however_consistent_they_are() {
    let original = object(2000);
    let (_, trusted) = published(&original, CHUNK);
    let mut replacement = original.clone();
    replacement[10] ^= 0xFF;
    let (forged_manifest, forged_commitment) = published(&replacement, CHUNK);
    // The forgery is perfectly self-consistent: it opens against its own commitment.
    let forged = VerifiedManifest::verify(&forged_manifest, &forged_commitment).unwrap();
    assert_eq!(AuthenticatedReader::open(&replacement[..], forged).unwrap().read_at(10, 1).unwrap(), [replacement[10]]);
    // And it means nothing against the commitment the caller actually trusts.
    assert!(matches!(VerifiedManifest::verify(&forged_manifest, &trusted), Err(AuthError::ManifestMismatch)));
}

#[test]
fn an_oversized_manifest_is_refused_without_being_hashed() {
    let (_, commitment) = published(&object(10), CHUNK);
    let huge = vec![0; MAX_MANIFEST_LEN + 1];
    assert!(matches!(VerifiedManifest::verify(&huge, &commitment), Err(AuthError::ManifestTooLong { .. })));
}

/// Applies `change` to a valid manifest's bytes and trusts the result, so that the parser, not the
/// commitment, is what must refuse it.
fn forge(change: impl FnOnce(&mut Vec<u8>)) -> Result<VerifiedManifest, AuthError> {
    let (mut manifest, _) = published(&object(1300), CHUNK);
    change(&mut manifest);
    let commitment = commitment_of(&manifest);
    VerifiedManifest::verify(&manifest, &commitment)
}

fn set(manifest: &mut [u8], at: usize, field: &[u8]) {
    manifest[at..at + field.len()].copy_from_slice(field);
}

#[test]
fn a_committed_manifest_that_is_not_a_manifest_is_refused_by_name() {
    assert!(matches!(forge(|m| m[0] ^= 1), Err(AuthError::MalformedManifest("wrong magic"))));
    assert!(matches!(forge(|m| set(m, 8, &2_u16.to_le_bytes())), Err(AuthError::UnsupportedVersion(2))));
    assert!(matches!(forge(|m| set(m, 8, &0_u16.to_le_bytes())), Err(AuthError::UnsupportedVersion(0))));
    assert!(matches!(forge(|m| m[10] = 2), Err(AuthError::UnsupportedAlgorithm(2))));
    assert!(matches!(forge(|m| m[11] = 16), Err(AuthError::MalformedManifest("wrong digest length"))));
    for bad in [0_u32, 1, 256, 513, 1000, MAX_CHUNK_SIZE * 2, u32::MAX] {
        assert!(
            matches!(
                forge(|m| set(m, 12, &bad.to_le_bytes())),
                Err(AuthError::MalformedManifest("invalid chunk size"))
            ),
            "{bad}"
        );
    }
    // A chunk size that is valid but no longer agrees with the count, and the reverse.
    assert!(matches!(forge(|m| set(m, 12, &1024_u32.to_le_bytes())), Err(AuthError::MalformedManifest(_))));
    assert!(matches!(forge(|m| set(m, 16, &5000_u64.to_le_bytes())), Err(AuthError::MalformedManifest(_))));
    assert!(matches!(forge(|m| set(m, 24, &2_u64.to_le_bytes())), Err(AuthError::MalformedManifest(_))));
    assert!(matches!(forge(|m| set(m, 24, &0_u64.to_le_bytes())), Err(AuthError::MalformedManifest(_))));
    assert!(matches!(forge(|m| m.truncate(HEADER)), Err(AuthError::MalformedManifest(_))));
    assert!(matches!(forge(|m| m.truncate(HEADER - 1)), Err(AuthError::MalformedManifest(_))));
    assert!(matches!(forge(Vec::clear), Err(AuthError::MalformedManifest(_))));
    assert!(matches!(forge(|m| m.extend_from_slice(&[0; 32])), Err(AuthError::MalformedManifest(_))));
    assert!(matches!(forge(|m| m.push(0)), Err(AuthError::MalformedManifest(_))));
}

#[test]
fn claims_that_would_overflow_or_exhaust_memory_stop_at_the_header() {
    // Each is refused on the numbers alone: none of them has anywhere near the bytes it claims.
    assert!(matches!(
        forge(|m| set(m, 24, &u64::MAX.to_le_bytes())),
        Err(AuthError::MalformedManifest("too many chunks"))
    ));
    assert!(matches!(forge(|m| set(m, 16, &u64::MAX.to_le_bytes())), Err(AuthError::MalformedManifest(_))));
    assert!(matches!(
        forge(|m| {
            set(m, 16, &u64::MAX.to_le_bytes());
            set(m, 24, &(1_u64 << 55).to_le_bytes());
        }),
        Err(AuthError::MalformedManifest("too many chunks"))
    ));
    assert!(matches!(
        forge(|m| {
            set(m, 16, &(512_u64 << 24).to_le_bytes());
            set(m, 24, &(1_u64 << 24).to_le_bytes());
        }),
        Err(AuthError::MalformedManifest("the length does not fit the chunk count"))
    ));
}

#[test]
fn reordered_or_substituted_digests_fail_the_commitment_and_then_the_chunk() {
    let bytes = object(CHUNK as usize * 3);
    let (manifest, trusted) = published(&bytes, CHUNK);
    let mut swapped = manifest.clone();
    for at in 0..32 {
        swapped.swap(HEADER + at, HEADER + 32 + at);
    }
    assert!(matches!(VerifiedManifest::verify(&swapped, &trusted), Err(AuthError::ManifestMismatch)));
    // Trusted anyway, a swapped directory still cannot move a chunk: positions are in the digest.
    let forged = VerifiedManifest::verify(&swapped, &commitment_of(&swapped)).unwrap();
    let mut reader = AuthenticatedReader::open(&bytes[..], forged).unwrap();
    assert!(matches!(reader.read_at(0, 1), Err(AuthError::ChunkMismatch { index: 0 })));
    assert!(matches!(reader.read_at(512, 1), Err(AuthError::ChunkMismatch { index: 1 })));
    assert_eq!(reader.read_at(1024, 1).unwrap(), [bytes[1024]]);
}

// ---------------------------------------------------------------------------------------------
// Damaged objects
// ---------------------------------------------------------------------------------------------

#[test]
fn damage_fails_exactly_the_reads_that_cover_it() {
    let chunk = CHUNK as usize;
    let bytes = object(chunk * 4 + 100);
    let (manifest, commitment) = published(&bytes, CHUNK);
    let verified = std::sync::Arc::new(VerifiedManifest::verify(&manifest, &commitment).unwrap());
    // Every byte of the object damaged in turn, on a sample to keep this fast.
    for damaged_at in (0..bytes.len()).step_by(37).chain([chunk - 1, chunk, chunk * 4, bytes.len() - 1]) {
        let mut damaged = bytes.clone();
        damaged[damaged_at] ^= 0x40;
        let mut reader = AuthenticatedReader::open(&damaged[..], verified.clone()).unwrap();
        let index = (damaged_at / chunk) as u64;
        // A read of the damaged byte, of another byte of its chunk, and through it all fail.
        for (offset, count) in
            [(damaged_at, 1), (index as usize * chunk, 1), (0, bytes.len()), (damaged_at.saturating_sub(1), 2)]
        {
            assert!(matches!(reader.read_at(offset as u64, count), Err(AuthError::ChunkMismatch { .. })));
        }
        // Reads of chunks that hold no damage are not refused; they were not looked at.
        for other in (0..=4).filter(|other| *other != index) {
            let start = other as usize * chunk;
            let end = (start + chunk).min(bytes.len());
            assert_eq!(reader.read_at(start as u64, end - start).unwrap(), bytes[start..end]);
        }
    }
}

#[test]
fn swapping_two_chunks_of_the_object_is_caught_even_when_their_lengths_agree() {
    let chunk = CHUNK as usize;
    let bytes = object(chunk * 3);
    let (manifest, commitment) = published(&bytes, CHUNK);
    let mut swapped = bytes.clone();
    swapped[..chunk].copy_from_slice(&bytes[chunk..chunk * 2]);
    swapped[chunk..chunk * 2].copy_from_slice(&bytes[..chunk]);
    let verified = VerifiedManifest::verify(&manifest, &commitment).unwrap();
    let mut reader = AuthenticatedReader::open(&swapped[..], verified).unwrap();
    assert!(matches!(reader.read_at(0, 1), Err(AuthError::ChunkMismatch { index: 0 })));
    assert!(matches!(reader.read_at(chunk as u64, 1), Err(AuthError::ChunkMismatch { index: 1 })));
}

#[test]
fn identical_chunks_at_different_positions_have_different_digests() {
    let bytes = vec![9; CHUNK as usize * 2];
    let (manifest, _) = published(&bytes, CHUNK);
    assert_ne!(manifest[HEADER..HEADER + 32], manifest[HEADER + 32..HEADER + 64]);
}

#[test]
fn a_source_that_panics_mid_read_leaves_no_trace_either() {
    struct Panics<'a>(&'a [u8]);
    impl RangeSource for Panics<'_> {
        fn read_at(&self, offset: u64, dst: &mut [u8]) -> io::Result<usize> {
            dst.fill(0xBB);
            assert!(offset < 512, "this source fails after the first chunk");
            self.0.read_at(offset, dst)
        }
        fn len(&self) -> io::Result<u64> {
            RangeSource::len(self.0)
        }
    }
    let chunk = CHUNK as usize;
    let bytes = object(chunk * 3);
    let (manifest, commitment) = published(&bytes, CHUNK);
    let verified = VerifiedManifest::verify(&manifest, &commitment).unwrap();
    let mut reader = AuthenticatedReader::open(Panics(&bytes), verified).unwrap();
    // A whole first chunk verifies into the target, then a partial one panics while being read.
    let mut target = vec![0xAA; chunk + 10];
    let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| reader.read_into(0, &mut target)));
    assert!(caught.is_err());
    assert!(target.iter().all(|byte| *byte == 0), "a panic left bytes behind");
    // The panicked chunk is not treated as held, and the reader still answers correctly.
    assert_eq!(reader.read_at(1, 3).unwrap(), bytes[1..4]);
}

#[test]
fn a_failed_read_leaves_no_trace_in_its_target() {
    let chunk = CHUNK as usize;
    let bytes = object(chunk * 4);
    let mut damaged = bytes.clone();
    // The last chunk is damaged, so the earlier chunks of a long read verify before it fails.
    damaged[chunk * 3 + 5] ^= 1;
    let (manifest, commitment) = published(&bytes, CHUNK);
    let verified = std::sync::Arc::new(VerifiedManifest::verify(&manifest, &commitment).unwrap());
    let mut reader = AuthenticatedReader::open(&damaged[..], verified.clone()).unwrap();
    for (offset, length) in [(0, chunk * 4), (100, chunk * 3 + 200), (chunk * 3, chunk), (chunk * 3 + 5, 1)] {
        let mut target = vec![0xAA; length];
        let result = reader.read_into(offset as u64, &mut target);
        assert!(matches!(result, Err(AuthError::ChunkMismatch { index: 3 })), "{offset}+{length}");
        assert!(target.iter().all(|byte| *byte == 0), "{offset}+{length} left bytes behind");
    }
    // A range refused for its bounds also says so by leaving nothing, however long the target.
    let mut target = vec![0xAA; 10];
    assert!(reader.read_into(chunk as u64 * 4, &mut target).is_err());
    assert!(target.iter().all(|byte| *byte == 0));
}

// ---------------------------------------------------------------------------------------------
// Sources that misbehave
// ---------------------------------------------------------------------------------------------

type Hook = Box<dyn FnMut(usize, &mut Vec<u8>)>;

/// A source that counts its calls and can act differently on any of them.
struct Scripted {
    bytes: RefCell<Vec<u8>>,
    calls: Cell<usize>,
    read_bytes: Cell<u64>,
    /// The most any one call returns.
    limit: Cell<usize>,
    /// Calls that fail with this kind, once each, in order of the call number.
    failures: RefCell<Vec<(usize, io::ErrorKind)>>,
    /// Runs on each call before it reads: mutates the bytes as another process might.
    hook: RefCell<Option<Hook>>,
    /// Every call reports having read this many more bytes than it was given.
    lie: Cell<usize>,
    /// What `len` reports instead of the truth.
    claimed_len: Cell<Option<u64>>,
    fail_len: Cell<bool>,
}

impl Scripted {
    fn new(bytes: Vec<u8>) -> Scripted {
        Scripted {
            bytes: RefCell::new(bytes),
            calls: Cell::new(0),
            read_bytes: Cell::new(0),
            limit: Cell::new(usize::MAX),
            failures: RefCell::new(Vec::new()),
            hook: RefCell::new(None),
            lie: Cell::new(0),
            claimed_len: Cell::new(None),
            fail_len: Cell::new(false),
        }
    }
}

impl RangeSource for Scripted {
    fn read_at(&self, offset: u64, dst: &mut [u8]) -> io::Result<usize> {
        let call = self.calls.get();
        self.calls.set(call + 1);
        if let Some(hook) = self.hook.borrow_mut().as_mut() {
            hook(call, &mut self.bytes.borrow_mut());
        }
        let mut failures = self.failures.borrow_mut();
        if let Some(at) = failures.iter().position(|(number, _)| *number == call) {
            return Err(failures.remove(at).1.into());
        }
        let bytes = self.bytes.borrow();
        let start = usize::try_from(offset).unwrap().min(bytes.len());
        let count = dst.len().min(bytes.len() - start).min(self.limit.get());
        dst[..count].copy_from_slice(&bytes[start..start + count]);
        self.read_bytes.set(self.read_bytes.get() + count as u64);
        Ok(if count == 0 { 0 } else { count + self.lie.get() })
    }

    fn len(&self) -> io::Result<u64> {
        if self.fail_len.get() {
            return Err(io::ErrorKind::PermissionDenied.into());
        }
        Ok(self.claimed_len.get().unwrap_or(self.bytes.borrow().len() as u64))
    }
}

/// A scripted copy of the object and a reader of it through the object's true manifest. The source
/// is leaked so a test can keep directing it while the reader holds it.
fn scripted(bytes: &[u8], chunk_size: u32) -> (&'static Scripted, AuthenticatedReader<&'static Scripted>) {
    let (manifest, commitment) = published(bytes, chunk_size);
    let verified = VerifiedManifest::verify(&manifest, &commitment).unwrap();
    let source: &'static Scripted = Box::leak(Box::new(Scripted::new(bytes.to_vec())));
    (source, AuthenticatedReader::open(source, verified).unwrap())
}

#[test]
fn a_read_costs_only_the_chunks_it_covers() {
    let chunk = u64::from(CHUNK);
    let bytes = object(CHUNK as usize * 100);
    let (manifest, commitment) = published(&bytes, CHUNK);
    let source = Scripted::new(bytes.clone());
    let mut reader =
        AuthenticatedReader::open(&source, VerifiedManifest::verify(&manifest, &commitment).unwrap()).unwrap();
    assert_eq!(reader.read_at(10 * chunk + 1, 10).unwrap(), bytes[5121..5131]);
    assert_eq!((source.calls.get(), source.read_bytes.get()), (1, chunk));
    source.calls.set(0);
    source.read_bytes.set(0);
    // Across one boundary, the first chunk of it still held: one more read, not two.
    assert_eq!(reader.read_at(11 * chunk - 1, 2).unwrap(), bytes[5631..5633]);
    assert_eq!((source.calls.get(), source.read_bytes.get()), (1, chunk));
    source.calls.set(0);
    source.read_bytes.set(0);
    // Whole chunks in the middle read as one run: a partial, the run, a partial.
    assert_eq!(reader.read_at(20 * chunk + 1, 10 * CHUNK as usize).unwrap(), bytes[10241..15361]);
    assert_eq!((source.calls.get(), source.read_bytes.get()), (3, 11 * chunk));
    source.calls.set(0);
    source.read_bytes.set(0);
    // Whole chunks exactly: one read, nothing more than asked.
    assert_eq!(reader.read_at(30 * chunk, 5 * CHUNK as usize).unwrap(), bytes[15360..17920]);
    assert_eq!((source.calls.get(), source.read_bytes.get()), (1, 5 * chunk));
}

#[test]
fn small_reads_in_order_verify_each_chunk_once() {
    let chunk = CHUNK as usize;
    let bytes = object(chunk * 3);
    let (source, mut reader) = scripted(&bytes, CHUNK);
    for offset in (0..bytes.len() - 8).step_by(8) {
        assert_eq!(reader.read_at(offset as u64, 8).unwrap(), bytes[offset..offset + 8]);
    }
    assert_eq!((source.calls.get(), source.read_bytes.get()), (3, bytes.len() as u64));
}

#[test]
fn the_held_chunk_returns_committed_bytes_after_the_file_changes() {
    let chunk = CHUNK as usize;
    let bytes = object(chunk * 2);
    let (source, reader) = scripted(&bytes, CHUNK);
    let mut reader = reader.hold_bytes(0);
    assert_eq!(reader.read_at(5, 1).unwrap(), [bytes[5]]);
    source.bytes.borrow_mut()[5] ^= 1;
    // Served from the verified copy: the right bytes, whatever the file says now.
    assert_eq!(reader.read_at(5, 1).unwrap(), [bytes[5]]);
    // Once the slot moves on, the damage is met like any other.
    assert_eq!(reader.read_at(chunk as u64, 1).unwrap(), [bytes[chunk]]);
    assert!(matches!(reader.read_at(5, 1), Err(AuthError::ChunkMismatch { index: 0 })));
}

#[test]
fn the_budget_bounds_what_is_held_and_the_oldest_goes_first() {
    let chunk = u64::from(CHUNK);
    let bytes = object(CHUNK as usize * 6);
    let (source, reader) = scripted(&bytes, CHUNK);
    let mut reader = reader.hold_bytes(2 * chunk);
    let read = |reader: &mut AuthenticatedReader<&'static Scripted>, index: u64| {
        let at = index * chunk + 1;
        assert_eq!(reader.read_at(at, 1).unwrap(), [bytes[at as usize]]);
    };
    read(&mut reader, 0);
    read(&mut reader, 1);
    assert_eq!(source.calls.get(), 2);
    // Both held, in either order, and using the older makes the other the older.
    read(&mut reader, 0);
    read(&mut reader, 1);
    read(&mut reader, 0);
    assert_eq!(source.calls.get(), 2);
    read(&mut reader, 2);
    assert_eq!(source.calls.get(), 3, "chunk 1 went, chunk 0 stayed");
    read(&mut reader, 0);
    assert_eq!(source.calls.get(), 3);
    read(&mut reader, 1);
    assert_eq!(source.calls.get(), 4, "chunk 1 had gone");
    // A budget under one chunk still holds one, since a partial read needs somewhere to verify.
    let (source, reader) = scripted(&bytes, CHUNK);
    let mut reader = reader.hold_bytes(1);
    read(&mut reader, 0);
    read(&mut reader, 0);
    read(&mut reader, 1);
    read(&mut reader, 0);
    assert_eq!(source.calls.get(), 3);
}

#[test]
fn the_default_budget_absorbs_reads_that_turn_about() {
    let chunk = u64::from(CHUNK);
    let bytes = object(CHUNK as usize * 6);
    let (source, mut reader) = scripted(&bytes, CHUNK);
    for _ in 0..50 {
        assert_eq!(reader.read_at(1, 1).unwrap(), [bytes[1]]);
        assert_eq!(reader.read_at(4 * chunk + 1, 1).unwrap(), [bytes[4 * CHUNK as usize + 1]]);
    }
    assert_eq!(source.calls.get(), 2);
}

#[test]
fn damage_to_one_held_chunk_never_changes_another() {
    let chunk = CHUNK as usize;
    let bytes = object(chunk * 4);
    let (source, mut reader) = scripted(&bytes, CHUNK);
    assert_eq!(reader.read_at(1, 1).unwrap(), [bytes[1]]);
    source.bytes.borrow_mut()[chunk * 2 + 1] ^= 1;
    // A load that fails keeps what was held and holds nothing new, however often it is asked.
    for _ in 0..3 {
        assert!(matches!(reader.read_at(chunk as u64 * 2 + 1, 1), Err(AuthError::ChunkMismatch { index: 2 })));
    }
    assert_eq!(reader.read_at(1, 1).unwrap(), [bytes[1]]);
    assert_eq!(source.calls.get(), 4, "one good load and three failed ones");
}

#[test]
fn a_chunk_that_failed_is_never_treated_as_held() {
    let chunk = CHUNK as usize;
    let bytes = object(chunk * 2);
    let (source, reader) = scripted(&bytes, CHUNK);
    let mut reader = reader.hold_bytes(0);
    assert_eq!(reader.read_at(chunk as u64, 1).unwrap(), [bytes[chunk]]);
    source.bytes.borrow_mut()[5] ^= 1;
    // The failure overwrote the slot that held chunk 1, and it must not count as either.
    assert!(matches!(reader.read_at(5, 1), Err(AuthError::ChunkMismatch { index: 0 })));
    assert!(matches!(reader.read_at(5, 1), Err(AuthError::ChunkMismatch { index: 0 })));
    assert_eq!(reader.read_at(chunk as u64, 1).unwrap(), [bytes[chunk]]);
    // Short and failed reads clear it too, and a repaired source verifies again.
    source.bytes.borrow_mut()[5] ^= 1;
    source.failures.borrow_mut().push((source.calls.get(), io::ErrorKind::PermissionDenied));
    assert!(matches!(reader.read_at(5, 1), Err(AuthError::Io(_))));
    assert_eq!(reader.read_at(5, 1).unwrap(), [bytes[5]]);
}

#[test]
fn short_reads_are_retried_until_the_chunk_is_whole() {
    let bytes = object(CHUNK as usize * 5 + 9);
    let (source, mut reader) = scripted(&bytes, CHUNK);
    source.limit.set(7);
    assert_eq!(reader.read_at(100, bytes.len() - 100).unwrap(), bytes[100..]);
    assert_eq!(reader.read_at(3, 2).unwrap(), bytes[3..5]);
}

#[test]
fn interrupted_reads_are_retried() {
    let bytes = object(CHUNK as usize * 2);
    let (source, mut reader) = scripted(&bytes, CHUNK);
    *source.failures.borrow_mut() = vec![(0, io::ErrorKind::Interrupted), (1, io::ErrorKind::Interrupted)];
    assert_eq!(reader.read_at(1, 3).unwrap(), bytes[1..4]);
}

#[test]
fn errors_from_the_source_surface_and_leave_nothing() {
    let chunk = CHUNK as usize;
    let bytes = object(chunk * 4);
    for failing_call in 0..3 {
        let (source, mut reader) = scripted(&bytes, CHUNK);
        source.failures.borrow_mut().push((failing_call, io::ErrorKind::PermissionDenied));
        let mut target = vec![0xAA; chunk * 2 + 2];
        let result = reader.read_into(chunk as u64 - 1, &mut target);
        match result {
            Err(AuthError::Io(error)) => assert_eq!(error.kind(), io::ErrorKind::PermissionDenied),
            other => panic!("call {failing_call}: {other:?}"),
        }
        assert!(target.iter().all(|byte| *byte == 0));
    }
}

#[test]
fn a_source_that_shrinks_answers_with_the_chunk_it_cut() {
    let chunk = CHUNK as usize;
    let bytes = object(chunk * 4 + 1);
    // Cut inside chunk 2: chunks 0 and 1 still read, 2 is short, and past it is a short read of
    // nothing at all.
    let (source, mut reader) = scripted(&bytes, CHUNK);
    source.bytes.borrow_mut().truncate(chunk * 2 + 10);
    assert_eq!(reader.read_at(5, 5).unwrap(), bytes[5..10]);
    assert!(matches!(
        reader.read_at(chunk as u64 * 2, 1),
        Err(AuthError::ShortChunk { index: 2, expected: 512, actual: 10 })
    ));
    assert!(matches!(reader.read_at(chunk as u64 * 3, 1), Err(AuthError::ShortChunk { index: 3, actual: 0, .. })));
    // A run that reaches the cut reports the same chunk.
    assert!(matches!(reader.read_at(0, chunk * 4), Err(AuthError::ShortChunk { index: 2, expected: 512, actual: 10 })));
    // The final, shorter chunk's own length is enforced too.
    let (source, mut reader) = scripted(&bytes, CHUNK);
    source.bytes.borrow_mut().truncate(chunk * 4);
    assert!(matches!(
        reader.read_at(chunk as u64 * 4, 1),
        Err(AuthError::ShortChunk { index: 4, expected: 1, actual: 0 })
    ));
}

#[test]
fn a_source_truncated_and_refilled_with_other_bytes_fails_the_digest() {
    let chunk = CHUNK as usize;
    let bytes = object(chunk * 3);
    let (source, mut reader) = scripted(&bytes, CHUNK);
    source.bytes.borrow_mut().truncate(chunk);
    source.bytes.borrow_mut().resize(chunk * 3, 0);
    assert_eq!(reader.read_at(0, 1).unwrap(), bytes[..1]);
    assert!(matches!(reader.read_at(chunk as u64, 1), Err(AuthError::ChunkMismatch { index: 1 })));
}

#[test]
fn bytes_that_change_between_chunk_reads_fail_the_read() {
    let chunk = CHUNK as usize;
    let bytes = object(chunk * 3);
    // A range over partial, whole and partial chunks makes three reads; each case damages what
    // the next read will see, after the earlier chunks have already verified.
    for (call, damaged_at, index) in [(1, chunk + 7, 1), (2, chunk * 2 + 7, 2)] {
        let (source, mut reader) = scripted(&bytes, CHUNK);
        *source.hook.borrow_mut() = Some(Box::new(move |number, bytes| {
            if number == call {
                bytes[damaged_at] ^= 1;
            }
        }));
        let mut target = vec![0xAA; chunk * 3 - 2];
        let result = reader.read_into(1, &mut target);
        assert!(matches!(result, Err(AuthError::ChunkMismatch { index: got }) if got == index), "{result:?}");
        assert!(target.iter().all(|byte| *byte == 0));
    }
}

#[test]
fn a_source_that_claims_an_impossible_count_fails_without_a_panic() {
    let bytes = object(CHUNK as usize * 2);
    let (source, mut reader) = scripted(&bytes, CHUNK);
    source.lie.set(1);
    match reader.read_at(1, 3) {
        Err(AuthError::Io(error)) => assert_eq!(error.kind(), io::ErrorKind::InvalidData),
        other => panic!("{other:?}"),
    }
}

#[test]
fn opening_checks_the_committed_length() {
    let bytes = object(1000);
    let (manifest, commitment) = published(&bytes, CHUNK);
    let verified = std::sync::Arc::new(VerifiedManifest::verify(&manifest, &commitment).unwrap());
    let open = |source: &Scripted| AuthenticatedReader::open(source, verified.clone()).map(drop);
    assert!(open(&Scripted::new(bytes.clone())).is_ok());
    assert!(matches!(
        open(&Scripted::new(bytes[..999].to_vec())),
        Err(AuthError::LengthMismatch { expected: 1000, actual: 999 })
    ));
    let mut grown = bytes.clone();
    grown.push(0);
    assert!(matches!(open(&Scripted::new(grown)), Err(AuthError::LengthMismatch { expected: 1000, actual: 1001 })));
    let refuses = Scripted::new(bytes);
    refuses.fail_len.set(true);
    assert!(matches!(open(&refuses), Err(AuthError::Io(_))));
}

#[test]
fn a_source_that_lies_about_its_length_still_cannot_pass_a_short_chunk() {
    let bytes = object(1000);
    let (manifest, commitment) = published(&bytes, CHUNK);
    let source = Scripted::new(bytes[..600].to_vec());
    source.claimed_len.set(Some(1000));
    let verified = VerifiedManifest::verify(&manifest, &commitment).unwrap();
    let mut reader = AuthenticatedReader::open(source, verified).unwrap();
    assert_eq!(reader.read_at(0, 512).unwrap(), bytes[..512]);
    assert!(matches!(reader.read_at(512, 1), Err(AuthError::ShortChunk { index: 1, expected: 488, actual: 88 })));
}

#[test]
fn building_notices_a_source_whose_size_changes() {
    let bytes = object(CHUNK as usize * 3);
    let shrinking = Scripted::new(bytes.clone());
    *shrinking.hook.borrow_mut() = Some(Box::new(|number, bytes| {
        if number == 1 {
            bytes.truncate(600);
        }
    }));
    assert!(matches!(build_manifest(&shrinking, CHUNK), Err(AuthError::ShortChunk { index: 1, .. })));

    let growing = Scripted::new(bytes.clone());
    *growing.hook.borrow_mut() = Some(Box::new(|number, bytes| {
        if number == 2 {
            bytes.push(0);
        }
    }));
    assert!(matches!(build_manifest(&growing, CHUNK), Err(AuthError::LengthMismatch { .. })));

    let failing = Scripted::new(bytes);
    failing.failures.borrow_mut().push((1, io::ErrorKind::PermissionDenied));
    assert!(matches!(build_manifest(&failing, CHUNK), Err(AuthError::Io(_))));
}

// ---------------------------------------------------------------------------------------------
// Real files
// ---------------------------------------------------------------------------------------------

struct Scratch(PathBuf);

impl Scratch {
    fn new(name: &str) -> Scratch {
        let path = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("dream-auth-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).unwrap();
        Scratch(path)
    }

    fn lua(&self) -> String {
        format!("{:?}", self.0.display().to_string())
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A file with the object in it, and a reader of it through the true manifest.
fn file_reader(path: &Path, bytes: &[u8]) -> AuthenticatedReader<std::fs::File> {
    std::fs::write(path, bytes).unwrap();
    let file = std::fs::File::open(path).unwrap();
    let published = build_manifest(&file, CHUNK).unwrap();
    let verified = VerifiedManifest::verify(&published.bytes, &published.commitment).unwrap();
    AuthenticatedReader::open(file, verified).unwrap()
}

#[test]
fn a_file_truncated_under_an_open_reader_fails_instead_of_faulting() {
    let scratch = Scratch::new("truncate");
    let path = scratch.0.join("object");
    let bytes = object(CHUNK as usize * 4);
    let mut reader = file_reader(&path, &bytes);
    assert_eq!(reader.read_at(1100, 10).unwrap(), bytes[1100..1110]);
    std::fs::OpenOptions::new().write(true).open(&path).unwrap().set_len(600).unwrap();
    assert_eq!(reader.read_at(0, 512).unwrap(), bytes[..512]);
    assert!(matches!(reader.read_at(600, 1), Err(AuthError::ShortChunk { index: 1, expected: 512, actual: 88 })));
    assert!(matches!(reader.read_at(2000, 1), Err(AuthError::ShortChunk { index: 3, actual: 0, .. })));
}

#[test]
fn a_file_changed_in_place_under_an_open_reader_fails_the_chunk() {
    use std::io::{Seek, SeekFrom, Write};
    let scratch = Scratch::new("change");
    let path = scratch.0.join("object");
    let bytes = object(CHUNK as usize * 4);
    let mut reader = file_reader(&path, &bytes);
    assert_eq!(reader.read_at(1200, 1).unwrap(), [bytes[1200]]);
    let mut writer = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
    writer.seek(SeekFrom::Start(600)).unwrap();
    writer.write_all(&[bytes[600] ^ 1]).unwrap();
    drop(writer);
    assert!(matches!(reader.read_at(600, 1), Err(AuthError::ChunkMismatch { index: 1 })));
    assert!(matches!(reader.read_at(513, 1), Err(AuthError::ChunkMismatch { index: 1 })));
    assert_eq!(reader.read_at(2000, 1).unwrap(), [bytes[2000]]);
}

#[test]
fn a_file_replaced_by_rename_keeps_serving_the_revision_the_handle_holds() {
    let scratch = Scratch::new("rename");
    let path = scratch.0.join("object");
    let bytes = object(CHUNK as usize * 2);
    let mut reader = file_reader(&path, &bytes);
    let replacement = scratch.0.join("replacement");
    std::fs::write(&replacement, vec![0; bytes.len()]).unwrap();
    // Windows may refuse to rename over a file that is open; what it serves then is unchanged.
    let renamed = std::fs::rename(&replacement, &path);
    assert!(renamed.is_ok() || cfg!(windows));
    assert_eq!(reader.read_at(0, bytes.len()).unwrap(), bytes);
}

#[test]
fn a_file_that_grows_under_an_open_reader_never_returns_the_new_bytes() {
    use std::io::Write;
    let scratch = Scratch::new("grow");
    let path = scratch.0.join("object");
    let bytes = object(1000);
    let mut reader = file_reader(&path, &bytes);
    std::fs::OpenOptions::new().append(true).open(&path).unwrap().write_all(&[1; 2000]).unwrap();
    // What the commitment covers still verifies, and what it does not is out of range.
    assert_eq!(reader.read_at(0, 1000).unwrap(), bytes);
    assert!(matches!(reader.read_at(1000, 1), Err(AuthError::OutOfRange { .. })));
}

/// The invariant itself, under a real race: while a thread keeps flipping a byte in the file,
/// every read that succeeds holds exactly the object's bytes.
#[test]
fn a_racing_writer_can_fail_reads_but_never_change_what_one_returns() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    let scratch = Scratch::new("race");
    let path = scratch.0.join("object");
    let chunk = CHUNK as usize;
    let bytes = object(chunk * 8);
    let mut reader = file_reader(&path, &bytes);
    let stop = Arc::new(AtomicBool::new(false));
    let writer = {
        let (path, stop, original) = (path.clone(), stop.clone(), bytes[chunk * 3 + 1]);
        std::thread::spawn(move || {
            use std::io::{Seek, SeekFrom, Write};
            let mut file = std::fs::OpenOptions::new().write(true).open(path).unwrap();
            let mut flip = false;
            while !stop.load(Ordering::Relaxed) {
                flip = !flip;
                file.seek(SeekFrom::Start(chunk as u64 * 3 + 1)).unwrap();
                file.write_all(&[if flip { original ^ 0xFF } else { original }]).unwrap();
            }
        })
    };
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(250);
    let (mut good, mut failed) = (0_u32, 0_u32);
    while std::time::Instant::now() < deadline {
        match reader.read_at(chunk as u64 * 3 - 5, chunk + 10) {
            Ok(got) => {
                assert_eq!(got, bytes[chunk * 3 - 5..chunk * 4 + 5]);
                good += 1;
            }
            Err(AuthError::ChunkMismatch { index: 3 }) => failed += 1,
            Err(other) => panic!("{other:?}"),
        }
    }
    stop.store(true, Ordering::Relaxed);
    writer.join().unwrap();
    assert!(good + failed > 0);
}

// ---------------------------------------------------------------------------------------------
// Through Luau
// ---------------------------------------------------------------------------------------------

mod through_luau {
    use l3i::Runtime;
    use l3i::extension::{RuntimePlan, RuntimePolicy};
    use l3i::fs::{FsExtension, READ_CAPABILITY, WRITE_CAPABILITY};

    use super::{CHUNK, Scratch, object};

    fn runtime_with(capabilities: &[&str]) -> Runtime {
        let mut policy = RuntimePolicy::new().compat_global("@dream/fs", "fs");
        for capability in capabilities {
            policy = policy.capability(capability);
        }
        let plan = RuntimePlan::builder().policy(policy).extension(FsExtension).finalize().unwrap();
        Runtime::from_plan(&plan).unwrap()
    }

    fn runtime() -> Runtime {
        runtime_with(&[READ_CAPABILITY, WRITE_CAPABILITY])
    }

    /// The common opening of a test script: an object on disk and its publication.
    fn prologue(scratch: &Scratch, bytes: &[u8]) -> String {
        std::fs::write(scratch.0.join("object"), bytes).unwrap();
        format!(
            "local path = {root} .. '/object' \
             local manifest, commitment = assert(fs.authManifest(path, {{ chunkSize = {CHUNK} }})) \
             local whole = assert(fs.readFile(path)) ",
            root = scratch.lua()
        )
    }

    #[test]
    fn a_script_publishes_opens_and_reads() {
        let scratch = Scratch::new("luau-read");
        let script = prologue(&scratch, &object(CHUNK as usize * 3 + 5));
        runtime()
            .exec(&format!(
                "{script} \
                 assert(type(manifest) == 'buffer') \
                 assert(#commitment == 64 and commitment:match('^%x+$')) \
                 local reader = assert(fs.openAuthenticated(path, manifest, commitment)) \
                 assert(reader:size() == buffer.len(whole) and reader:chunkSize() == {CHUNK}) \
                 assert(reader:commitment() == commitment) \
                 assert(tostring(reader):find('AuthenticatedReader')) \
                 local function same(got, offset) \
                    for i = 0, buffer.len(got) - 1 do \
                        assert(buffer.readu8(got, i) == buffer.readu8(whole, offset + i), offset + i) \
                    end \
                 end \
                 for _, range in {{ {{0, 1}}, {{511, 2}}, {{100, 1300}}, {{0, buffer.len(whole)}}, {{buffer.len(whole) - 1, 1}} }} do \
                    local got = assert(reader:readAt(range[1], range[2])) \
                    assert(buffer.len(got) == range[2]) same(got, range[1]) \
                 end \
                 assert(buffer.len(assert(reader:readAt(buffer.len(whole), 0))) == 0) \
                 local target = buffer.create(100) \
                 assert(reader:readAtInto(target, 500, 20, 40) == 20) \
                 assert(buffer.readu8(target, 39) == 0 and buffer.readu8(target, 60) == 0) \
                 for i = 0, 19 do assert(buffer.readu8(target, 40 + i) == buffer.readu8(whole, 500 + i)) end \
                 reader:close() reader:close() \
                 assert(reader:size() == buffer.len(whole), 'what was committed outlives the handle') \
                 local ok, err = pcall(reader.readAt, reader, 0, 1) assert(not ok and err:find('closed'), err) \
                 ok, err = pcall(reader.readAtInto, reader, target, 0, 1) assert(not ok and err:find('closed'), err)"
            ))
            .unwrap();
    }

    #[test]
    fn the_manifest_may_be_a_string_and_the_commitment_either_case() {
        let scratch = Scratch::new("luau-string");
        let script = prologue(&scratch, &object(1000));
        runtime()
            .exec(&format!(
                "{script} \
                 local reader = assert(fs.openAuthenticated(path, buffer.tostring(manifest), commitment:upper())) \
                 assert(reader:commitment() == commitment)"
            ))
            .unwrap();
    }

    #[test]
    fn a_script_can_choose_what_a_reader_holds() {
        let scratch = Scratch::new("luau-held");
        let script = prologue(&scratch, &object(CHUNK as usize * 3));
        runtime()
            .exec(&format!(
                "{script} \
                 local function read(options) \
                    local reader = assert(fs.openAuthenticated(path, manifest, commitment, options)) \
                    assert(buffer.len(assert(reader:readAt(1, 1))) == 1, 'held chunk 0') \
                    assert(buffer.len(assert(reader:readAt(513, 1))) == 1, 'held chunk 1') \
                    return reader \
                 end \
                 local one, plenty = read({{ heldBytes = 0 }}), read({{ heldBytes = 1048576 }}) \
                 local default = read(nil) \
                 assert(fs.writeFile(path, 'x', {{ offset = 1 }})) \
                 local got, _, kind = one:readAt(1, 1) \
                 assert(got == nil and kind == 'integrityFailed', 'chunk 0 had been evicted') \
                 assert(buffer.readu8(assert(plenty:readAt(1, 1)), 0) == 1, 'chunk 0 is still held, committed bytes') \
                 assert(buffer.readu8(assert(default:readAt(1, 1)), 0) == 1, 'by default too')"
            ))
            .unwrap();
    }

    #[test]
    fn the_default_chunk_size_is_published() {
        let scratch = Scratch::new("luau-default");
        std::fs::write(scratch.0.join("object"), object(10)).unwrap();
        runtime()
            .exec(&format!(
                "local path = {root} .. '/object' \
                 local manifest, commitment = assert(fs.authManifest(path)) \
                 local reader = assert(fs.openAuthenticated(path, manifest, commitment)) \
                 assert(reader:chunkSize() == {default})",
                root = scratch.lua(),
                default = l3i::fs::authenticated::DEFAULT_CHUNK_SIZE
            ))
            .unwrap();
    }

    #[test]
    fn verification_failures_are_answers_with_their_own_kind() {
        let scratch = Scratch::new("luau-fail");
        let script = prologue(&scratch, &object(CHUNK as usize * 3));
        runtime()
            .exec(&format!(
                "{script} \
                 local forged = buffer.create(buffer.len(manifest)) buffer.copy(forged, 0, manifest) \
                 buffer.writeu8(forged, 40, bit32.bxor(buffer.readu8(forged, 40), 1)) \
                 local r, message, kind = fs.openAuthenticated(path, forged, commitment) \
                 assert(r == nil and kind == 'integrityFailed' and message:find('openAuthenticated') and message:find('trusted commitment'), message) \
                 r, message, kind = fs.openAuthenticated(path .. '.missing', manifest, commitment) \
                 assert(r == nil and kind == 'notFound', message) \
                 local reader = assert(fs.openAuthenticated(path, manifest, commitment)) \
                 assert(fs.writeFile(path, 'x', {{ offset = 600 }}) == 1) \
                 local got, why, how = reader:readAt(600, 1) \
                 assert(got == nil and how == 'integrityFailed' and why:find('chunk 1') and why:find('readAt'), why) \
                 local target = buffer.create(1024) \
                 buffer.fill(target, 0, 0xAA) \
                 local count, into_why, into_how = reader:readAtInto(target, 0, 1024) \
                 assert(count == nil and into_how == 'integrityFailed', into_why) \
                 for i = 0, 1023 do assert(buffer.readu8(target, i) == 0, 'a failed read left a byte') end \
                 assert(buffer.len(assert(reader:readAt(0, 512))) == 512, 'an untouched chunk is unaffected')"
            ))
            .unwrap();
    }

    #[test]
    fn a_file_of_the_wrong_length_is_refused_at_open_and_a_shrunken_one_at_the_read() {
        let scratch = Scratch::new("luau-len");
        let script = prologue(&scratch, &object(CHUNK as usize * 3));
        runtime()
            .exec(&format!(
                "{script} \
                 local reader = assert(fs.openAuthenticated(path, manifest, commitment)) \
                 assert(fs.writeFile(path, 'short')) \
                 local got, why, how = reader:readAt(600, 1) \
                 assert(got == nil and how == 'integrityFailed' and why:find('ended after'), why) \
                 local r, message, kind = fs.openAuthenticated(path, manifest, commitment) \
                 assert(r == nil and kind == 'integrityFailed' and message:find('commits to'), message)"
            ))
            .unwrap();
    }

    #[test]
    fn a_trusted_manifest_that_is_malformed_or_from_the_future_says_so() {
        let scratch = Scratch::new("luau-malformed");
        let script = prologue(&scratch, &object(100));
        // These commit to whatever they are, so the commitment passes and the parser must refuse.
        let not_manifest = "0".repeat(10);
        let not_manifest_commitment = l3i::fs::authenticated::Commitment::from_bytes(
            <sha2::Sha256 as sha2::Digest>::digest(&not_manifest).into(),
        );
        let mut future = l3i::fs::authenticated::build_manifest(&[1_u8; 100][..], CHUNK).unwrap().bytes;
        future[8] = 2;
        let future_commitment =
            l3i::fs::authenticated::Commitment::from_bytes(<sha2::Sha256 as sha2::Digest>::digest(&future).into());
        let future = future.iter().fold(String::new(), |mut text, byte| {
            std::fmt::Write::write_fmt(&mut text, format_args!("\\x{byte:02x}")).unwrap();
            text
        });
        runtime()
            .exec(&format!(
                "{script} \
                 local r, message, kind = fs.openAuthenticated(path, '{not_manifest}', '{not_manifest_commitment}') \
                 assert(r == nil and kind == 'invalidManifest' and message:find('malformed'), message) \
                 r, message, kind = fs.openAuthenticated(path, '{future}', '{future_commitment}') \
                 assert(r == nil and kind == 'unsupported' and message:find('version 2'), message)"
            ))
            .unwrap();
    }

    #[test]
    fn mistakes_raise_and_name_what_is_wrong() {
        let scratch = Scratch::new("luau-mistake");
        let script = prologue(&scratch, &object(1000));
        runtime()
            .exec(&format!(
                "{script} \
                 local reader = assert(fs.openAuthenticated(path, manifest, commitment)) \
                 local function refuses(pattern, f, ...) \
                    local ok, err = pcall(f, ...) \
                    assert(not ok and tostring(err):find(pattern), tostring(err)) \
                 end \
                 refuses('64 hex digits', fs.openAuthenticated, path, manifest, 'abc') \
                 refuses('64 hex digits', fs.openAuthenticated, path, manifest, string.rep('z', 64)) \
                 refuses('outside the object', reader.readAt, reader, 1001, 0) \
                 refuses('outside the object', reader.readAt, reader, 1000, 1) \
                 refuses('outside the object', reader.readAt, reader, 0, 1001) \
                 refuses('outside the object', reader.readAt, reader, 0, 2^53) \
                 refuses('outside the object', reader.readAt, reader, 2^53, 2^53) \
                 refuses('negative', reader.readAt, reader, -1, 1) \
                 refuses('negative', reader.readAt, reader, 0, -1) \
                 local target = buffer.create(10) \
                 refuses('does not fit', reader.readAtInto, reader, target, 0, 11) \
                 refuses('past the end', reader.readAtInto, reader, target, 0, 1, 11) \
                 refuses('outside the object', reader.readAtInto, reader, target, 1000, 1) \
                 refuses('power of two', fs.authManifest, path, {{ chunkSize = 100 }}) \
                 refuses('power of two', fs.authManifest, path, {{ chunkSize = -512 }}) \
                 refuses('power of two', fs.authManifest, path .. '.missing', {{ chunkSize = 0 }}) \
                 refuses('power of two', fs.authManifest, path, {{ chunkSize = 2^40 }}) \
                 refuses('nope', fs.authManifest, path, {{ nope = true }}) \
                 refuses('negative', fs.openAuthenticated, path, manifest, commitment, {{ heldBytes = -1 }}) \
                 refuses('over the limit', fs.openAuthenticated, path, manifest, commitment, {{ heldBytes = 2^40 }}) \
                 refuses('nope', fs.openAuthenticated, path, manifest, commitment, {{ nope = 1 }})"
            ))
            .unwrap();
    }

    #[test]
    fn a_missing_file_is_an_answer_for_a_publisher_too() {
        let scratch = Scratch::new("luau-pubmiss");
        runtime()
            .exec(&format!(
                "local m, message, kind = fs.authManifest({root} .. '/missing') \
                 assert(m == nil and kind == 'notFound' and message:find('authManifest'), message) \
                 m, message, kind = fs.authManifest({root}) \
                 assert(m == nil and kind == 'isADirectory', message)",
                root = scratch.lua()
            ))
            .unwrap();
    }

    #[test]
    fn authenticated_reads_are_refused_without_the_read_capability() {
        runtime_with(&[])
            .exec(
                "local ok, err = pcall(fs.authManifest, 'x') assert(not ok and tostring(err):find('filesystem.read'), err) \
                 ok, err = pcall(fs.openAuthenticated, 'x', '', '') assert(not ok and tostring(err):find('filesystem.read'), err)",
            )
            .unwrap();
    }

    /// The example on the Built-in Extensions page, verbatim but for the file's name.
    const DOCUMENTED_EXAMPLE: &str = r#"
local fs = require("@dream/fs")

-- Publishing: once, by whoever makes the object.
local manifest, commitment = assert(fs.authManifest("assets.pak"))
assert(fs.writeFile("assets.pak.manifest", manifest))
-- Store `commitment` somewhere the file's storage cannot write: a signed record, your program's
-- own constants, a catalog you already authenticated. This is the step that makes it all mean something.

-- Reading: the manifest can come from anywhere, the commitment cannot.
local reader, message, kind = fs.openAuthenticated(
    "assets.pak",
    assert(fs.readAt("assets.pak.manifest", 0, 4 * 1024 * 1024)),
    commitment
)
assert(reader, message)
local bytes = assert(reader:readAt(1048576, 4096))

-- Damage is an answer, like anything else the disk does.
assert(fs.writeFile("assets.pak", "x", { offset = 0 }))
local nothing, reason, failure = reader:readAt(0, 16)
assert(nothing == nil and failure == "integrityFailed", reason)
"#;

    #[test]
    fn the_documented_example_does_what_it_says() {
        let scratch = Scratch::new("luau-example");
        let path = scratch.0.join("assets.pak");
        std::fs::write(&path, object(2 << 20)).unwrap();
        let script = DOCUMENTED_EXAMPLE.replace("assets.pak", &path.display().to_string().replace('\\', "/"));
        runtime().exec(&script).unwrap();
    }

    #[test]
    fn plain_reads_are_untouched_by_the_feature() {
        let scratch = Scratch::new("luau-plain");
        let script = prologue(&scratch, &object(1000));
        runtime()
            .exec(&format!(
                "{script} \
                 assert(fs.writeFile(path, 'zz', {{ offset = 0 }})) \
                 assert(buffer.tostring(fs.readAt(path, 0, 2)) == 'zz', 'readAt never checks anything') \
                 assert(buffer.len(fs.readAt(path, 990, 100)) == 10, 'readAt still clamps') \
                 local reader = fs.open(path) \
                 assert(buffer.len(reader:readAt(990, 100)) == 10, 'Reader.readAt still clamps')"
            ))
            .unwrap();
    }
}
