//! Secure randomness and keyed digests (feature `bytes-crypto`), through ring, the provider
//! `@dream/tls` already links: bytes from the operating system's CSPRNG, and HMAC with a
//! verification that compares in constant time.
//!
//! These are the primitives protocols need around a transport (a WebSocket mask, a nonce, a
//! webhook signature), not a cryptography library: no ciphers, no key management, no choice of
//! random source.

use ring::hmac;
use ring::rand::{SecureRandom as _, SystemRandom};

use crate::convert::{BytesView, Exact, NewBuffer};
use crate::error::{Error, Result};
use crate::extension::ModuleDecl;

/// The most bytes one `bytes.random` call returns.
pub const MAX_RANDOM_BYTES: usize = 65_536;

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

/// `count` bytes from the operating system's CSPRNG.
///
/// # Errors
///
/// A count past [`MAX_RANDOM_BYTES`], or an operating system that has no randomness to give.
pub fn random(count: usize) -> Result<Vec<u8>> {
    if count > MAX_RANDOM_BYTES {
        return Err(Error::runtime(format!(
            "bytes.random: {count} bytes is more than the {MAX_RANDOM_BYTES} one call returns"
        )));
    }
    let mut data = vec![0; count];
    SystemRandom::new()
        .fill(&mut data)
        .map_err(|_| Error::runtime("bytes.random: the operating system's random source failed"))?;
    Ok(data)
}

fn algorithm(name: &str, what: &str) -> Result<hmac::Algorithm> {
    match name {
        "sha1" => Ok(hmac::HMAC_SHA1_FOR_LEGACY_USE_ONLY),
        "sha256" => Ok(hmac::HMAC_SHA256),
        "sha384" => Ok(hmac::HMAC_SHA384),
        "sha512" => Ok(hmac::HMAC_SHA512),
        other => Err(Error::runtime(format!("{what}: '{other}' is not one of sha1, sha256, sha384, sha512"))),
    }
}

/// The HMAC of `data` under `key` with `name`'s digest.
///
/// # Errors
///
/// An algorithm other than sha1, sha256, sha384 or sha512.
pub fn sign(data: &[u8], key: &[u8], name: &str) -> Result<Vec<u8>> {
    let key = hmac::Key::new(algorithm(name, "bytes.hmac")?, key);
    Ok(hmac::sign(&key, data).as_ref().to_vec())
}

/// Whether `tag` is the HMAC of `data` under `key`, compared in constant time.
///
/// # Errors
///
/// As [`sign`].
pub fn verify(data: &[u8], key: &[u8], name: &str, tag: &[u8]) -> Result<bool> {
    let key = hmac::Key::new(algorithm(name, "bytes.hmacVerify")?, key);
    Ok(hmac::verify(&key, data, tag).is_ok())
}

pub(crate) fn describe(module: &mut ModuleDecl) {
    module
        .function("random", |count: Exact<i64>| -> Result<NewBuffer> {
            let count = usize::try_from(count.0)
                .map_err(|_| Error::runtime(format!("bytes.random: {} is not a count of bytes", count.0)))?;
            random(count).map(NewBuffer)
        })
        .signature("(count: number) -> buffer")
        .doc("count bytes (at most 65536) from the operating system's CSPRNG, for keys, nonces and masks.")
        .function("hmac", |data: BytesView<'_>, key: BytesView<'_>, name: &str| -> Result<NewBuffer> {
            // SAFETY: HMAC reads both views and calls nothing.
            sign(unsafe { bytes(&data) }, unsafe { bytes(&key) }, name).map(NewBuffer)
        })
        .signature("(data: buffer | string, key: buffer | string, algorithm: string) -> buffer")
        .doc("The raw HMAC of data under key; algorithm is sha1, sha256, sha384 or sha512.")
        .function(
            "hmacVerify",
            |data: BytesView<'_>, key: BytesView<'_>, name: &str, tag: BytesView<'_>| -> Result<bool> {
                // SAFETY: as `hmac`.
                verify(unsafe { bytes(&data) }, unsafe { bytes(&key) }, name, unsafe { bytes(&tag) })
            },
        )
        .signature("(data: buffer | string, key: buffer | string, algorithm: string, tag: buffer | string) -> boolean")
        .doc("Whether tag is the raw HMAC of data under key, compared in constant time.");
}
