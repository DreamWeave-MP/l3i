//! Authenticated ranged reads (feature `fs-authenticated`): a few bytes of a large immutable
//! object, proven to belong to it, without reading or hashing the rest.
//!
//! # The trust model
//!
//! The only thing this module trusts is a [`Commitment`] the caller got through a channel the
//! object's storage cannot write to: a signed release record, a compiled-in constant, a catalog
//! the caller already authenticated. Everything else, the object and its manifest alike, may be
//! controlled by an attacker. A digest computed from the bytes being checked proves nothing, so
//! no function here reads a commitment from a file, and [`build_manifest`]'s output cannot open a
//! reader: only [`VerifiedManifest::verify`] makes the [`VerifiedManifest`] one needs.
//!
//! ```text
//! caller's Commitment = SHA-256(manifest bytes)
//!        |
//!        v
//! manifest: format, version, algorithm, object length, chunk size, chunk count, chunk digests
//!        |
//!        v
//! chunk digest i = SHA-256(domain, i, length of chunk i, bytes of chunk i)
//! ```
//!
//! A successful read returns exactly the requested bytes of the object the commitment names.
//! That says nothing about freshness: an old revision stays valid against its old commitment, and
//! whether a commitment is current is the caller's question. It says nothing about durability
//! either. Chunks a read did not touch have not been inspected, and damage there surfaces when
//! something asks for them.
//!
//! # The manifest
//!
//! All integers are little-endian, every field has a fixed width, and exactly one encoding exists
//! for a given object and chunk size, so the commitment is the same on every platform:
//!
//! | Offset | Size | Field |
//! | ---: | ---: | --- |
//! | 0 | 8 | magic, `L3IAUTH\n` |
//! | 8 | 2 | format version, 1 |
//! | 10 | 1 | algorithm, 1 for SHA-256 |
//! | 11 | 1 | digest length, 32 |
//! | 12 | 4 | chunk size, a power of two from [`MIN_CHUNK_SIZE`] to [`MAX_CHUNK_SIZE`] |
//! | 16 | 8 | object length |
//! | 24 | 8 | chunk count, the object length divided by the chunk size, rounded up |
//! | 32 | 32 each | the chunk digests in order |
//!
//! Each chunk's digest also covers a domain string, its index and its length, so it cannot be
//! confused with a plain SHA-256 of the same bytes, or with the same bytes at another position.
//! The commitment is the plain SHA-256 of the manifest's bytes, which any tool can check. The two
//! cannot collide by construction: a manifest starts with the magic, a chunk's input with the
//! domain string.
//!
//! # Order of checks
//!
//! [`VerifiedManifest::verify`] hashes first and parses after, so nothing an attacker wrote in a
//! manifest is ever interpreted, and nothing is allocated for it, until it has matched the
//! commitment. Only the length is looked at before that, which bounds the hash.
//!
//! # Where bytes live while they are unverified
//!
//! A read copies from the source into memory nobody else can reach, hashes that copy, and only
//! then lets it be seen. The order matters: hashing the file's bytes and then copying them again,
//! or hashing a memory map in place, would let a concurrent writer change them after the check.
//! For the same reason [`super::Reader`] is not a source here. Its map can change underneath a
//! hash, and a file truncated underneath it can kill the process instead of failing the read.
//!
//! Whole chunks are read straight into the caller's target and hashed there, because the `&mut`
//! makes that memory private until the call returns. On any failure [`AuthenticatedReader::read_into`]
//! zeroes the whole target first, so neither unverified bytes nor a verified prefix survive.

mod script;

use std::fmt;
use std::fs::File;
use std::io;
use std::sync::Arc;

use sha2::{Digest as _, Sha256};

pub(crate) use script::{auth_manifest, describe_authenticated_reader, open_authenticated};

/// The manifest format this build reads and writes.
pub const FORMAT_VERSION: u16 = 1;
/// The bytes in a digest or a commitment.
pub const DIGEST_LEN: usize = 32;
/// The smallest chunk size a manifest may declare.
pub const MIN_CHUNK_SIZE: u32 = 512;
/// The largest chunk size a manifest may declare: the most a read of one byte can cost.
pub const MAX_CHUNK_SIZE: u32 = 16 << 20;
/// The chunk size [`build_manifest`]'s callers get when they have no reason to choose.
pub const DEFAULT_CHUNK_SIZE: u32 = 64 << 10;
/// The most chunks a manifest may declare, which bounds the hash a manifest can ask for.
pub const MAX_CHUNKS: u64 = 1 << 24;
/// The longest manifest [`VerifiedManifest::verify`] looks at.
pub const MAX_MANIFEST_LEN: usize = HEADER_LEN + DIGEST_LEN * (MAX_CHUNKS as usize);

