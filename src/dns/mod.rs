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

pub use request::{MAX_WAIT_MS, Request};
pub use resolver::{Lookup, LookupError, Resolver, ResolverConfig, SystemLookup};

use std::cell::Cell;
use std::net::{IpAddr, SocketAddr};
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

/// A name ready to resolve: what the caller wrote, its ASCII form, and the address when it is
/// a literal.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Name {
    /// As given.
    pub original: String,
    /// IDNA A-labels, lower case; an address literal's canonical spelling.
    pub ascii: String,
    /// The address, for a literal.
    pub literal: Option<IpAddr>,
}

/// Checks and maps a host name the way `dns.resolve` does; the message says what is wrong.
pub fn normalize(host: &str) -> std::result::Result<Name, String> {
    if host.is_empty() {
        return Err("the name is empty".to_owned());
    }
    if host.len() > 1024 {
        return Err(format!("the name is {} bytes, more than 1024", host.len()));
    }
    if host.contains('\0') {
        return Err("the name contains NUL".to_owned());
    }
    let unbracketed = host.strip_prefix('[').and_then(|inner| inner.strip_suffix(']'));
    if let Some(inner) = unbracketed {
        let ip: std::net::Ipv6Addr = inner.parse().map_err(|_| format!("'{host}' is not an IPv6 literal"))?;
        return Ok(Name { original: host.to_owned(), ascii: ip.to_string(), literal: Some(IpAddr::V6(ip)) });
    }
    if let Ok(ip) = host.parse::<IpAddr>() {
        return Ok(Name { original: host.to_owned(), ascii: ip.to_string(), literal: Some(ip) });
    }
    let ascii = idna::domain_to_ascii_cow(host.as_bytes(), idna::AsciiDenyList::URL)
        .map_err(|_| format!("'{host}' is not a valid host name (IDNA mapping failed)"))?
        .into_owned();
    let bare = ascii.strip_suffix('.').unwrap_or(&ascii);
    if bare.is_empty() {
        return Err(format!("'{host}' has no labels"));
    }
    if bare.len() > 253 {
        return Err(format!("'{host}' is {} characters as ASCII, more than 253", bare.len()));
    }
    for label in bare.split('.') {
        if label.is_empty() || label.len() > 63 {
            return Err(format!("'{host}' has a label of {} characters (1 to 63)", label.len()));
        }
        if let Some(bad) = label.chars().find(|c| !(c.is_ascii_alphanumeric() || *c == '-' || *c == '_')) {
            return Err(format!("'{host}' has a label with '{bad}' (letters, digits, '-' and '_' only)"));
        }
    }
    Ok(Name { original: host.to_owned(), ascii, literal: None })
}

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_map_to_ascii_and_bad_names_say_why() {
        let name = normalize("Bücher.Example.").unwrap();
        assert_eq!(
            (name.original.as_str(), name.ascii.as_str(), name.literal),
            ("Bücher.Example.", "xn--bcher-kva.example.", None)
        );
        assert_eq!(normalize("_srv.Internal-Host").unwrap().ascii, "_srv.internal-host");
        assert_eq!(normalize("127.0.0.1").unwrap().literal, Some("127.0.0.1".parse().unwrap()));
        assert_eq!(normalize("[::1]").unwrap().literal, Some("::1".parse().unwrap()));
        assert_eq!(normalize("::1").unwrap().ascii, "::1");
        for (bad, why) in [
            ("", "empty"),
            ("a\0b", "NUL"),
            ("[127.0.0.1]", "IPv6 literal"),
            ("exa mple.com", "IDNA"),
            ("a..b", "label of 0"),
            (".", "no labels"),
            ("example.com:443", "IDNA"),
            ("a!b.com", "'!'"),
        ] {
            let error = normalize(bad).unwrap_err();
            assert!(error.contains(why), "{bad:?}: {error}");
        }
        let long_label = format!("{}.com", "a".repeat(64));
        assert!(normalize(&long_label).unwrap_err().contains("64 characters"));
        let long_name = vec!["abcdefghi"; 26].join(".");
        assert!(normalize(&long_name).unwrap_err().contains("more than 253"));
        assert!(normalize(&"a".repeat(1025)).unwrap_err().contains("1024"));
    }
}
