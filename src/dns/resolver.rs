//! The bounded resolver: worker threads that run the OS's blocking lookup, a bounded queue in
//! front of them, and the completion state a request shares with the worker that serves it.
//!
//! Workers own only plain Rust data: the name, the port and an `Arc` of the request's
//! [`Shared`] state. No Luau value, no runtime `Rc` and no callback crosses to them. A lookup
//! the OS is running cannot be interrupted, so cancelling or timing out a running request only
//! decides that its result is thrown away; the worker stays busy until the OS returns, and the
//! pool's size bounds how many such calls run at once.

use std::collections::VecDeque;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, OnceLock, Weak};
use std::time::Instant;

/// Why a lookup found no address.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LookupError {
    /// The name does not exist, or has no address records (`EAI_NONAME`, `EAI_NODATA`).
    NotFound,
    /// The resolver could not answer now; trying again later may work (`EAI_AGAIN`).
    TemporaryFailure,
    /// Anything else, with the OS's description.
    Other(String),
}

impl LookupError {
    /// The kind a script sees.
    pub fn kind(&self) -> &'static str {
        match self {
            LookupError::NotFound => "notFound",
            LookupError::TemporaryFailure => "temporaryFailure",
            LookupError::Other(_) => "other",
        }
    }
}

impl std::fmt::Display for LookupError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LookupError::NotFound => f.write_str("the name has no address"),
            LookupError::TemporaryFailure => f.write_str("the resolver failed temporarily"),
            LookupError::Other(message) => f.write_str(message),
        }
    }
}

/// A blocking name lookup, run only on the resolver's worker threads. [`SystemLookup`] is the
/// OS's; tests and hosts with their own naming implement it.
pub trait Lookup: Send + Sync + 'static {
    /// The addresses of `host` (an ASCII name, already validated) with `port`, in the order
    /// they should be tried.
    fn lookup(&self, host: &str, port: u16) -> Result<Vec<SocketAddr>, LookupError>;
}

/// The operating system's resolver: `getaddrinfo` with no address family and stream sockets,
/// so the hosts file, NSS, search domains, VPN and split DNS all apply, in the OS's order.
#[derive(Clone, Copy, Debug, Default)]
pub struct SystemLookup;

#[cfg(unix)]
impl Lookup for SystemLookup {
    fn lookup(&self, host: &str, port: u16) -> Result<Vec<SocketAddr>, LookupError> {
        use std::net::{Ipv4Addr, Ipv6Addr, SocketAddrV4, SocketAddrV6};
        let name = std::ffi::CString::new(host).map_err(|_| LookupError::Other("the name contains NUL".to_owned()))?;
        // SAFETY: a zeroed addrinfo is the documented "no hints" value; the fields set are the
        // ones getaddrinfo reads.
        let mut hints: libc::addrinfo = unsafe { std::mem::zeroed() };
        hints.ai_family = libc::AF_UNSPEC;
        hints.ai_socktype = libc::SOCK_STREAM;
        hints.ai_protocol = libc::IPPROTO_TCP;
        let mut list: *mut libc::addrinfo = std::ptr::null_mut();
        // SAFETY: `name` is NUL-terminated and outlives the call; the service is null (the port
        // is set below); on success `list` owns a list freed exactly once below.
        let status = unsafe { libc::getaddrinfo(name.as_ptr(), std::ptr::null(), &raw const hints, &raw mut list) };
        if status != 0 {
            let error = std::io::Error::last_os_error();
            return Err(match status {
                libc::EAI_NONAME => LookupError::NotFound,
                #[cfg(any(target_os = "linux", target_os = "android"))]
                libc::EAI_NODATA => LookupError::NotFound,
                libc::EAI_AGAIN => LookupError::TemporaryFailure,
                libc::EAI_SYSTEM => LookupError::Other(error.to_string()),
                _ => {
                    // SAFETY: gai_strerror returns a static NUL-terminated string.
                    let text = unsafe { std::ffi::CStr::from_ptr(libc::gai_strerror(status)) };
                    LookupError::Other(text.to_string_lossy().into_owned())
                }
            });
        }
        let mut addresses = Vec::new();
        let mut entry = list;
        while !entry.is_null() {
            // SAFETY: `entry` is a node of the list getaddrinfo returned, not yet freed; its
            // address is a sockaddr of the family it names, at least `ai_addrlen` bytes.
            unsafe {
                let info = &*entry;
                match info.ai_family {
                    libc::AF_INET if !info.ai_addr.is_null() => {
                        let v4 = &*info.ai_addr.cast::<libc::sockaddr_in>();
                        let ip = Ipv4Addr::from(u32::from_be(v4.sin_addr.s_addr));
                        addresses.push(SocketAddr::V4(SocketAddrV4::new(ip, port)));
                    }
                    libc::AF_INET6 if !info.ai_addr.is_null() => {
                        let v6 = &*info.ai_addr.cast::<libc::sockaddr_in6>();
                        let ip = Ipv6Addr::from(v6.sin6_addr.s6_addr);
                        addresses.push(SocketAddr::V6(SocketAddrV6::new(ip, port, v6.sin6_flowinfo, v6.sin6_scope_id)));
                    }
                    _ => {}
                }
                entry = info.ai_next;
            }
        }
        // SAFETY: `list` came from a successful getaddrinfo and is freed once.
        unsafe { libc::freeaddrinfo(list) };
        Ok(addresses)
    }
}

