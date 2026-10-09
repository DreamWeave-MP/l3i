//! Asynchronous hostname resolution through the OS's resolver: the `dream.dns` extension,
//! module `@dream/dns`.
//!
//! ```lua
//! local dns = require('@dream/dns')
//! local request = assert(dns.resolve('example.com', 443, { timeoutMs = 5000 }))
//! request:wait(250)                          -- or watch it with a @dream/tcp poller
//! local addresses, message, kind = request:take()
//! -- addresses: { '[2606:2800:21f:cb07:6820:80da:af6b:8b2c]:443', '93.184.215.14:443' }
//! ```
//!
//! `resolve` returns at once. The lookup runs on one of a [`Resolver`]'s bounded worker
//! threads, through the OS (`getaddrinfo`): the hosts file, NSS, search domains, VPN and split
//! DNS apply as they do for every other program on the machine, and queries go wherever the OS
//! sends them. This is not DNS-over-HTTPS, caches nothing, and never talks to a resolver of its
//! own choosing.
//!
//! Results are endpoints `tcp.connect` accepts, in the OS's order, both families kept,
//! duplicates removed, at most `maxAddresses` of them; an empty answer is `notFound`. A request
//! is one-shot: `take` answers once, then raises. Resolving needs [`RESOLVE_CAPABILITY`],
//! which grants nothing else: connecting to what it returns is still `network.tcp.connect`'s.
//!
//! # Names
//!
//! The name and port are separate arguments; nothing parses `host:port`. A name is mapped to
//! ASCII as URLs map host names (UTS #46 nontransitional processing, WHATWG's forbidden host
//! code points refused, so `_` is allowed), then checked: at most 253 characters, labels of 1
//! to 63 letters, digits, `-` or `_`, one optional trailing dot. `Bücher.example` resolves as
//! `xn--bcher-kva.example`; `request.host` keeps what the caller wrote, which is the identity
//! a TLS session checks. An IPv4 or IPv6 literal (bracketed or not) completes at once with no
//! lookup.

mod request;
mod resolver;
#[cfg(feature = "tcp")]
mod watch;

#[cfg(feature = "tcp")]
pub(crate) use watch::watch_target;

pub use request::{MAX_WAIT_MS, Request};
pub use resolver::{Lookup, LookupError, Resolver, ResolverConfig, SystemLookup};

use std::cell::Cell;
use std::net::SocketAddr;
use std::rc::Rc;
use std::time::{Duration, Instant};

use crate::bind::{ArgView, Call, StackResults};
use crate::convert::Exact;
use crate::error::{Error, Result};
use crate::extension::{Extension, ExtensionDescriptor, InstallContext};
use crate::options::Options;
use crate::outcome::{Failure, Outcome};
use crate::stack::ValueView;

use resolver::{Phase, Refused, Shared};

/// The extension id.
pub const EXTENSION_ID: &str = "dream.dns";
/// The module path.
pub const MODULE: &str = "@dream/dns";
/// The capability `dns.resolve` needs.
pub const RESOLVE_CAPABILITY: &str = "network.dns.resolve";
/// The longest a request may take, in milliseconds.
pub const MAX_TIMEOUT_MS: u32 = 60_000;
/// The most addresses a request returns.
pub const MAX_ADDRESSES: u32 = 64;

const KIND_TYPE: &str = "\"wouldBlock\" | \"notFound\" | \"temporaryFailure\" | \"timedOut\" | \"cancelled\" \
    | \"limitReached\" | \"other\"";
const STATUS_TYPE: &str =
    "\"pending\" | \"ready\" | \"failed\" | \"cancelled\" | \"timedOut\" | \"consumed\" | \"closed\"";
const OPTIONS_TYPE: &str = "{ timeoutMs: number?, maxAddresses: number? }";

pub use crate::hostname::{Name, normalize};

/// What `dns.resolve` reads from its options.
struct ResolveOptions {
    timeout: Duration,
    max_addresses: usize,
}

fn read_options(call: &Call<'_>, options: Option<ValueView<'_>>) -> Result<ResolveOptions> {
    let mut resolved = ResolveOptions { timeout: Duration::from_secs(5), max_addresses: 16 };
    let Some(options) = options.filter(|view| !view.is_nil()) else { return Ok(resolved) };
    Options::read(call, options, "dream.dns.resolve", |o| {
        if let Some(ms) = o.optional::<Exact<i64>>("timeoutMs")? {
            let ms = u32::try_from(ms.0).ok().filter(|ms| (1..=MAX_TIMEOUT_MS).contains(ms)).ok_or_else(|| {
                Error::runtime(format!("dream.dns.resolve: timeoutMs must be in [1, {MAX_TIMEOUT_MS}], got {}", ms.0))
            })?;
            resolved.timeout = Duration::from_millis(u64::from(ms));
        }
        if let Some(count) = o.optional::<Exact<i64>>("maxAddresses")? {
            let count = u32::try_from(count.0).ok().filter(|n| (1..=MAX_ADDRESSES).contains(n)).ok_or_else(|| {
                Error::runtime(format!(
                    "dream.dns.resolve: maxAddresses must be in [1, {MAX_ADDRESSES}], got {}",
                    count.0
                ))
            })?;
            resolved.max_addresses = count as usize;
        }
        Ok(())
    })?;
    Ok(resolved)
}

/// What one runtime's `resolve` closes over.
struct Context {
    resolver: Resolver,
    active: Rc<Cell<u32>>,
    max_requests: u32,
}

