//! Checksums and digests (feature `bytes-digests`): CRC-32, Adler-32, FNV-1a, xxHash, MD5,
//! SHA-1, SHA-256 and BLAKE3, one-shot over a `buffer | string` or through a [`Hasher`] that
//! takes the data in pieces.
//!
//! 32-bit results are numbers, 64-bit results are Luau integers carrying the bits, and a
//! digest is a lower-case hex string (`sha256`) or the raw bytes (`digest(data, "sha256")`).

use std::cell::RefCell;

use crate::convert::{Bits64, BytesView, Exact, NewBuffer};
use crate::error::{Error, Result};
use crate::extension::{ExtensionDescriptor, ModuleDecl, TagPolicy};
use crate::userdata::Owned;

use super::to_hex;

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

/// FNV-1a over 32 bits.
#[must_use]
pub fn fnv1a32(data: &[u8]) -> u32 {
    data.iter().fold(0x811C_9DC5u32, |hash, byte| (hash ^ u32::from(*byte)).wrapping_mul(0x0100_0193))
}

/// FNV-1a over 64 bits.
#[must_use]
pub fn fnv1a64(data: &[u8]) -> u64 {
    data.iter().fold(0xCBF2_9CE4_8422_2325u64, |hash, byte| (hash ^ u64::from(*byte)).wrapping_mul(0x0100_0000_01B3))
}

/// An incremental digest state. The variants differ in size by an order of magnitude (BLAKE3
/// keeps a chunk stack); the state lives in one Box in the `Hasher`, so the enum's own size is
/// paid once per hasher and is not worth a second allocation per variant.
#[allow(clippy::large_enum_variant)]
enum State {
    Crc32(crc32fast::Hasher),
    Adler32(adler2::Adler32),
    Fnv1a32(u32),
    Fnv1a64(u64),
    Xxh32(xxhash_rust::xxh32::Xxh32),
    Xxh64(xxhash_rust::xxh64::Xxh64),
    Xxh3(xxhash_rust::xxh3::Xxh3),
    Md5(md5::Md5),
    Sha1(sha1::Sha1),
    Sha256(sha2::Sha256),
    Blake3(blake3::Hasher),
}

impl State {
    fn new(algorithm: &str, what: &str) -> Result<State> {
        use md5::Digest as _;
        Ok(match algorithm {
            "crc32" => State::Crc32(crc32fast::Hasher::new()),
            "adler32" => State::Adler32(adler2::Adler32::new()),
            "fnv1a32" => State::Fnv1a32(0x811C_9DC5),
            "fnv1a64" => State::Fnv1a64(0xCBF2_9CE4_8422_2325),
            "xxh32" => State::Xxh32(xxhash_rust::xxh32::Xxh32::new(0)),
            "xxh64" => State::Xxh64(xxhash_rust::xxh64::Xxh64::new(0)),
            "xxh3" => State::Xxh3(xxhash_rust::xxh3::Xxh3::new()),
            "md5" => State::Md5(md5::Md5::new()),
            "sha1" => State::Sha1(sha1::Sha1::new()),
            "sha256" => State::Sha256(sha2::Sha256::new()),
            "blake3" => State::Blake3(blake3::Hasher::new()),
            other => {
                return Err(Error::runtime(format!(
                    "{what}: '{other}' is not one of crc32, adler32, fnv1a32, fnv1a64, xxh32, xxh64, xxh3, md5, sha1, sha256, blake3"
                )));
            }
        })
    }

    fn update(&mut self, data: &[u8]) {
        use md5::Digest as _;
        match self {
            State::Crc32(h) => h.update(data),
            State::Adler32(h) => h.write_slice(data),
            State::Fnv1a32(h) => {
                *h = data.iter().fold(*h, |hash, byte| (hash ^ u32::from(*byte)).wrapping_mul(0x0100_0193));
            }
            State::Fnv1a64(h) => {
                *h = data.iter().fold(*h, |hash, byte| (hash ^ u64::from(*byte)).wrapping_mul(0x0100_0000_01B3));
            }
            State::Xxh32(h) => h.update(data),
            State::Xxh64(h) => h.update(data),
            State::Xxh3(h) => h.update(data),
            State::Md5(h) => h.update(data),
            State::Sha1(h) => h.update(data),
            State::Sha256(h) => h.update(data),
            State::Blake3(h) => {
                h.update(data);
            }
        }
    }

