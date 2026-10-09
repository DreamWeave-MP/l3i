//! Raw TCP byte streams and a readiness poller: the `dream.tcp` extension, module `@dream/tcp`.
//!
//! ```lua
//! local tcp = require('@dream/tcp')
//! local listener = assert(tcp.listen('127.0.0.1:0', { backlog = 64 }))
//! local poller = tcp.poller()
//! poller:watch(listener, 1, 'read')
//! local scratch = buffer.create(65536)
//! while running do
//!     poller:wait(250)
//!     while true do
//!         local token, readable, writable, closed = poller:next()
//!         if token == nil then break end
//!         -- drain: accept or readInto until 'wouldBlock', write until done or 'wouldBlock'
//!     end
//! end
//! ```
//!
//! This is a transport primitive, not a protocol: no HTTP, TLS, framing, text encoding or
//! message boundaries. Protocols are written in Luau on top of it. It is separate from the
//! `dream.udp` game transport and shares nothing with it, capabilities included.
//!
//! # Bytes
//!
//! A [`Stream`] is a byte stream. `readInto` receives straight into a caller-owned buffer and
//! returns the count; `0` is end of stream (the peer closed its write half), and no data yet is
//! `nil, message, 'wouldBlock'`: the two are never conflated. `write` hands the OS one range and
//! returns how much it accepted, which may be less than asked: the caller keeps the rest and
//! waits for writable readiness. There is no send queue behind a stream and no call that loops
//! until everything is written. Nothing reads or writes the buffer after the call returns.
//!
//! # Readiness
//!
//! A [`Poller`] watches handles under caller-chosen tokens. `wait(ms)` waits, bounded, for
//! readiness and queues at most `maxEvents` events; `next()` hands them out one at a time as
//! `token, readable, writable, closed`. The library never calls Luau. Readiness is level
//! triggered: a handle stays readable until an operation on it reports `'wouldBlock'`, so a
//! caller drains accept and read until `'wouldBlock'`, and watches `'write'` only while it has
//! bytes waiting (a stream with room in its send buffer is always writable). `closed` says the
//! peer will send nothing more (end of stream or a socket error); it is reported with read
//! interest and stays set.
//!
//! # Authority
//!
//! Nothing is granted by default. `tcp.connect` needs [`CONNECT_CAPABILITY`]; `tcp.listen` needs
//! [`LISTEN_CAPABILITY`], and binding anything but a loopback address also needs
//! [`PUBLIC_CAPABILITY`]. A stream accepted from a listener carries the listener's authority, and
//! handles a host pushes with [`Listener::push`] or [`Stream::push`] need no capability at all.
//! `connect` reaches every destination the host can reach, loopback services included: it is a
//! coarse grant, not an endpoint policy. Addresses are numeric (`127.0.0.1:80`, `[::1]:80`): a
//! name lookup blocks, so DNS is not part of this module.
//!
//! # Lifetimes
//!
//! Handles and pollers are native state owned by their userdata; a poller refers to the handles
//! it watches weakly and a handle to its poller weakly, so neither keeps the other, or a socket,
//! alive. `close()` releases a socket and its registration at once, and collecting the userdata
//! does the same. Handles belong to the runtime that made them and are not `Send`.

pub(crate) mod poller;
pub(crate) mod socket;

pub use poller::Poller;
pub use socket::{Listener, Stream, StreamState};

use std::net::SocketAddr;

use crate::bind::{ArgView, Call, StackResults};
use crate::convert::Exact;
use crate::error::{Error, Result};
use crate::extension::{Extension, ExtensionDescriptor, InstallContext};
use crate::options::Options;
use crate::outcome::{Failure, Outcome};
use crate::stack::ValueView;
use crate::userdata::Owned;

/// The extension id.
pub const EXTENSION_ID: &str = "dream.tcp";
/// The module path.
pub const MODULE: &str = "@dream/tcp";
/// The capability `tcp.connect` needs: outbound connections to any address.
pub const CONNECT_CAPABILITY: &str = "network.tcp.connect";
/// The capability `tcp.listen` needs: listening on loopback addresses.
pub const LISTEN_CAPABILITY: &str = "network.tcp.listen";
/// With [`LISTEN_CAPABILITY`], what `tcp.listen` needs to bind a wildcard or non-loopback address.
pub const PUBLIC_CAPABILITY: &str = "network.tcp.public";

/// The longest `Poller:wait` any poller allows, in milliseconds.
pub const MAX_WAIT_MS: u32 = 60_000;
/// The most events one wait queues, and the most watches one poller holds.
pub const MAX_POLLER_LIMIT: u32 = 65_536;
/// The most streams accepted from one listener that may be open at once.
pub const MAX_STREAM_LIMIT: u32 = 65_536;

