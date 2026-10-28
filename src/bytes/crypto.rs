//! Secure randomness, keyed digests and RSA signatures (feature `bytes-crypto`), through ring, the
//! provider `@dream/tls` already links: bytes from the operating system's CSPRNG, HMAC with a
//! verification that compares in constant time, and PKCS#1 v1.5 RSA signing.
//!
//! These are the primitives protocols need around a transport (a WebSocket mask, a nonce, a
//! webhook signature, a signed token), not a cryptography library: no ciphers, no key generation,
//! no key storage, no choice of random source.

use ring::hmac;
use ring::rand::{SecureRandom as _, SystemRandom};
use ring::signature::{self, RsaKeyPair};
use zeroize::Zeroizing;

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

/// The DER bytes of `key`: DER as given, or the base64 body of a PEM block, labelled `PRIVATE KEY`
/// (PKCS#8) or `RSA PRIVATE KEY` (PKCS#1). Errors never quote the key.
fn private_key_der(key: &[u8]) -> Result<Zeroizing<Vec<u8>>> {
    const WHAT: &str = "bytes.rsaSign";
    let trimmed = key.trim_ascii_start();
    if !trimmed.starts_with(b"-----BEGIN ") {
        return Ok(Zeroizing::new(key.to_vec()));
    }

    let text =
        std::str::from_utf8(trimmed).map_err(|_| Error::runtime(format!("{WHAT}: the PEM text is not UTF-8")))?;
    let label_end = text["-----BEGIN ".len()..]
        .find("-----")
        .ok_or_else(|| Error::runtime(format!("{WHAT}: the PEM text has no BEGIN line")))?;
    let label = &text["-----BEGIN ".len().."-----BEGIN ".len() + label_end];
    if label != "PRIVATE KEY" && label != "RSA PRIVATE KEY" {
        return Err(Error::runtime(format!(
            "{WHAT}: a PEM '{label}' block is not an unencrypted RSA private key; expected PRIVATE KEY or RSA PRIVATE KEY"
        )));
    }

    let body_start = "-----BEGIN ".len() + label_end + "-----".len();
    let end = text
        .find(&format!("-----END {label}-----"))
        .ok_or_else(|| Error::runtime(format!("{WHAT}: the PEM text has no END {label} line")))?;
    let body =
        Zeroizing::new(text[body_start..end].bytes().filter(|byte| !byte.is_ascii_whitespace()).collect::<Vec<u8>>());
    super::decode_base64(&body, WHAT)
        .map(Zeroizing::new)
        .map_err(|_| Error::runtime(format!("{WHAT}: the PEM body is not base64")))
}

/// The PKCS#1 v1.5 RSA signature of `data` under `key` with `name`'s digest: RS256, RS384 or
/// RS512 as JSON Web Tokens name them.
///
/// # Errors
///
/// A digest other than sha256, sha384 or sha512, or a key that isn't an RSA private key of at
/// least 2048 bits as PKCS#8 or PKCS#1, in DER or PEM.
pub fn sign_rsa(data: &[u8], key: &[u8], name: &str) -> Result<Vec<u8>> {
    let padding: &'static dyn signature::RsaEncoding = match name {
        "sha256" => &signature::RSA_PKCS1_SHA256,
        "sha384" => &signature::RSA_PKCS1_SHA384,
        "sha512" => &signature::RSA_PKCS1_SHA512,
        other => return Err(Error::runtime(format!("bytes.rsaSign: '{other}' is not one of sha256, sha384, sha512"))),
    };
    let der = private_key_der(key)?;
    let pair = RsaKeyPair::from_pkcs8(&der).or_else(|_| RsaKeyPair::from_der(&der)).map_err(|rejected| {
        Error::runtime(format!(
            "bytes.rsaSign: the key is not an RSA private key ring accepts as PKCS#8 or PKCS#1 ({rejected})"
        ))
    })?;
    let mut signed = vec![0; pair.public().modulus_len()];
    pair.sign(padding, &SystemRandom::new(), data, &mut signed)
        .map_err(|_| Error::runtime("bytes.rsaSign: signing failed"))?;
    Ok(signed)
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
        .doc("Whether tag is the raw HMAC of data under key, compared in constant time.")
        .function("rsaSign", |data: BytesView<'_>, key: BytesView<'_>, name: &str| -> Result<NewBuffer> {
            // SAFETY: signing reads both views and calls nothing.
            sign_rsa(unsafe { bytes(&data) }, unsafe { bytes(&key) }, name).map(NewBuffer)
        })
        .signature("(data: buffer | string, privateKey: buffer | string, algorithm: string) -> buffer")
        .doc("The raw PKCS#1 v1.5 RSA signature of data (RS256, RS384 or RS512 for algorithm sha256, sha384 or sha512) under an RSA private key of at least 2048 bits, PKCS#8 or PKCS#1, PEM or DER.");
}