const MAGIC: [u8; 8] = *b"L3IAUTH\n";
const ALGORITHM_SHA256: u8 = 1;
const HEADER_LEN: usize = 32;
const CHUNK_DOMAIN: &[u8] = b"l3i.fs.authenticated.chunk.v1\0";

/// Why an authenticated operation refused. No variant carries data from an unverified read.
#[derive(Debug)]
#[non_exhaustive]
pub enum AuthError {
    /// The manifest's bytes do not hash to the trusted commitment.
    ManifestMismatch,
    /// The manifest is longer than [`MAX_MANIFEST_LEN`]; it was not hashed.
    ManifestTooLong { length: usize },
    /// The manifest matches its commitment but is not one this format can mean.
    MalformedManifest(&'static str),
    /// The manifest matches its commitment and declares a version this build does not know.
    UnsupportedVersion(u16),
    /// The manifest matches its commitment and declares an algorithm this build does not know.
    UnsupportedAlgorithm(u8),
    /// A chunk size that is not a power of two from [`MIN_CHUNK_SIZE`] to [`MAX_CHUNK_SIZE`].
    InvalidChunkSize(u64),
    /// An object that would need more than [`MAX_CHUNKS`] chunks at this chunk size.
    ObjectTooLarge { length: u64, chunk_size: u32 },
    /// A commitment that is not 64 hex digits.
    InvalidCommitment,
    /// A range outside the committed object. Nothing was read.
    OutOfRange { offset: u64, length: u64, object_length: u64 },
    /// The source's size differs from the committed length.
    LengthMismatch { expected: u64, actual: u64 },
    /// The source ended inside a chunk: it was truncated, or never was this object.
    ShortChunk { index: u64, expected: u64, actual: u64 },
    /// A chunk's bytes do not match its committed digest.
    ChunkMismatch { index: u64 },
    /// The allocator refused a buffer for the read.
    OutOfMemory { length: u64 },
    /// The source failed.
    Io(io::Error),
}

impl fmt::Display for AuthError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AuthError::ManifestMismatch => f.write_str("the manifest does not match the trusted commitment"),
            AuthError::ManifestTooLong { length } => {
                write!(f, "a manifest of {length} bytes is longer than the limit of {MAX_MANIFEST_LEN}")
            }
            AuthError::MalformedManifest(why) => write!(f, "malformed manifest: {why}"),
            AuthError::UnsupportedVersion(version) => write!(f, "unsupported manifest version {version}"),
            AuthError::UnsupportedAlgorithm(algorithm) => write!(f, "unsupported manifest algorithm {algorithm}"),
            AuthError::InvalidChunkSize(size) => {
                write!(f, "chunk size {size} is not a power of two from {MIN_CHUNK_SIZE} to {MAX_CHUNK_SIZE}")
            }
            AuthError::ObjectTooLarge { length, chunk_size } => {
                write!(f, "an object of {length} bytes needs more than {MAX_CHUNKS} chunks of {chunk_size} bytes")
            }
            AuthError::InvalidCommitment => f.write_str("a commitment is 64 hex digits"),
            AuthError::OutOfRange { offset, length, object_length } => {
                write!(f, "{length} bytes at offset {offset} are outside the object (size {object_length})")
            }
            AuthError::LengthMismatch { expected, actual } => {
                write!(f, "the file is {actual} bytes but the manifest commits to {expected}")
            }
            AuthError::ShortChunk { index, expected, actual } => {
                write!(f, "chunk {index} ended after {actual} of its {expected} bytes")
            }
            AuthError::ChunkMismatch { index } => write!(f, "chunk {index} does not match its committed digest"),
            AuthError::OutOfMemory { length } => write!(f, "no memory for a read of {length} bytes"),
            AuthError::Io(error) => error.fmt(f),
        }
    }
}