#[cfg(not(unix))]
impl Lookup for SystemLookup {
    fn lookup(&self, host: &str, port: u16) -> Result<Vec<SocketAddr>, LookupError> {
        use std::net::ToSocketAddrs;
        match (host, port).to_socket_addrs() {
            Ok(addresses) => Ok(addresses.collect()),
            // WSAHOST_NOT_FOUND, WSANO_DATA, WSATRY_AGAIN.
            Err(error) => Err(match error.raw_os_error() {
                Some(11001 | 11004) => LookupError::NotFound,
                Some(11002) => LookupError::TemporaryFailure,
                _ => LookupError::Other(error.to_string()),
            }),
        }
    }
}

/// How many lookups a resolver runs at once and how many may wait.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ResolverConfig {
    /// Worker threads, started as lookups need them and never more: 1 to 64.
    pub workers: usize,
    /// Lookups waiting for a worker before a request is refused: 1 to 4096.
    pub queue: usize,
}

impl Default for ResolverConfig {
    fn default() -> Self {
        ResolverConfig { workers: 4, queue: 64 }
    }
}

/// Where a request is. Exactly one of the terminal phases is entered, once, under the lock.
#[derive(Debug)]
pub(crate) enum Phase {
    Queued,
    Running,
    Done(Result<Vec<SocketAddr>, LookupError>),
    Cancelled,
    TimedOut,
    /// The script took the result.
    Consumed,
    Closed,
}

/// What a waiting poller needs to be woken.
pub(crate) trait Wake: Send + Sync {
    fn wake(&self);
}

#[cfg(feature = "tcp")]
impl Wake for mio::Waker {
    fn wake(&self) {
        // A failed wake means the poller is gone; nothing waits on it.
        let _ = mio::Waker::wake(self);
    }
}

pub(crate) struct Inner {
    pub(crate) phase: Phase,
    /// The poller watching the request, if any.
    pub(crate) waker: Option<Arc<dyn Wake>>,
}

/// A request's state, shared by the runtime thread and the worker serving it.
pub(crate) struct Shared {
    inner: Mutex<Inner>,
    finished: Condvar,
    /// Set with the first terminal phase: a scan reads it without the lock.
    terminal: AtomicBool,
    /// Every addresses list is cut to this many entries, duplicates removed first.
    max_addresses: usize,
}

