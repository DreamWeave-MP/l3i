//! `dream.tls.ClientConfig` and `dream.tls.ServerConfig`: immutable rustls configurations,
//! built once and shared by every session made from them.
//!
//! Certificates and keys arrive as PEM or DER bytes the caller already holds: this module
//! never reads a file and needs no filesystem capability. Private key bytes are copied once
//! into a buffer that is zeroed when dropped; the Luau string they came from cannot be.
//!
//! Every input is bounded before it costs native memory: an array's length before its first
//! element is read, each input's size and the list's running total before its bytes are
//! parsed, and the certificate count before each certificate is kept. Certificates are parsed
//! straight from the script's bytes, so no copy of an input is staged. A table repeating one
//! large string costs the script almost nothing, so only bounds checked this early hold.

use std::borrow::Cow;
use std::sync::Arc;

use rustls::client::WebPkiServerVerifier;
use rustls::client::danger::ServerCertVerifier;
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use rustls::{RootCertStore, SupportedProtocolVersion};
use zeroize::Zeroizing;

use crate::bind::Call;
use crate::convert::{BytesView, FromView};
use crate::error::{Error, Result};
use crate::extension::{ExtensionDescriptor, TagPolicy};
use crate::options::Options;
use crate::outcome::{Failure, Outcome};
use crate::stack::{Frame, ValueView};
use crate::userdata::{Owned, Userdata};

use super::provider;

/// The most bytes one certificate input may have.
pub const MAX_INPUT_BYTES: usize = 1 << 20;
/// The most bytes all the certificate inputs of one list (`roots`, `certChain`) may have.
pub const MAX_LIST_BYTES: usize = 4 << 20;
/// The most bytes a private key input may have.
pub const MAX_KEY_BYTES: usize = 64 << 10;
/// The most trust anchors one client configuration adds.
pub const MAX_ROOTS: usize = 1024;
/// The most certificates in a server's chain.
pub const MAX_CHAIN: usize = 16;
/// The most ALPN protocols.
const MAX_ALPN: usize = 16;
/// The most entries of `versions` (there are two versions; repeats are allowed, not hoarded).
const MAX_VERSIONS: usize = 4;

/// `dream.tls.ClientConfig`: trust and protocol settings for client sessions.
pub struct ClientConfig(pub(crate) Arc<rustls::ClientConfig>);

// SAFETY: plain Rust data behind an `Arc`, no Lua references, no Lua API in `Drop`.
unsafe impl Userdata for ClientConfig {
    const NAME: &'static str = "dream.tls.ClientConfig";
}

/// `dream.tls.ServerConfig`: a certificate chain, its key and protocol settings.
pub struct ServerConfig(pub(crate) Arc<rustls::ServerConfig>);

// SAFETY: as `ClientConfig`.
unsafe impl Userdata for ServerConfig {
    const NAME: &'static str = "dream.tls.ServerConfig";
}

fn failure(message: String, kind: &'static str) -> Failure {
    Failure { message: Cow::Owned(message), kind }
}

fn is_pem(bytes: &[u8]) -> bool {
    bytes.windows(10).any(|window| window == b"-----BEGIN")
}

/// A private key's bytes, copied once into a buffer zeroed when it drops.
fn key_bytes(what: &str, view: ValueView<'_>) -> Result<Zeroizing<Vec<u8>>> {
    let bytes = BytesView::from_view(view)
        .map_err(|_| view.field_type_error(what, "PEM or DER bytes as a string or buffer"))?;
    if bytes.len() > MAX_KEY_BYTES {
        return Err(Error::runtime(format!("{what}: {} bytes, more than {MAX_KEY_BYTES}", bytes.len())));
    }
    // SAFETY: copied at once, with no call into Lua while the slice lives.
    Ok(Zeroizing::new(unsafe { bytes.bytes_unchecked() }.to_vec()))
}

/// The certificates of one list, collected within its bounds as the inputs are read.
struct Collected<'w> {
    what: &'w str,
    limit: usize,
    bytes: usize,
    certificates: Vec<CertificateDer<'static>>,
    /// Malformed input: a result for the script, not an error.
    failure: Option<Failure>,
}