impl std::error::Error for AuthError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            AuthError::Io(error) => Some(error),
            _ => None,
        }
    }
}

/// The trusted root: the SHA-256 of a manifest's bytes.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct Commitment([u8; DIGEST_LEN]);

impl Commitment {
    /// A commitment from its raw bytes.
    #[must_use]
    pub const fn from_bytes(bytes: [u8; DIGEST_LEN]) -> Commitment {
        Commitment(bytes)
    }

    /// A commitment from 64 hex digits of either case.
    ///
    /// # Errors
    ///
    /// [`AuthError::InvalidCommitment`] for anything else.
    pub fn from_hex(text: &[u8]) -> Result<Commitment, AuthError> {
        fn digit(byte: u8) -> Result<u8, AuthError> {
            match byte {
                b'0'..=b'9' => Ok(byte - b'0'),
                b'a'..=b'f' => Ok(byte - b'a' + 10),
                b'A'..=b'F' => Ok(byte - b'A' + 10),
                _ => Err(AuthError::InvalidCommitment),
            }
        }
        if text.len() != DIGEST_LEN * 2 {
            return Err(AuthError::InvalidCommitment);
        }
        let mut bytes = [0; DIGEST_LEN];
        for (byte, [high, low]) in bytes.iter_mut().zip(text.as_chunks::<2>().0) {
            *byte = digit(*high)? << 4 | digit(*low)?;
        }
        Ok(Commitment(bytes))
    }

    /// The commitment that names exactly `manifest`.
    fn of(manifest: &[u8]) -> Commitment {
        Commitment(Sha256::digest(manifest).into())
    }

    /// The raw bytes.
    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; DIGEST_LEN] {
        &self.0
    }
}

/// Lower-case hex, the spelling `bytes.sha256` and `sha256sum` print.
impl fmt::Display for Commitment {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.iter().try_for_each(|byte| write!(f, "{byte:02x}"))
    }
}

impl fmt::Debug for Commitment {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Commitment({self})")
    }
}

/// Where a reader's bytes come from: positional reads, which never expose a mapping.
pub trait RangeSource {
    /// Copies bytes from `offset` into `dst` and returns the count; zero means the end. A short
    /// count is allowed and is retried, as is [`io::ErrorKind::Interrupted`].
    ///
    /// # Errors
    ///
    /// Whatever the source's read fails with.
    fn read_at(&self, offset: u64, dst: &mut [u8]) -> io::Result<usize>;

    /// The source's current size.
    ///
    /// # Errors
    ///
    /// Whatever asking the source for its size fails with.
    fn len(&self) -> io::Result<u64>;

    /// Whether the source is currently empty.
    ///
    /// # Errors
    ///
    /// As [`RangeSource::len`].
    fn is_empty(&self) -> io::Result<bool> {
        Ok(self.len()? == 0)
    }
}

impl RangeSource for File {
    fn read_at(&self, offset: u64, dst: &mut [u8]) -> io::Result<usize> {
        super::io::read_file_at(self, offset, dst)
    }

    fn len(&self) -> io::Result<u64> {
        self.metadata().map(|metadata| metadata.len())
    }
}

impl RangeSource for [u8] {
    fn read_at(&self, offset: u64, dst: &mut [u8]) -> io::Result<usize> {
        let start = usize::try_from(offset).unwrap_or(usize::MAX).min(<[u8]>::len(self));
        let count = dst.len().min(<[u8]>::len(self) - start);
        dst[..count].copy_from_slice(&self[start..start + count]);
        Ok(count)
    }

    fn len(&self) -> io::Result<u64> {
        Ok(<[u8]>::len(self) as u64)
    }
}

impl<S: RangeSource + ?Sized> RangeSource for &S {
    fn read_at(&self, offset: u64, dst: &mut [u8]) -> io::Result<usize> {
        (**self).read_at(offset, dst)
    }

    fn len(&self) -> io::Result<u64> {
        (**self).len()
    }
}