    /// The digest bytes, big-endian for the integer checksums, leaving the state usable.
    fn finish(&self) -> Vec<u8> {
        use md5::Digest as _;
        match self {
            State::Crc32(h) => h.clone().finalize().to_be_bytes().to_vec(),
            State::Adler32(h) => h.checksum().to_be_bytes().to_vec(),
            State::Fnv1a32(h) => h.to_be_bytes().to_vec(),
            State::Fnv1a64(h) => h.to_be_bytes().to_vec(),
            State::Xxh32(h) => h.digest().to_be_bytes().to_vec(),
            State::Xxh64(h) => h.digest().to_be_bytes().to_vec(),
            State::Xxh3(h) => h.digest().to_be_bytes().to_vec(),
            State::Md5(h) => h.clone().finalize().to_vec(),
            State::Sha1(h) => h.clone().finalize().to_vec(),
            State::Sha256(h) => h.clone().finalize().to_vec(),
            State::Blake3(h) => h.finalize().as_bytes().to_vec(),
        }
    }

    /// The checksum as an integer, for the algorithms that are one.
    fn value(&self) -> Option<i64> {
        Some(match self {
            State::Crc32(h) => i64::from(h.clone().finalize()),
            State::Adler32(h) => i64::from(h.checksum()),
            State::Fnv1a32(h) => i64::from(*h),
            State::Fnv1a64(h) => *h as i64,
            State::Xxh32(h) => i64::from(h.digest()),
            State::Xxh64(h) => h.digest() as i64,
            State::Xxh3(h) => h.digest() as i64,
            _ => return None,
        })
    }

    fn reset(&mut self) {
        use md5::Digest as _;
        match self {
            State::Crc32(h) => h.reset(),
            State::Adler32(h) => *h = adler2::Adler32::new(),
            State::Fnv1a32(h) => *h = 0x811C_9DC5,
            State::Fnv1a64(h) => *h = 0xCBF2_9CE4_8422_2325,
            State::Xxh32(h) => h.reset(0),
            State::Xxh64(h) => h.reset(0),
            State::Xxh3(h) => h.reset(),
            State::Md5(h) => h.reset(),
            State::Sha1(h) => h.reset(),
            State::Sha256(h) => h.reset(),
            State::Blake3(h) => {
                h.reset();
            }
        }
    }
}

/// An incremental digest (`dream.bytes.Hasher`, from `bytes.hasher(algorithm)`): `update`
/// takes the data in any pieces, `finish` gives the hex digest, `finishBytes` the raw digest,
/// `value` the checksum as an integer for the algorithms that are one, and `reset` starts over.
pub struct Hasher {
    algorithm: String,
    // Boxed: the SIMD digest states are over-aligned for a Luau userdata payload.
    state: RefCell<Box<State>>,
}

// SAFETY: the payload holds no Lua references.
unsafe impl crate::userdata::Userdata for Hasher {
    const NAME: &'static str = "dream.bytes.Hasher";
}

fn one_shot(what: &'static str, algorithm: &'static str) -> impl Fn(BytesView<'_>) -> Result<Vec<u8>> + Clone {
    move |data: BytesView<'_>| {
        let mut state = State::new(algorithm, what)?;
        // SAFETY: the digest reads the view and calls nothing.
        state.update(unsafe { bytes(&data) });
        Ok(state.finish())
    }
}

pub(crate) fn describe_hasher(d: &mut ExtensionDescriptor) {
    let mut hasher = d.userdata::<Hasher>("dream.bytes.Hasher");
    hasher.tag(TagPolicy::Never).doc("An incremental digest.");
    hasher
        .method("update", |h: &Hasher, data: BytesView<'_>| {
            // SAFETY: the digest reads the view and calls nothing.
            h.state.borrow_mut().update(unsafe { bytes(&data) });
        })
        .signature("(self, data: buffer | string): ()");
    hasher.method("algorithm", |h: &Hasher| h.algorithm.clone()).signature("(self): string");
    hasher.method("finish", |h: &Hasher| to_hex(&h.state.borrow().finish())).signature("(self): string");
    hasher.method("finishBytes", |h: &Hasher| NewBuffer(h.state.borrow().finish())).signature("(self): buffer");
    hasher
        .method("value", |h: &Hasher| -> Result<Bits64> {
            h.state.borrow().value().map(|v| Bits64(v as u64)).ok_or_else(|| {
                Error::runtime(format!("bytes.Hasher:value: {} is a digest, not a checksum; use finish", h.algorithm))
            })
        })
        .signature("(self): integer");
    hasher.method("reset", |h: &Hasher| h.state.borrow_mut().reset()).signature("(self): ()");
}