impl Shared {
    pub(crate) fn new(phase: Phase, max_addresses: usize) -> Arc<Shared> {
        let terminal = !matches!(phase, Phase::Queued | Phase::Running);
        Arc::new(Shared {
            inner: Mutex::new(Inner { phase, waker: None }),
            finished: Condvar::new(),
            terminal: AtomicBool::new(terminal),
            max_addresses,
        })
    }

    /// The lock, whatever a panicking holder left: every phase change is one assignment.
    pub(crate) fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    pub(crate) fn is_terminal(&self) -> bool {
        self.terminal.load(Ordering::Acquire)
    }

    /// Enters `phase` if the request is still pending; true when it did. Wakes a waiting
    /// script and a watching poller.
    pub(crate) fn finish(&self, phase: Phase) -> bool {
        let waker = {
            let mut inner = self.lock();
            if !matches!(inner.phase, Phase::Queued | Phase::Running) {
                return false;
            }
            inner.phase = phase;
            self.terminal.store(true, Ordering::Release);
            inner.waker.clone()
        };
        self.finished.notify_all();
        if let Some(waker) = waker {
            waker.wake();
        }
        true
    }

    /// The worker's half: a result for a request still pending, else thrown away.
    fn complete(&self, result: Result<Vec<SocketAddr>, LookupError>) {
        let result = result.and_then(|mut addresses| {
            let mut seen = std::collections::HashSet::new();
            addresses.retain(|address| seen.insert(*address));
            addresses.truncate(self.max_addresses);
            if addresses.is_empty() { Err(LookupError::NotFound) } else { Ok(addresses) }
        });
        self.finish(Phase::Done(result));
    }

    /// Waits up to `until` for a terminal phase; true when one was reached.
    pub(crate) fn wait_until(&self, until: Instant) -> bool {
        let mut inner = self.lock();
        loop {
            if !matches!(inner.phase, Phase::Queued | Phase::Running) {
                return true;
            }
            let now = Instant::now();
            if now >= until {
                return false;
            }
            inner = self.finished.wait_timeout(inner, until - now).unwrap_or_else(std::sync::PoisonError::into_inner).0;
        }
    }
}

struct Job {
    host: String,
    port: u16,
    shared: Arc<Shared>,
}

struct State {
    jobs: VecDeque<Job>,
    idle: usize,
    spawned: usize,
    closed: bool,
}

/// The queue and its workers' bookkeeping.
pub(crate) struct Pool {
    state: Mutex<State>,
    work: Condvar,
    lookup: Arc<dyn Lookup>,
    config: ResolverConfig,
}

/// Why a lookup could not be queued.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Refused {
    Full(usize),
    Closed,
    NoThread,
}

impl Pool {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Queues a lookup, starting a worker when none is idle and the pool is below its size.
    pub(crate) fn submit(self: &Arc<Pool>, host: String, port: u16, shared: Arc<Shared>) -> Result<(), Refused> {
        let spawn = {
            let mut state = self.lock();
            if state.closed {
                return Err(Refused::Closed);
            }
            if state.jobs.len() >= self.config.queue {
                return Err(Refused::Full(self.config.queue));
            }
            state.jobs.push_back(Job { host, port, shared });
            if state.idle > 0 {
                self.work.notify_one();
            }
            // Another thread only while queued lookups outnumber idle workers.
            let spawn = state.idle < state.jobs.len() && state.spawned < self.config.workers;
            if spawn {
                state.spawned += 1;
            }
            spawn
        };
        if spawn {
            let pool = Arc::downgrade(self);
            let started = std::thread::Builder::new().name("l3i-dns".to_owned()).spawn(move || work(&pool));
            if started.is_err() {
                let mut state = self.lock();
                state.spawned -= 1;
                if state.spawned == 0 {
                    // Nothing will ever serve the queue: take this job back.
                    state.jobs.pop_back();
                    return Err(Refused::NoThread);
                }
            }
        }
        Ok(())
    }