/// Fills as much of `dst` as the source has from `offset`, and returns the count.
fn fill_from<S: RangeSource + ?Sized>(source: &S, offset: u64, dst: &mut [u8]) -> io::Result<usize> {
    let mut filled = 0;
    while filled < dst.len() {
        match source.read_at(offset + filled as u64, &mut dst[filled..]) {
            Ok(0) => break,
            // A source claiming more than it was given has no meaningful bytes to offer.
            Ok(count) if count > dst.len() - filled => {
                return Err(io::Error::new(io::ErrorKind::InvalidData, "the source claimed an impossible read"));
            }
            Ok(count) => filled += count,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }
    Ok(filled)
}

/// The digest of chunk `index`, whose bytes are all of `bytes`.
fn chunk_digest(index: u64, bytes: &[u8]) -> [u8; DIGEST_LEN] {
    let mut hasher = Sha256::new();
    hasher.update(CHUNK_DOMAIN);
    hasher.update(index.to_le_bytes());
    hasher.update((bytes.len() as u64).to_le_bytes());
    hasher.update(bytes);
    hasher.finalize().into()
}

fn valid_chunk_size(size: u64) -> bool {
    size.is_power_of_two() && (u64::from(MIN_CHUNK_SIZE)..=u64::from(MAX_CHUNK_SIZE)).contains(&size)
}

/// What publishing an object produces: the manifest to store anywhere, and the commitment to
/// deliver through a trusted channel. It cannot open a reader; only verification makes the
/// [`VerifiedManifest`] one needs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PublishedManifest {
    /// The manifest's canonical bytes.
    pub bytes: Vec<u8>,
    /// The SHA-256 of [`PublishedManifest::bytes`].
    pub commitment: Commitment,
}

/// Reads all of `source` and builds its manifest. The source must not change meanwhile: a change
/// to its size fails the build, but a change in place is simply what gets committed to.
///
/// # Errors
///
/// [`AuthError::InvalidChunkSize`] and [`AuthError::ObjectTooLarge`] for what cannot be encoded,
/// [`AuthError::ShortChunk`] and [`AuthError::LengthMismatch`] for a source whose size changed,
/// [`AuthError::Io`] for a read that failed, and [`AuthError::OutOfMemory`].
pub fn build_manifest<S: RangeSource + ?Sized>(source: &S, chunk_size: u32) -> Result<PublishedManifest, AuthError> {
    if !valid_chunk_size(u64::from(chunk_size)) {
        return Err(AuthError::InvalidChunkSize(u64::from(chunk_size)));
    }
    let length = source.len().map_err(AuthError::Io)?;
    let count = length.div_ceil(u64::from(chunk_size));
    if count > MAX_CHUNKS {
        return Err(AuthError::ObjectTooLarge { length, chunk_size });
    }
    // Lossless: `count` is at most MAX_CHUNKS and MAX_MANIFEST_LEN fits a usize.
    let manifest_len = HEADER_LEN + DIGEST_LEN * count as usize;
    let mut bytes = Vec::new();
    bytes.try_reserve_exact(manifest_len).map_err(|_| AuthError::OutOfMemory { length: manifest_len as u64 })?;
    bytes.extend_from_slice(&MAGIC);
    bytes.extend_from_slice(&FORMAT_VERSION.to_le_bytes());
    bytes.push(ALGORITHM_SHA256);
    bytes.push(DIGEST_LEN as u8);
    bytes.extend_from_slice(&chunk_size.to_le_bytes());
    bytes.extend_from_slice(&length.to_le_bytes());
    bytes.extend_from_slice(&count.to_le_bytes());

    let mut chunk = zeroed(u64::from(chunk_size.min(u32::try_from(length).unwrap_or(u32::MAX))))?;
    for index in 0..count {
        let start = index * u64::from(chunk_size);
        let expected = (length - start).min(u64::from(chunk_size));
        // Lossless: `expected` is at most the length of `chunk`.
        let slot = &mut chunk[..expected as usize];
        let actual = fill_from(source, start, slot).map_err(AuthError::Io)?;
        if actual != slot.len() {
            return Err(AuthError::ShortChunk { index, expected, actual: actual as u64 });
        }
        bytes.extend_from_slice(&chunk_digest(index, slot));
    }

    // Growth during the build would otherwise pass unnoticed: every chunk read fine.
    let actual = source.len().map_err(AuthError::Io)?;
    let mut probe = [0; 1];
    let beyond = fill_from(source, length, &mut probe).map_err(AuthError::Io)?;
    if actual != length || beyond != 0 {
        return Err(AuthError::LengthMismatch { expected: length, actual: actual.max(length + beyond as u64) });
    }
    let commitment = Commitment::of(&bytes);
    Ok(PublishedManifest { bytes, commitment })
}