impl Collected<'_> {
    fn keep(&mut self, certificate: CertificateDer<'static>) -> Result<()> {
        if self.certificates.len() == self.limit {
            return Err(Error::runtime(format!("{}: more than {} certificates", self.what, self.limit)));
        }
        self.certificates.push(certificate);
        Ok(())
    }

    /// Parses one input straight from the script's bytes: every certificate of a PEM text, or
    /// one DER certificate. Its size and the list's total are checked before anything is
    /// copied, the count before each certificate is kept.
    fn add(&mut self, label: &str, view: ValueView<'_>) -> Result<()> {
        let bytes = BytesView::from_view(view)
            .map_err(|_| view.field_type_error(label, "PEM or DER bytes as a string or buffer"))?;
        if bytes.len() > MAX_INPUT_BYTES {
            return Err(Error::runtime(format!("{label}: {} bytes, more than {MAX_INPUT_BYTES}", bytes.len())));
        }
        self.bytes += bytes.len();
        if self.bytes > MAX_LIST_BYTES {
            return Err(Error::runtime(format!(
                "{}: more than {MAX_LIST_BYTES} bytes of certificate input",
                self.what
            )));
        }
        // SAFETY: the slice is parsed (and what is kept copied) before this returns, with no
        // call into Lua while it lives.
        let slice = unsafe { bytes.bytes_unchecked() };
        if !is_pem(slice) {
            return self.keep(CertificateDer::from(slice.to_vec()));
        }
        let before = self.certificates.len();
        for certificate in CertificateDer::pem_slice_iter(slice) {
            match certificate {
                Ok(certificate) => self.keep(certificate)?,
                Err(error) => {
                    self.failure = Some(failure(format!("{label}: malformed PEM: {error}"), "invalidCertificate"));
                    return Err(Error::runtime("malformed certificate input"));
                }
            }
        }
        if self.certificates.len() == before {
            self.failure =
                Some(failure(format!("{label}: the PEM text holds no CERTIFICATE block"), "invalidCertificate"));
            return Err(Error::runtime("malformed certificate input"));
        }
        Ok(())
    }
}

/// Certificates from a string, a buffer, or an array of them, at most `limit` of them and
/// [`MAX_LIST_BYTES`] of input.
fn certificates(
    frame: &Frame<'_>,
    what: &str,
    view: ValueView<'_>,
    limit: usize,
) -> Result<std::result::Result<Vec<CertificateDer<'static>>, Failure>> {
    let mut collected = Collected { what, limit, bytes: 0, certificates: Vec::new(), failure: None };
    let read = if view.is_table() {
        let table = view.as_table()?;
        if table.raw_len() > limit {
            return Err(Error::runtime(format!("{what}: {} entries, more than {limit}", table.raw_len())));
        }
        table.for_each_array(frame, |_, index, value| collected.add(&format!("{what}[{index}]"), value))
    } else {
        collected.add(what, view)
    };
    if let Some(failure) = collected.failure {
        return Ok(Err(failure));
    }
    read?;
    Ok(Ok(collected.certificates))
}

/// A private key from PEM (PKCS #8, PKCS #1 or SEC1) or DER.
fn private_key(what: &str, bytes: &[u8]) -> std::result::Result<PrivateKeyDer<'static>, Failure> {
    let key = if is_pem(bytes) {
        PrivateKeyDer::from_pem_slice(bytes).map_err(|error| error.to_string())
    } else {
        PrivateKeyDer::try_from(bytes.to_vec()).map_err(str::to_owned)
    };
    // The parser's message names the problem, never the key's bytes.
    key.map_err(|error| failure(format!("{what}: not a usable private key: {error}"), "invalidKey"))
}

/// `versions = { "1.2", "1.3" }`, both by default.
fn versions(what: &str, names: Option<Vec<String>>) -> Result<Vec<&'static SupportedProtocolVersion>> {
    let Some(names) = names else { return Ok(vec![&rustls::version::TLS13, &rustls::version::TLS12]) };
    let mut versions = Vec::new();
    for name in names {
        let version = match name.as_str() {
            "1.3" => &rustls::version::TLS13,
            "1.2" => &rustls::version::TLS12,
            other => {
                return Err(Error::runtime(format!(
                    "{what}: versions holds '{other}'; only '1.2' and '1.3' exist here"
                )));
            }
        };
        if !versions.contains(&version) {
            versions.push(version);
        }
    }
    if versions.is_empty() {
        return Err(Error::runtime(format!("{what}: versions is empty")));
    }
    Ok(versions)
}

/// An array of strings.
fn string_list(frame: &Frame<'_>, what: &str, view: ValueView<'_>) -> Result<Vec<String>> {
    if !view.is_table() {
        return Err(view.field_type_error(what, "an array of strings"));
    }
    let table = view.as_table()?;
    if table.raw_len() > MAX_VERSIONS {
        return Err(Error::runtime(format!("{what}: more than {MAX_VERSIONS} entries")));
    }
    let mut items = Vec::new();
    table.for_each_array(frame, |_, index, value| {
        if items.len() == MAX_VERSIONS {
            return Err(Error::runtime(format!("{what}: more than {MAX_VERSIONS} entries")));
        }
        let item = value.read::<&str>().map_err(|_| Error::runtime(format!("{what}[{index}] must be a string")))?;
        items.push(item.to_owned());
        Ok(())
    })?;
    Ok(items)
}

