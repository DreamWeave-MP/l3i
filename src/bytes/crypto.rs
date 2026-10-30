//! Secure randomness, keyed digests and signatures (feature `bytes-crypto`), through ring, the
//! provider `@dream/tls` already links: bytes from the operating system's CSPRNG, HMAC with a
//! verification that compares in constant time, PKCS#1 v1.5 RSA signing, and ECDSA signing.
//!
//! These are the primitives protocols need around a transport (a WebSocket mask, a nonce, a
//! webhook signature, a signed token), not a cryptography library: no ciphers, no key generation,
//! no key storage, no choice of random source.

use ring::hmac;
use ring::rand::{SecureRandom as _, SystemRandom};
use ring::signature::{self, EcdsaKeyPair, RsaKeyPair};
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

/// The DER bytes of `key` and the label of the PEM block they came from: DER as given with no
/// label, or the base64 body of a PEM block labelled one of `labels`. `what` names the caller and
/// `kind` the key it wants in errors, which never quote the key.
fn private_key_der(
    key: &[u8],
    what: &str,
    kind: &str,
    labels: &[&'static str],
) -> Result<(Option<&'static str>, Zeroizing<Vec<u8>>)> {
    let trimmed = key.trim_ascii_start();
    if !trimmed.starts_with(b"-----BEGIN ") {
        return Ok((None, Zeroizing::new(key.to_vec())));
    }

    let text =
        std::str::from_utf8(trimmed).map_err(|_| Error::runtime(format!("{what}: the PEM text is not UTF-8")))?;
    let label_end = text["-----BEGIN ".len()..]
        .find("-----")
        .ok_or_else(|| Error::runtime(format!("{what}: the PEM text has no BEGIN line")))?;
    let found = &text["-----BEGIN ".len().."-----BEGIN ".len() + label_end];
    let Some(label) = labels.iter().copied().find(|label| *label == found) else {
        return Err(Error::runtime(format!(
            "{what}: a PEM '{found}' block is not an unencrypted {kind} private key; expected {}",
            labels.join(" or ")
        )));
    };

    let body_start = "-----BEGIN ".len() + label_end + "-----".len();
    let end = text
        .find(&format!("-----END {label}-----"))
        .ok_or_else(|| Error::runtime(format!("{what}: the PEM text has no END {label} line")))?;
    let body =
        Zeroizing::new(text[body_start..end].bytes().filter(|byte| !byte.is_ascii_whitespace()).collect::<Vec<u8>>());
    super::decode_base64(&body, what)
        .map(|der| (Some(label), Zeroizing::new(der)))
        .map_err(|_| Error::runtime(format!("{what}: the PEM body is not base64")))
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
    let (_, der) = private_key_der(key, "bytes.rsaSign", "RSA", &["PRIVATE KEY", "RSA PRIVATE KEY"])?;
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

/// The DER of `id-ecPublicKey` (1.2.840.10045.2.1), the algorithm of every PKCS#8 EC key.
const EC_PUBLIC_KEY_OID: &[u8] = &[0x06, 0x07, 0x2a, 0x86, 0x48, 0xce, 0x3d, 0x02, 0x01];
/// The DER of `prime256v1` (1.2.840.10045.3.1.7), P-256.
const P256_OID: &[u8] = &[0x06, 0x08, 0x2a, 0x86, 0x48, 0xce, 0x3d, 0x03, 0x01, 0x07];
/// The DER of `secp384r1` (1.3.132.0.34), P-384.
const P384_OID: &[u8] = &[0x06, 0x05, 0x2b, 0x81, 0x04, 0x00, 0x22];

/// A DER tag, its definite length and `contents`, appended to `out`.
fn push_der(out: &mut Vec<u8>, tag: u8, contents: &[u8]) {
    out.push(tag);
    let length = contents.len();
    if length < 0x80 {
        out.push(length as u8);
    } else {
        let digits = length.to_be_bytes();
        let significant = &digits[digits.iter().take_while(|digit| **digit == 0).count()..];
        out.push(0x80 | significant.len() as u8);
        out.extend_from_slice(significant);
    }
    out.extend_from_slice(contents);
}

/// The PKCS#8 wrapping of a SEC1 `ECPrivateKey` on the curve `curve` names: ring reads only
/// PKCS#8, and the `EC PRIVATE KEY` blocks OpenSSL and token issuers hand out are SEC1. ring still
/// checks the SEC1 key's own curve parameter, when it has one, against `curve`.
fn sec1_to_pkcs8(sec1: &[u8], curve: &[u8]) -> Zeroizing<Vec<u8>> {
    let mut algorithm = Vec::with_capacity(EC_PUBLIC_KEY_OID.len() + curve.len());
    algorithm.extend_from_slice(EC_PUBLIC_KEY_OID);
    algorithm.extend_from_slice(curve);
    let mut body = Zeroizing::new(vec![0x02, 0x01, 0x00]);
    push_der(&mut body, 0x30, &algorithm);
    push_der(&mut body, 0x04, sec1);
    let mut pkcs8 = Zeroizing::new(Vec::with_capacity(body.len() + 4));
    push_der(&mut pkcs8, 0x30, &body);
    pkcs8
}

/// The ECDSA signature of `data` under `key` with `name`'s digest, `r` and `s` each fixed-width
/// and big-endian, one after the other: ES256 (a P-256 key, `sha256`) or ES384 (a P-384 key,
/// `sha384`) as JSON Web Tokens name them. ECDSA is randomized: two signatures of the same data
/// differ, and both verify.
///
/// # Errors
///
/// A digest other than sha256 or sha384, or a key that isn't a private key on that digest's curve
/// as PKCS#8 or SEC1, in DER or PEM, with its public point.
pub fn sign_ecdsa(data: &[u8], key: &[u8], name: &str) -> Result<Vec<u8>> {
    const WHAT: &str = "bytes.ecdsaSign";
    let (algorithm, curve, curve_name): (&'static signature::EcdsaSigningAlgorithm, _, _) = match name {
        "sha256" => (&signature::ECDSA_P256_SHA256_FIXED_SIGNING, P256_OID, "P-256"),
        "sha384" => (&signature::ECDSA_P384_SHA384_FIXED_SIGNING, P384_OID, "P-384"),
        other => return Err(Error::runtime(format!("{WHAT}: '{other}' is not one of sha256, sha384"))),
    };
    let (label, der) = private_key_der(key, WHAT, "EC", &["PRIVATE KEY", "EC PRIVATE KEY"])?;
    let random = SystemRandom::new();
    let from_sec1 = || EcdsaKeyPair::from_pkcs8(algorithm, &sec1_to_pkcs8(&der, curve), &random);
    let pair = match label {
        Some("EC PRIVATE KEY") => from_sec1(),
        Some(_) => EcdsaKeyPair::from_pkcs8(algorithm, &der, &random),
        None => EcdsaKeyPair::from_pkcs8(algorithm, &der, &random).or_else(|_| from_sec1()),
    }
    .map_err(|rejected| {
        Error::runtime(format!(
            "{WHAT}: the key is not a {curve_name} private key ring accepts as PKCS#8 or SEC1 with its public point ({rejected})"
        ))
    })?;
    pair.sign(&random, data)
        .map(|signed| signed.as_ref().to_vec())
        .map_err(|_| Error::runtime(format!("{WHAT}: signing failed")))
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
        .doc("The raw PKCS#1 v1.5 RSA signature of data (RS256, RS384 or RS512 for algorithm sha256, sha384 or sha512) under an RSA private key of at least 2048 bits, PKCS#8 or PKCS#1, PEM or DER.")
        .function("ecdsaSign", |data: BytesView<'_>, key: BytesView<'_>, name: &str| -> Result<NewBuffer> {
            // SAFETY: signing reads both views and calls nothing.
            sign_ecdsa(unsafe { bytes(&data) }, unsafe { bytes(&key) }, name).map(NewBuffer)
        })
        .signature("(data: buffer | string, privateKey: buffer | string, algorithm: string) -> buffer")
        .doc("The raw ECDSA signature of data, r then s fixed-width (ES256 for algorithm sha256 under a P-256 key, ES384 for sha384 under a P-384 key), from a PKCS#8 or SEC1 key, PEM or DER. Randomized: every call signs afresh.");
}