/// A zeroed buffer, without aborting the process when the allocator refuses it.
fn zeroed(length: u64) -> Result<Vec<u8>, AuthError> {
    let size = usize::try_from(length).map_err(|_| AuthError::OutOfMemory { length })?;
    let mut bytes = Vec::new();
    bytes.try_reserve_exact(size).map_err(|_| AuthError::OutOfMemory { length })?;
    bytes.resize(size, 0);
    Ok(bytes)
}

/// A manifest that matched a trusted commitment. It exists only through
/// [`VerifiedManifest::verify`], its fields never change, and one can be shared between readers
/// behind an [`Arc`], so opening another reader need not hash it again.
#[derive(Clone, PartialEq, Eq)]
pub struct VerifiedManifest {
    bytes: Box<[u8]>,
    commitment: Commitment,
    object_length: u64,
    chunk_size: u32,
    chunk_count: u64,
}

impl fmt::Debug for VerifiedManifest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("VerifiedManifest")
            .field("commitment", &self.commitment)
            .field("object_length", &self.object_length)
            .field("chunk_size", &self.chunk_size)
            .field("chunk_count", &self.chunk_count)
            .finish_non_exhaustive()
    }
}

impl VerifiedManifest {
    /// Authenticates `bytes` against `trusted`, then parses them. Nothing is parsed or allocated
    /// before they match.
    ///
    /// # Errors
    ///
    /// [`AuthError::ManifestTooLong`] before any hashing; [`AuthError::ManifestMismatch`] when the
    /// bytes do not hash to `trusted`; [`AuthError::MalformedManifest`],
    /// [`AuthError::UnsupportedVersion`] or [`AuthError::UnsupportedAlgorithm`] when they do and
    /// are still not a manifest this build can read; and [`AuthError::OutOfMemory`].
    pub fn verify(bytes: &[u8], trusted: &Commitment) -> Result<VerifiedManifest, AuthError> {
        if bytes.len() > MAX_MANIFEST_LEN {
            return Err(AuthError::ManifestTooLong { length: bytes.len() });
        }
        if Commitment::of(bytes) != *trusted {
            return Err(AuthError::ManifestMismatch);
        }
        let header: &[u8; HEADER_LEN] =
            bytes.first_chunk().ok_or(AuthError::MalformedManifest("shorter than the header"))?;
        let field = |start: usize| -> [u8; 8] {
            let mut word = [0; 8];
            word.copy_from_slice(&header[start..start + 8]);
            word
        };
        if field(0) != MAGIC {
            return Err(AuthError::MalformedManifest("wrong magic"));
        }
        let version = u16::from_le_bytes([header[8], header[9]]);
        if version != FORMAT_VERSION {
            return Err(AuthError::UnsupportedVersion(version));
        }
        if header[10] != ALGORITHM_SHA256 {
            return Err(AuthError::UnsupportedAlgorithm(header[10]));
        }
        if usize::from(header[11]) != DIGEST_LEN {
            return Err(AuthError::MalformedManifest("wrong digest length"));
        }
        let chunk_size = u32::from_le_bytes([header[12], header[13], header[14], header[15]]);
        if !valid_chunk_size(u64::from(chunk_size)) {
            return Err(AuthError::MalformedManifest("invalid chunk size"));
        }
        let object_length = u64::from_le_bytes(field(16));
        let chunk_count = u64::from_le_bytes(field(24));
        if chunk_count > MAX_CHUNKS {
            return Err(AuthError::MalformedManifest("too many chunks"));
        }
        if chunk_count != object_length.div_ceil(u64::from(chunk_size)) {
            return Err(AuthError::MalformedManifest("the chunk count does not fit the object length"));
        }
        // Lossless: `chunk_count` is at most MAX_CHUNKS and MAX_MANIFEST_LEN fits a usize.
        if bytes.len() != HEADER_LEN + DIGEST_LEN * chunk_count as usize {
            return Err(AuthError::MalformedManifest("the length does not fit the chunk count"));
        }
        let mut owned = Vec::new();
        owned.try_reserve_exact(bytes.len()).map_err(|_| AuthError::OutOfMemory { length: bytes.len() as u64 })?;
        owned.extend_from_slice(bytes);
        Ok(VerifiedManifest {
            bytes: owned.into_boxed_slice(),
            commitment: *trusted,
            object_length,
            chunk_size,
            chunk_count,
        })
    }