/// `alpn = { "h2", "http/1.1" }`.
pub(crate) fn alpn(frame: &Frame<'_>, what: &str, view: ValueView<'_>) -> Result<Vec<Vec<u8>>> {
    if !view.is_table() {
        return Err(view.field_type_error(what, "an array of protocol names"));
    }
    let table = view.as_table()?;
    if table.raw_len() > MAX_ALPN {
        return Err(Error::runtime(format!("{what}: more than {MAX_ALPN} protocols")));
    }
    let mut protocols = Vec::new();
    table.for_each_array(frame, |_, index, value| {
        if protocols.len() == MAX_ALPN {
            return Err(Error::runtime(format!("{what}: more than {MAX_ALPN} protocols")));
        }
        let name = value.read::<&[u8]>().map_err(|_| Error::runtime(format!("{what}[{index}] must be a string")))?;
        if name.is_empty() || name.len() > 255 {
            return Err(Error::runtime(format!("{what}[{index}] must be 1 to 255 bytes")));
        }
        protocols.push(name.to_vec());
        Ok(())
    })?;
    Ok(protocols)
}

/// The platform's verifier with optional extra roots: the OS trust store and its policy
/// (revocation and all, where the platform checks it). Fails when the store is unusable,
/// never falling back to roots of its own.
fn platform_verifier(extra: Vec<CertificateDer<'static>>) -> std::result::Result<Arc<dyn ServerCertVerifier>, Failure> {
    let unavailable = |error: rustls::Error| {
        failure(format!("dream.tls: the platform trust store is unusable: {error}"), "trustStoreUnavailable")
    };
    #[cfg(not(target_os = "android"))]
    let verifier = if extra.is_empty() {
        rustls_platform_verifier::Verifier::new(provider())
    } else {
        rustls_platform_verifier::Verifier::new_with_extra_roots(extra, provider())
    };
    #[cfg(target_os = "android")]
    let verifier = if extra.is_empty() {
        rustls_platform_verifier::Verifier::new(provider())
    } else {
        return Err(failure("dream.tls: extra roots need platform = false on Android".to_owned(), "unsupported"));
    };
    Ok(Arc::new(verifier.map_err(unavailable)?))
}