fn resolve(
    call: &Call<'_>,
    cx: &Context,
    host: &str,
    port: Exact<i64>,
    options: Option<ValueView<'_>>,
) -> Result<Outcome<StackResults>> {
    let name = normalize(host).map_err(|message| Error::runtime(format!("dream.dns.resolve: {message}")))?;
    let port = u16::try_from(port.0)
        .ok()
        .filter(|port| *port != 0)
        .ok_or_else(|| Error::runtime(format!("dream.dns.resolve: port must be in [1, 65535], got {}", port.0)))?;
    let options = read_options(call, options)?;
    if cx.active.get() >= cx.max_requests {
        return Ok(Outcome::Failed(Failure {
            message: format!(
                "dream.dns.resolve: {} requests are open in this runtime, its maximum; take or close one",
                cx.max_requests
            )
            .into(),
            kind: "limitReached",
        }));
    }
    let deadline = Instant::now() + options.timeout;
    let (shared, pool) = match name.literal {
        Some(ip) => (Shared::new(Phase::Done(Ok(vec![SocketAddr::new(ip, port)])), options.max_addresses), None),
        None => {
            let shared = Shared::new(Phase::Queued, options.max_addresses);
            let pool = cx.resolver.pool();
            if let Err(refused) = pool.submit(name.ascii.clone(), port, std::sync::Arc::clone(&shared)) {
                let (message, kind) = match refused {
                    Refused::Full(queue) => (
                        format!(
                            "dream.dns.resolve: {}: the resolver's {queue} queued lookups are its maximum",
                            name.original
                        ),
                        "limitReached",
                    ),
                    Refused::Closed => {
                        (format!("dream.dns.resolve: {}: the resolver was shut down", name.original), "other")
                    }
                    Refused::NoThread => {
                        (format!("dream.dns.resolve: {}: no resolver thread could be started", name.original), "other")
                    }
                };
                return Ok(Outcome::Failed(Failure { message: message.into(), kind }));
            }
            (shared, Some(std::sync::Arc::clone(pool)))
        }
    };
    let request = Request::new(shared, pool, name.original, name.ascii, port, deadline, Rc::clone(&cx.active));
    crate::userdata::push_owned(call, request)?;
    Ok(Outcome::Done(StackResults))
}

/// The `dream.dns` extension over a [`Resolver`].
#[derive(Clone, Debug)]
pub struct DnsExtension {
    resolver: Resolver,
    max_requests: u32,
}

impl DnsExtension {
    /// Over `resolver`, with at most 64 open requests per runtime.
    pub fn new(resolver: Resolver) -> DnsExtension {
        DnsExtension { resolver, max_requests: 64 }
    }

    /// At most `max` requests open per runtime at once (taken, closed or collected ones do not
    /// count); 1 to 65536.
    #[must_use]
    pub fn max_requests(mut self, max: u32) -> DnsExtension {
        self.max_requests = max.clamp(1, 65_536);
        self
    }
}

impl Default for DnsExtension {
    /// Over [`Resolver::system`].
    fn default() -> Self {
        DnsExtension::new(Resolver::system())
    }
}

impl Extension for DnsExtension {
    fn id(&self) -> &'static str {
        EXTENSION_ID
    }

    fn describe(&self, d: &mut ExtensionDescriptor) -> Result<()> {
        d.type_alias("dream_dns_ErrorKind", KIND_TYPE);
        d.type_alias("dream_dns_Status", STATUS_TYPE);
        d.type_alias("dream_dns_ResolveOptions", OPTIONS_TYPE);
        d.optional_capability(RESOLVE_CAPABILITY);
        // With @dream/tcp in the plan, its pollers watch requests too.
        #[cfg(feature = "tcp")]
        d.optional(crate::tcp::EXTENSION_ID).widen_type_alias("dream_tcp_Watchable", "dream_dns_Request");
        request::describe_request(d);
        d.module(MODULE)
            .doc("Hostname resolution through the OS resolver on bounded worker threads; never blocks the script.")
            .installed("resolve")
            .signature("(host: string, port: number, options: dream_dns_ResolveOptions?) -> (dream_dns_Request?, string?, dream_dns_ErrorKind?)")
            .doc("Starts resolving host for port and returns the request at once; needs network.dns.resolve. timeoutMs defaults to 5000, maxAddresses to 16.")
            .constant("MAX_TIMEOUT_MS", crate::source::CompileConstant::Number(f64::from(MAX_TIMEOUT_MS)))
            .constant("MAX_ADDRESSES", crate::source::CompileConstant::Number(f64::from(MAX_ADDRESSES)));
        Ok(())
    }

    fn install(&self, cx: &mut InstallContext<'_>) -> Result<()> {
        let granted = cx.has_capability(RESOLVE_CAPABILITY)?;
        let module = cx.module(MODULE)?;
        if granted {
            let context = Rc::new(Context {
                resolver: self.resolver.clone(),
                active: Rc::new(Cell::new(0)),
                max_requests: self.max_requests,
            });
            module.function(
                "resolve",
                move |call: &Call<'_>, host: &str, port: Exact<i64>, options: Option<ValueView<'_>>| {
                    resolve(call, &context, host, port, options)
                },
            )?;
        } else {
            module.function("resolve", |_: ArgView<'_>| -> Result<()> {
                Err(Error::permission(format!(
                    "dream.dns.resolve: needs the '{RESOLVE_CAPABILITY}' capability, which this runtime does not grant"
                )))
            })?;
        }
        Ok(())
    }
}