    /// The manifest's canonical bytes.
    #[must_use]
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// The commitment these bytes matched.
    #[must_use]
    pub const fn commitment(&self) -> &Commitment {
        &self.commitment
    }

    /// The committed object's length in bytes.
    #[must_use]
    pub const fn object_length(&self) -> u64 {
        self.object_length
    }

    /// The chunk size in bytes; every chunk but the last has exactly this length.
    #[must_use]
    pub const fn chunk_size(&self) -> u32 {
        self.chunk_size
    }

    /// The number of chunks.
    #[must_use]
    pub const fn chunk_count(&self) -> u64 {
        self.chunk_count
    }

    /// Whether `length` bytes at `offset` are inside the object. A read may end exactly at the
    /// end, and an empty read may be anywhere up to it.
    ///
    /// # Errors
    ///
    /// [`AuthError::OutOfRange`] otherwise, including when the end would overflow.
    pub const fn check_range(&self, offset: u64, length: u64) -> Result<(), AuthError> {
        match offset.checked_add(length) {
            Some(end) if end <= self.object_length => Ok(()),
            _ => Err(AuthError::OutOfRange { offset, length, object_length: self.object_length }),
        }
    }

    /// Whether `bytes` are chunk `index`, in full and at that position.
    fn check_chunk(&self, index: u64, bytes: &[u8]) -> Result<(), AuthError> {
        // Lossless: an index below the chunk count, bounded by MAX_CHUNKS.
        let start = HEADER_LEN + DIGEST_LEN * index as usize;
        if chunk_digest(index, bytes) == self.bytes[start..start + DIGEST_LEN] {
            Ok(())
        } else {
            Err(AuthError::ChunkMismatch { index })
        }
    }

    /// The byte range `[start, end)` of chunk `index` in the object.
    const fn chunk_bounds(&self, index: u64) -> (u64, u64) {
        let start = index * self.chunk_size as u64;
        let end = start + self.chunk_size as u64;
        (start, if end < self.object_length { end } else { self.object_length })
    }
}

/// Random access to an object through a verified manifest. Reads may repeat in any order and on
/// any part of the object; each pays only for the chunks it covers.
#[derive(Debug)]
pub struct AuthenticatedReader<S> {
    source: S,
    manifest: Arc<VerifiedManifest>,
    /// One chunk for reads that cover only part of one. Private, and never shown to anyone
    /// before its digest has matched.
    scratch: Vec<u8>,
}

impl<S: RangeSource> AuthenticatedReader<S> {
    /// A reader of `source`, which must currently be exactly the committed length.
    ///
    /// # Errors
    ///
    /// [`AuthError::LengthMismatch`], or [`AuthError::Io`] when the source cannot say.
    pub fn open(source: S, manifest: impl Into<Arc<VerifiedManifest>>) -> Result<AuthenticatedReader<S>, AuthError> {
        let manifest = manifest.into();
        let actual = source.len().map_err(AuthError::Io)?;
        if actual != manifest.object_length {
            return Err(AuthError::LengthMismatch { expected: manifest.object_length, actual });
        }
        Ok(AuthenticatedReader { source, manifest, scratch: Vec::new() })
    }

    /// The manifest this reader checks against.
    #[must_use]
    pub const fn manifest(&self) -> &Arc<VerifiedManifest> {
        &self.manifest
    }

    /// Exactly `length` bytes at `offset`, all verified, in a new buffer.
    ///
    /// # Errors
    ///
    /// As [`AuthenticatedReader::read_into`], and [`AuthError::OutOfMemory`].
    pub fn read_at(&mut self, offset: u64, length: usize) -> Result<Vec<u8>, AuthError> {
        // Checked first: a length from a caller is not an allocation until it fits the object.
        self.manifest.check_range(offset, length as u64)?;
        let mut bytes = zeroed(length as u64)?;
        self.read_into(offset, &mut bytes)?;
        Ok(bytes)
    }