    /// Removes a lookup no worker has started.
    pub(crate) fn withdraw(&self, shared: &Arc<Shared>) {
        self.lock().jobs.retain(|job| !Arc::ptr_eq(&job.shared, shared));
    }

    /// Lookups waiting for a worker, and workers started.
    pub(crate) fn load(&self) -> (usize, usize) {
        let state = self.lock();
        (state.jobs.len(), state.spawned)
    }
}

/// A worker: takes jobs until its pool is gone. It holds the pool weakly between jobs, so
/// dropping the last [`Resolver`] lets the workers end.
fn work(pool: &Weak<Pool>) {
    loop {
        let Some(strong) = pool.upgrade() else { return };
        let job = {
            let mut state = strong.lock();
            loop {
                if state.closed {
                    state.spawned -= 1;
                    return;
                }
                if let Some(job) = state.jobs.pop_front() {
                    break job;
                }
                state.idle += 1;
                state = strong.work.wait(state).unwrap_or_else(std::sync::PoisonError::into_inner);
                state.idle -= 1;
            }
        };
        let lookup = Arc::clone(&strong.lookup);
        drop(strong);
        {
            let mut inner = job.shared.lock();
            if !matches!(inner.phase, Phase::Queued) {
                continue;
            }
            inner.phase = Phase::Running;
        }
        // A panicking lookup must not take the worker with it; no Luau is on this thread.
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| lookup.lookup(&job.host, job.port)))
            .unwrap_or_else(|_| Err(LookupError::Other("the lookup panicked".to_owned())));
        job.shared.complete(result);
    }
}

/// A resolver: a bounded pool of lookup threads. Cheap to clone; runtimes may share one.
#[derive(Clone)]
pub struct Resolver {
    pool: Arc<Pool>,
    _closer: Arc<Closer>,
}

/// Closes the pool when the last resolver handle goes; requests keep only the pool.
struct Closer(Arc<Pool>);

impl Drop for Closer {
    fn drop(&mut self) {
        let jobs = {
            let mut state = self.0.lock();
            state.closed = true;
            std::mem::take(&mut state.jobs)
        };
        self.0.work.notify_all();
        for job in jobs {
            job.shared.finish(Phase::Done(Err(LookupError::Other("the resolver was shut down".to_owned()))));
        }
    }
}

impl Resolver {
    /// A resolver running `lookup` on at most `config.workers` threads (clamped to 1..=64) with
    /// at most `config.queue` waiting lookups (clamped to 1..=4096).
    pub fn new(config: ResolverConfig, lookup: Arc<dyn Lookup>) -> Resolver {
        let config = ResolverConfig { workers: config.workers.clamp(1, 64), queue: config.queue.clamp(1, 4096) };
        let pool = Arc::new(Pool {
            state: Mutex::new(State { jobs: VecDeque::new(), idle: 0, spawned: 0, closed: false }),
            work: Condvar::new(),
            lookup,
            config,
        });
        Resolver { _closer: Arc::new(Closer(Arc::clone(&pool))), pool }
    }

    /// The process's shared resolver over [`SystemLookup`] with the default configuration,
    /// made on first use. Its threads start only when lookups need them.
    pub fn system() -> Resolver {
        static SYSTEM: OnceLock<Resolver> = OnceLock::new();
        SYSTEM.get_or_init(|| Resolver::new(ResolverConfig::default(), Arc::new(SystemLookup))).clone()
    }

    /// Its configuration, clamped.
    pub fn config(&self) -> ResolverConfig {
        self.pool.config
    }

    /// Lookups waiting for a worker, and worker threads started.
    pub fn load(&self) -> (usize, usize) {
        self.pool.load()
    }

    pub(crate) fn pool(&self) -> &Arc<Pool> {
        &self.pool
    }
}

impl std::fmt::Debug for Resolver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Resolver").field("config", &self.pool.config).finish_non_exhaustive()
    }
}