const LISTEN_OPTIONS_TYPE: &str =
    "{ backlog: number?, reuseAddress: boolean?, noDelay: boolean?, maxStreams: number? }";
const CONNECT_OPTIONS_TYPE: &str = "{ noDelay: boolean? }";
const POLLER_OPTIONS_TYPE: &str = "{ maxEvents: number?, maxWatches: number?, maxWaitMs: number? }";

/// A numeric socket address: `127.0.0.1:80` or `[::1]:80`. A name is a script error: resolving
/// it would block.
fn parse_address(what: &str, text: &str) -> Result<SocketAddr> {
    text.parse().map_err(|_| {
        Error::runtime(format!(
            "dream.tcp.{what}: '{text}' is not a numeric address and port such as 127.0.0.1:80 or [::1]:80 (names are not resolved)"
        ))
    })
}

/// A whole-number option in `1..=max`.
fn bounded(what: &str, key: &str, value: Option<Exact<i64>>, default: u32, max: u32) -> Result<u32> {
    let Some(value) = value else { return Ok(default) };
    u32::try_from(value.0)
        .ok()
        .filter(|value| (1..=max).contains(value))
        .ok_or_else(|| Error::runtime(format!("dream.tcp.{what}: {key} must be in [1, {max}], got {}", value.0)))
}

/// An OS refusal as the `nil, message, kind` a script gets.
fn refused(what: &str, subject: &impl std::fmt::Display, error: &std::io::Error) -> Failure {
    Failure {
        message: format!("dream.tcp.{what}: {subject}: {error}").into(),
        kind: crate::outcome::network_kind_of(error),
    }
}

/// What `tcp.listen` reads from its options.
pub(crate) struct ListenOptions {
    pub(crate) backlog: u32,
    pub(crate) reuse_address: bool,
    pub(crate) no_delay: bool,
    pub(crate) max_streams: u32,
}

fn listen(
    call: &Call<'_>,
    granted: Grants,
    address: &str,
    options: Option<ValueView<'_>>,
) -> Result<Outcome<StackResults>> {
    let address = parse_address("listen", address)?;
    if !granted.listen {
        return Err(denied("listen", LISTEN_CAPABILITY));
    }
    if !address.ip().to_canonical().is_loopback() && !granted.public {
        return Err(Error::permission(format!(
            "dream.tcp.listen: {address} is not a loopback address; binding it needs the '{PUBLIC_CAPABILITY}' capability, which this runtime does not grant"
        )));
    }
    let mut resolved = ListenOptions { backlog: 128, reuse_address: true, no_delay: false, max_streams: 1024 };
    if let Some(options) = options.filter(|view| !view.is_nil()) {
        Options::read(call, options, "dream.tcp.listen", |o| {
            resolved.backlog = bounded("listen", "backlog", o.optional("backlog")?, 128, 65_535)?;
            resolved.reuse_address = o.or("reuseAddress", true)?;
            resolved.no_delay = o.or("noDelay", false)?;
            resolved.max_streams = bounded("listen", "maxStreams", o.optional("maxStreams")?, 1024, MAX_STREAM_LIMIT)?;
            Ok(())
        })?;
    }
    let listener = match Listener::bind(address, &resolved) {
        Ok(listener) => listener,
        Err(error) => return Ok(Outcome::Failed(refused("listen", &address, &error))),
    };
    crate::userdata::push_owned(call, listener)?;
    Ok(Outcome::Done(StackResults))
}

fn connect(
    call: &Call<'_>,
    granted: Grants,
    address: &str,
    options: Option<ValueView<'_>>,
) -> Result<Outcome<StackResults>> {
    let address = parse_address("connect", address)?;
    if !granted.connect {
        return Err(denied("connect", CONNECT_CAPABILITY));
    }
    let mut no_delay = false;
    if let Some(options) = options.filter(|view| !view.is_nil()) {
        Options::read(call, options, "dream.tcp.connect", |o| {
            no_delay = o.or("noDelay", false)?;
            Ok(())
        })?;
    }
    let stream = match Stream::connect(address, no_delay) {
        Ok(stream) => stream,
        Err(error) => return Ok(Outcome::Failed(refused("connect", &address, &error))),
    };
    crate::userdata::push_owned(call, stream)?;
    Ok(Outcome::Done(StackResults))
}