pub(crate) fn describe(module: &mut ModuleDecl) {
    module
        .function("hasher", |algorithm: &str| -> Result<Owned<Hasher>> {
            Ok(Owned(Hasher {
                algorithm: algorithm.to_owned(),
                state: RefCell::new(Box::new(State::new(algorithm, "bytes.hasher")?)),
            }))
        })
        .signature("(algorithm: string) -> dream_bytes_Hasher")
        .doc("crc32, adler32, fnv1a32, fnv1a64, xxh32, xxh64, xxh3, md5, sha1, sha256 or blake3, fed in pieces.")
        .function("crc32", |data: BytesView<'_>, seed: Option<Exact<i64>>| -> Result<f64> {
            let seed = seed
                .map_or(Ok(0), |s| u32::try_from(s.0).map_err(|_| Error::runtime("bytes.crc32: seed is not a u32")))?;
            let mut hasher = crc32fast::Hasher::new_with_initial(seed);
            // SAFETY: the digest reads the view and calls nothing.
            hasher.update(unsafe { bytes(&data) });
            Ok(f64::from(hasher.finalize()))
        })
        .signature("(data: buffer | string, seed: number?) -> number")
        .doc("CRC-32 (IEEE, as zip and PNG use it); seed continues an earlier value.")
        .function("adler32", |data: BytesView<'_>| -> f64 {
            // SAFETY: as `crc32`.
            f64::from(adler2::adler32_slice(unsafe { bytes(&data) }))
        })
        .signature("(data: buffer | string) -> number")
        .function("fnv1a32", |data: BytesView<'_>| -> f64 {
            // SAFETY: as `crc32`.
            f64::from(fnv1a32(unsafe { bytes(&data) }))
        })
        .signature("(data: buffer | string) -> number")
        .function("fnv1a64", |data: BytesView<'_>| -> Bits64 {
            // SAFETY: as `crc32`.
            Bits64(fnv1a64(unsafe { bytes(&data) }))
        })
        .signature("(data: buffer | string) -> integer")
        .function("xxh32", |data: BytesView<'_>, seed: Option<Exact<i64>>| -> Result<f64> {
            let seed = seed
                .map_or(Ok(0), |s| u32::try_from(s.0).map_err(|_| Error::runtime("bytes.xxh32: seed is not a u32")))?;
            // SAFETY: as `crc32`.
            Ok(f64::from(xxhash_rust::xxh32::xxh32(unsafe { bytes(&data) }, seed)))
        })
        .signature("(data: buffer | string, seed: number?) -> number")
        .function("xxh64", |data: BytesView<'_>, seed: Option<Bits64>| -> Bits64 {
            // SAFETY: as `crc32`.
            Bits64(xxhash_rust::xxh64::xxh64(unsafe { bytes(&data) }, seed.map_or(0, |s| s.0)))
        })
        .signature("(data: buffer | string, seed: integer?) -> integer")
        .function("xxh3", |data: BytesView<'_>| -> Bits64 {
            // SAFETY: as `crc32`.
            Bits64(xxhash_rust::xxh3::xxh3_64(unsafe { bytes(&data) }))
        })
        .signature("(data: buffer | string) -> integer")
        .doc("xxHash3, 64 bits.");
    let hex = |what: &'static str, algorithm: &'static str| {
        let digest = one_shot(what, algorithm);
        move |data: BytesView<'_>| -> Result<String> { Ok(to_hex(&digest(data)?)) }
    };
    module
        .function("md5", hex("bytes.md5", "md5"))
        .signature("(data: buffer | string) -> string")
        .function("sha1", hex("bytes.sha1", "sha1"))
        .signature("(data: buffer | string) -> string")
        .function("sha256", hex("bytes.sha256", "sha256"))
        .signature("(data: buffer | string) -> string")
        .function("blake3", hex("bytes.blake3", "blake3"))
        .signature("(data: buffer | string) -> string")
        .function("digest", |data: BytesView<'_>, algorithm: &str| -> Result<NewBuffer> {
            let mut state = State::new(algorithm, "bytes.digest")?;
            // SAFETY: as `crc32`.
            state.update(unsafe { bytes(&data) });
            Ok(NewBuffer(state.finish()))
        })
        .signature("(data: buffer | string, algorithm: string) -> buffer")
        .doc("The raw digest bytes of any algorithm bytes.hasher accepts; checksums are big-endian.");
}