    /// Fills `target` with exactly the bytes at `offset`, all verified. On failure the whole of
    /// `target` is zeroed, so it holds neither unverified bytes nor a verified prefix.
    ///
    /// # Errors
    ///
    /// [`AuthError::OutOfRange`] when the range leaves the object, [`AuthError::ChunkMismatch`]
    /// when bytes differ from the commitment, [`AuthError::ShortChunk`] when the source has
    /// shrunk, and [`AuthError::Io`] when it fails. An empty read inside the object succeeds
    /// without touching the source.
    pub fn read_into(&mut self, offset: u64, target: &mut [u8]) -> Result<(), AuthError> {
        let result = self.fill(offset, target);
        if result.is_err() {
            target.fill(0);
        }
        result
    }

    fn fill(&mut self, offset: u64, target: &mut [u8]) -> Result<(), AuthError> {
        self.manifest.check_range(offset, target.len() as u64)?;
        if target.is_empty() {
            return Ok(());
        }
        let chunk_size = u64::from(self.manifest.chunk_size);
        let end = offset + target.len() as u64;
        // The chunks entirely inside the range: read as one run, then checked one by one.
        let first_whole = offset.div_ceil(chunk_size);
        let last_whole = if end == self.manifest.object_length { self.manifest.chunk_count } else { end / chunk_size };
        let (first_covered, last_covered) = (offset / chunk_size, (end - 1) / chunk_size);

        if first_whole >= last_whole {
            // No whole chunk: at most two partial ones.
            let mut written = 0;
            for index in first_covered..=last_covered {
                written += self.copy_part(index, offset + written as u64, &mut target[written..])?;
            }
            return Ok(());
        }
        let mut written = 0;
        if first_covered < first_whole {
            written = self.copy_part(first_covered, offset, target)?;
        }
        let run_start = first_whole * chunk_size;
        let (_, run_end) = self.manifest.chunk_bounds(last_whole - 1);
        // Lossless: inside `target`, whose length is a usize.
        let run_len = (run_end - run_start) as usize;
        self.read_run(first_whole, last_whole, run_start, &mut target[written..written + run_len])?;
        written += run_len;
        if written < target.len() {
            self.copy_part(last_covered, offset + written as u64, &mut target[written..])?;
        }
        Ok(())
    }

    /// Reads whole chunks `[first, last)` straight into `run`, private until this call returns,
    /// and checks each one.
    fn read_run(&self, first: u64, last: u64, start: u64, run: &mut [u8]) -> Result<(), AuthError> {
        let count = fill_from(&self.source, start, run).map_err(AuthError::Io)?;
        if count != run.len() {
            let index = start.saturating_add(count as u64) / u64::from(self.manifest.chunk_size);
            let (chunk_start, chunk_end) = self.manifest.chunk_bounds(index);
            return Err(AuthError::ShortChunk {
                index,
                expected: chunk_end - chunk_start,
                actual: (start + count as u64) - chunk_start,
            });
        }
        let mut position = 0;
        for index in first..last {
            let (chunk_start, chunk_end) = self.manifest.chunk_bounds(index);
            // Lossless: a chunk's length is at most MAX_CHUNK_SIZE.
            let next = position + (chunk_end - chunk_start) as usize;
            self.manifest.check_chunk(index, &run[position..next])?;
            position = next;
        }
        Ok(())
    }

    /// Verifies chunk `index` in the scratch buffer, then copies what `target` wants of it from
    /// `offset`; returns the count copied.
    fn copy_part(&mut self, index: u64, offset: u64, target: &mut [u8]) -> Result<usize, AuthError> {
        let (start, end) = self.manifest.chunk_bounds(index);
        // Lossless: a chunk's length is at most MAX_CHUNK_SIZE.
        let length = (end - start) as usize;
        if self.scratch.len() < length {
            self.scratch = zeroed(u64::from(self.manifest.chunk_size).min(self.manifest.object_length))?;
        }
        let chunk = &mut self.scratch[..length];
        let actual = fill_from(&self.source, start, chunk).map_err(AuthError::Io)?;
        if actual != length {
            return Err(AuthError::ShortChunk { index, expected: length as u64, actual: actual as u64 });
        }
        self.manifest.check_chunk(index, chunk)?;
        // Lossless: `offset` is inside this chunk.
        let within = (offset - start) as usize;
        let count = target.len().min(length - within);
        target[..count].copy_from_slice(&chunk[within..within + count]);
        Ok(count)
    }
}