fn new_poller(call: &Call<'_>, options: Option<ValueView<'_>>) -> Result<Owned<Poller>> {
    let mut limits = poller::Limits::default();
    if let Some(options) = options.filter(|view| !view.is_nil()) {
        Options::read(call, options, "dream.tcp.poller", |o| {
            limits.events = bounded("poller", "maxEvents", o.optional("maxEvents")?, 256, MAX_POLLER_LIMIT)?;
            limits.watches = bounded("poller", "maxWatches", o.optional("maxWatches")?, 1024, MAX_POLLER_LIMIT)?;
            if let Some(ms) = o.optional::<Exact<i64>>("maxWaitMs")? {
                limits.wait_ms = u32::try_from(ms.0).ok().filter(|ms| *ms <= MAX_WAIT_MS).ok_or_else(|| {
                    Error::runtime(format!("dream.tcp.poller: maxWaitMs must be in [0, {MAX_WAIT_MS}], got {}", ms.0))
                })?;
            }
            Ok(())
        })?;
    }
    match Poller::new(limits) {
        Ok(poller) => Ok(Owned(poller)),
        // Creating an epoll, kqueue or completion port fails only when the process is out of
        // descriptors or memory: nothing a script can act on.
        Err(error) => Err(Error::runtime(format!("dream.tcp.poller: {error}"))),
    }
}

fn denied(what: &str, capability: &str) -> Error {
    Error::permission(format!(
        "dream.tcp.{what}: needs the '{capability}' capability, which this runtime does not grant"
    ))
}

/// The capabilities a runtime granted, read once at install.
#[derive(Clone, Copy)]
struct Grants {
    connect: bool,
    listen: bool,
    public: bool,
}

/// The `dream.tcp` extension.
#[derive(Clone, Copy, Debug, Default)]
pub struct TcpExtension;

impl Extension for TcpExtension {
    fn id(&self) -> &'static str {
        EXTENSION_ID
    }

    fn describe(&self, d: &mut ExtensionDescriptor) -> Result<()> {
        d.type_alias("dream_tcp_ErrorKind", crate::outcome::NETWORK_ERROR_KIND_TYPE);
        d.type_alias("dream_tcp_ListenOptions", LISTEN_OPTIONS_TYPE);
        d.type_alias("dream_tcp_ConnectOptions", CONNECT_OPTIONS_TYPE);
        d.type_alias("dream_tcp_PollerOptions", POLLER_OPTIONS_TYPE);
        d.type_alias(
            "dream_tcp_StreamState",
            "\"connecting\" | \"connected\" | \"failed\" | \"closed\" | \"consumed\"",
        );
        // Other extensions widen this with the handles they let a poller watch.
        d.type_alias("dream_tcp_Watchable", "dream_tcp_Listener | dream_tcp_Stream");
        d.type_alias("dream_tcp_Interest", "\"read\" | \"write\" | \"readwrite\"");
        d.type_alias("dream_tcp_Shutdown", "\"read\" | \"write\" | \"both\"");
        d.optional_capability(CONNECT_CAPABILITY);
        d.optional_capability(LISTEN_CAPABILITY);
        d.optional_capability(PUBLIC_CAPABILITY);
        socket::describe_listener(d);
        socket::describe_stream(d);
        poller::describe_poller(d);
        d.module(MODULE)
            .doc("Raw TCP byte streams over numeric addresses and a bounded, level-triggered readiness poller; the host or script drives every phase.")
            .installed("listen")
            .signature("(address: string, options: dream_tcp_ListenOptions?) -> (dream_tcp_Listener?, string?, dream_tcp_ErrorKind?)")
            .doc("A nonblocking listener bound to address (port 0 picks one); needs network.tcp.listen, and network.tcp.public for a wildcard or non-loopback address.")
            .installed("connect")
            .signature("(address: string, options: dream_tcp_ConnectOptions?) -> (dream_tcp_Stream?, string?, dream_tcp_ErrorKind?)")
            .doc("Starts a nonblocking connection; the stream is usually 'connecting' until finishConnect() says otherwise. Needs network.tcp.connect.")
            .function("poller", new_poller)
            .signature("(options: dream_tcp_PollerOptions?) -> dream_tcp_Poller")
            .doc("A readiness poller: maxEvents per wait (default 256), maxWatches (default 1024), maxWaitMs (default 1000).")
            .constant("MAX_WAIT_MS", crate::source::CompileConstant::Number(f64::from(MAX_WAIT_MS)));
        Ok(())
    }

    fn install(&self, cx: &mut InstallContext<'_>) -> Result<()> {
        let granted = Grants {
            connect: cx.has_capability(CONNECT_CAPABILITY)?,
            listen: cx.has_capability(LISTEN_CAPABILITY)?,
            public: cx.has_capability(PUBLIC_CAPABILITY)?,
        };
        let module = cx.module(MODULE)?;
        module.function("listen", move |call: &Call<'_>, address: &str, options: Option<ValueView<'_>>| {
            listen(call, granted, address, options)
        })?;
        if granted.connect {
            module.function("connect", move |call: &Call<'_>, address: &str, options: Option<ValueView<'_>>| {
                connect(call, granted, address, options)
            })?;
        } else {
            module
                .function("connect", |_: ArgView<'_>| -> Result<()> { Err(denied("connect", CONNECT_CAPABILITY)) })?;
        }
        Ok(())
    }
}
