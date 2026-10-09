//! Verified, encrypted streams over `@dream/tcp`: the `dream.tls` extension, module
//! `@dream/tls`.
//!
//! ```lua
//! local tls = require('@dream/tls')
//! -- raw: a dream.tcp.Stream whose finishConnect() returned true, not watched by any poller
//! local secure = assert(tls.client(raw, { serverName = 'example.com', alpn = { 'http/1.1' } }))
//! -- raw is now 'consumed'. Watch secure instead, and call handshake() on readiness:
//! local done, message, kind = secure:handshake()   -- true, false (call again), or nil
//! ```
//!
//! rustls with the ring provider: TLS 1.3 and 1.2 with rustls's default suites, no older
//! protocol, no 0-RTT, no key log, no way to extract session secrets. A client verifies the
//! certificate chain, its validity period and the server name it was given, which is
//! independent of the address it connected to (and is sent as SNI when it is a DNS name). By
//! default it trusts the platform's store through rustls-platform-verifier and fails closed
//! when that store is unusable; a [`ClientConfig`] can add roots, or trust only given roots.
//! Nothing in the API skips or weakens verification.
//!
//! A session takes its TCP stream whole (see `dream.tcp.Stream`'s `consumed` state), so the
//! plaintext handle can neither read, write nor close the socket again. The session is a
//! `dream_tcp_Watchable`: one poller, one registration, the same tokens.
//!
//! # Bytes and bounds
//!
//! `write` returns how much plaintext the session took into its send buffer, capped at
//! `bufferLimit` bytes of pending ciphertext (64 KiB by default); `flush` pushes ciphertext to
//! the socket. `readInto` hands over decrypted bytes; rustls holds at most 16 KiB of them.
//! `0` is a clean end of stream (the peer's close_notify); an end without close_notify is the
//! error `truncated`, never `0`. Each call does a bounded amount of socket work.
//!
//! # Authority
//!
//! There is no capability here: a session needs a connected TCP stream, and making one is
//! `@dream/tcp`'s to allow. Certificates and keys are bytes the caller holds; this module never
//! opens a file.

mod config;
mod stream;

pub use config::{ClientConfig, MAX_CHAIN, MAX_INPUT_BYTES, MAX_ROOTS, ServerConfig};
pub(crate) use stream::watch_target;
pub use stream::{
    DEFAULT_BUFFER_LIMIT, DEFAULT_HANDSHAKE_TIMEOUT_MS, MAX_BUFFER_LIMIT, MAX_HANDSHAKE_TIMEOUT_MS, MIN_BUFFER_LIMIT,
    TlsState, TlsStream,
};

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::{Arc, OnceLock};

use rustls::crypto::CryptoProvider;

use crate::bind::Call;
use crate::error::Result;
use crate::extension::{Extension, ExtensionDescriptor, InstallContext};
use crate::stack::ValueView;

/// The extension id.
pub const EXTENSION_ID: &str = "dream.tls";
/// The module path.
pub const MODULE: &str = "@dream/tls";

/// ring's provider with rustls's default suites and key exchanges, made once.
pub(crate) fn provider() -> Arc<CryptoProvider> {
    static PROVIDER: OnceLock<Arc<CryptoProvider>> = OnceLock::new();
    Arc::clone(PROVIDER.get_or_init(|| Arc::new(rustls::crypto::ring::default_provider())))
}

/// The kinds a TLS call reports beyond the socket's own.
const TLS_KINDS: &str = "\"truncated\" | \"certificateUntrusted\" | \"certificateNameMismatch\" \
    | \"certificateExpired\" | \"certificateNotYetValid\" | \"certificateRevoked\" | \"certificateInvalid\" \
    | \"tlsAlert\" | \"noApplicationProtocol\" | \"tlsIncompatible\" | \"tlsProtocol\" | \"invalidCertificate\" \
    | \"invalidKey\" | \"keyMismatch\" | \"trustStoreUnavailable\"";

/// The `dream.tls` extension. It requires `dream.tcp`.
#[derive(Clone, Copy, Debug, Default)]
pub struct TlsExtension;

impl Extension for TlsExtension {
    fn id(&self) -> &'static str {
        EXTENSION_ID
    }

    fn describe(&self, d: &mut ExtensionDescriptor) -> Result<()> {
        d.requires(crate::tcp::EXTENSION_ID).widen_type_alias("dream_tcp_Watchable", "dream_tls_Stream");
        d.type_alias("dream_tls_ErrorKind", format!("{} | {TLS_KINDS}", crate::outcome::NETWORK_ERROR_KIND_TYPE));
        d.type_alias("dream_tls_State", "\"handshaking\" | \"open\" | \"failed\" | \"closed\"");
        d.type_alias(
            "dream_tls_ClientConfigOptions",
            "{ roots: (string | buffer | { string | buffer })?, platform: boolean?, alpn: { string }?, versions: { string }? }",
        );
        d.type_alias(
            "dream_tls_ServerConfigOptions",
            "{ certChain: string | buffer | { string | buffer }, privateKey: string | buffer, alpn: { string }?, versions: { string }? }",
        );
        d.type_alias(
            "dream_tls_ClientOptions",
            "{ serverName: string, config: dream_tls_ClientConfig?, alpn: { string }?, bufferLimit: number?, handshakeTimeoutMs: number? }",
        );
        d.type_alias("dream_tls_ServerOptions", "{ bufferLimit: number?, handshakeTimeoutMs: number? }");
        config::describe(d);
        stream::describe(d);
        d.module(MODULE)
            .doc("Verified TLS 1.3 and 1.2 sessions over @dream/tcp streams, client and server, driven without blocking.")
            .installed("client")
            .signature("(stream: dream_tcp_Stream, options: dream_tls_ClientOptions) -> (dream_tls_Stream?, string?, dream_tls_ErrorKind?)")
            .doc("Takes a connected, unwatched TCP stream for a client session verifying serverName (a DNS name or an IP address, independent of the address connected to). Without config, the platform trust store, loaded once per runtime.")
            .function("server", stream::server)
            .signature("(stream: dream_tcp_Stream, config: dream_tls_ServerConfig, options: dream_tls_ServerOptions?) -> (dream_tls_Stream?, string?, dream_tls_ErrorKind?)")
            .doc("Takes an accepted, unwatched TCP stream for a server session under config.");
        Ok(())
    }

    fn install(&self, cx: &mut InstallContext<'_>) -> Result<()> {
        let defaults: Rc<stream::DefaultClient> = Rc::new(RefCell::new(None));
        cx.module(MODULE)?.function(
            "client",
            move |call: &Call<'_>, stream: ValueView<'_>, options: ValueView<'_>| {
                stream::client(call, &defaults, stream, options)
            },
        )?;
        Ok(())
    }
}