/// A client configuration over `verifier`.
fn client_over(
    verifier: Arc<dyn ServerCertVerifier>,
    versions: &[&'static SupportedProtocolVersion],
    alpn: Vec<Vec<u8>>,
) -> std::result::Result<Arc<rustls::ClientConfig>, Failure> {
    let builder = rustls::ClientConfig::builder_with_provider(provider())
        .with_protocol_versions(versions)
        .map_err(|error| failure(format!("dream.tls: {error}"), "tlsIncompatible"))?;
    // `dangerous` is rustls's name for supplying a verifier at all; the one supplied verifies
    // fully (the platform's, or webpki's over explicit roots).
    let mut config = builder.dangerous().with_custom_certificate_verifier(verifier).with_no_client_auth();
    config.alpn_protocols = alpn;
    config.enable_early_data = false;
    Ok(Arc::new(config))
}

/// The configuration `tls.client` uses when given none: the platform verifier, TLS 1.2 and 1.3,
/// no ALPN.
pub(crate) fn platform_default() -> std::result::Result<Arc<rustls::ClientConfig>, Failure> {
    client_over(platform_verifier(Vec::new())?, &[&rustls::version::TLS13, &rustls::version::TLS12], Vec::new())
}

/// `tls.clientConfig{ roots?, platform?, alpn?, versions? }`.
fn client_config(call: &Call<'_>, options: Option<ValueView<'_>>) -> Result<Outcome<Owned<ClientConfig>>> {
    const WHAT: &str = "dream.tls.clientConfig";
    let mut roots: Option<std::result::Result<Vec<CertificateDer<'static>>, Failure>> = None;
    let mut platform = true;
    let mut protocols = Vec::new();
    let mut names: Option<Vec<String>> = None;
    if let Some(options) = options.filter(|view| !view.is_nil()) {
        Options::read(call, options, WHAT, |o| {
            roots = o.with_optional_in("roots", |frame, view| {
                certificates(frame, &format!("{WHAT}: roots"), view, MAX_ROOTS)
            })?;
            platform = o.or("platform", true)?;
            protocols = o
                .with_optional_in("alpn", |frame, view| alpn(frame, &format!("{WHAT}: alpn"), view))?
                .unwrap_or_default();
            names =
                o.with_optional_in("versions", |frame, view| string_list(frame, &format!("{WHAT}: versions"), view))?;
            Ok(())
        })?;
    }
    let versions = versions(WHAT, names)?;
    let roots = match roots.transpose() {
        Ok(roots) => roots.unwrap_or_default(),
        Err(failure) => return Ok(Outcome::Failed(failure)),
    };
    let verifier: Arc<dyn ServerCertVerifier> = if platform {
        match platform_verifier(roots) {
            Ok(verifier) => verifier,
            Err(failure) => return Ok(Outcome::Failed(failure)),
        }
    } else {
        if roots.is_empty() {
            return Err(Error::runtime(format!("{WHAT}: platform = false needs roots to trust")));
        }
        let mut store = RootCertStore::empty();
        for (index, root) in roots.into_iter().enumerate() {
            if let Err(error) = store.add(root) {
                return Ok(Outcome::Failed(failure(
                    format!("{WHAT}: roots: certificate {}: {error}", index + 1),
                    "invalidCertificate",
                )));
            }
        }
        match WebPkiServerVerifier::builder_with_provider(Arc::new(store), provider()).build() {
            Ok(verifier) => verifier,
            Err(error) => return Ok(Outcome::Failed(failure(format!("{WHAT}: {error}"), "invalidCertificate"))),
        }
    };
    Ok(match client_over(verifier, &versions, protocols) {
        Ok(config) => Outcome::Done(Owned(ClientConfig(config))),
        Err(failure) => Outcome::Failed(failure),
    })
}

/// `tls.serverConfig{ certChain, privateKey, alpn?, versions? }`.
fn server_config(call: &Call<'_>, options: ValueView<'_>) -> Result<Outcome<Owned<ServerConfig>>> {
    const WHAT: &str = "dream.tls.serverConfig";
    let mut chain = None;
    let mut key = None;
    let mut protocols = Vec::new();
    let mut names: Option<Vec<String>> = None;
    Options::read(call, options, WHAT, |o| {
        chain = Some(o.with_required_in("certChain", |frame, view| {
            certificates(frame, &format!("{WHAT}: certChain"), view, MAX_CHAIN)
        })?);
        key = Some(o.with_required("privateKey", |view| key_bytes(&format!("{WHAT}: privateKey"), view))?);
        protocols =
            o.with_optional_in("alpn", |frame, view| alpn(frame, &format!("{WHAT}: alpn"), view))?.unwrap_or_default();
        names = o.with_optional_in("versions", |frame, view| string_list(frame, &format!("{WHAT}: versions"), view))?;
        Ok(())
    })?;
    let versions = versions(WHAT, names)?;
    let chain = match chain.expect("required") {
        Ok(chain) if chain.is_empty() => return Err(Error::runtime(format!("{WHAT}: certChain is empty"))),
        Ok(chain) => chain,
        Err(failure) => return Ok(Outcome::Failed(failure)),
    };
    let key = match private_key(&format!("{WHAT}: privateKey"), &key.expect("required")) {
        Ok(key) => key,
        Err(failure) => return Ok(Outcome::Failed(failure)),
    };
    let builder = match rustls::ServerConfig::builder_with_provider(provider()).with_protocol_versions(&versions) {
        Ok(builder) => builder,
        Err(error) => return Ok(Outcome::Failed(failure(format!("{WHAT}: {error}"), "tlsIncompatible"))),
    };
    // Checks that the key parses and is the leaf certificate's.
    let mut config = match builder.with_no_client_auth().with_single_cert(chain, key) {
        Ok(config) => config,
        Err(rustls::Error::InconsistentKeys(why)) => {
            return Ok(Outcome::Failed(failure(
                format!("{WHAT}: the private key does not match the leaf certificate ({why:?})"),
                "keyMismatch",
            )));
        }
        Err(error) => return Ok(Outcome::Failed(failure(format!("{WHAT}: {error}"), "invalidKey"))),
    };
    config.alpn_protocols = protocols;
    config.max_early_data_size = 0;
    Ok(Outcome::Done(Owned(ServerConfig(Arc::new(config)))))
}

pub(crate) fn describe(d: &mut ExtensionDescriptor) {
    d.userdata::<ClientConfig>(ClientConfig::NAME)
        .tag(TagPolicy::Never)
        .doc("Immutable client trust and protocol settings, shared by every session made from it.");
    d.userdata::<ServerConfig>(ServerConfig::NAME)
        .tag(TagPolicy::Never)
        .doc("An immutable certificate chain, its key and protocol settings, shared by every session made from it.");
    d.module(super::MODULE)
        .function("clientConfig", client_config)
        .signature("(options: dream_tls_ClientConfigOptions?) -> (dream_tls_ClientConfig?, string?, dream_tls_ErrorKind?)")
        .doc("Builds client settings once: the platform's trust store (platform = true, the default) with any extra roots, or only the given roots (platform = false). Loading the platform store may read files; do it once and reuse the config.")
        .function("serverConfig", server_config)
        .signature("(options: dream_tls_ServerConfigOptions) -> (dream_tls_ServerConfig?, string?, dream_tls_ErrorKind?)")
        .doc("Builds server settings once from a PEM or DER chain and its private key, checking that they match. New configs serve new sessions; sessions keep the config they started with.");
}
