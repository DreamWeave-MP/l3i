//! `dream.dns.Request`: one lookup, its deadline, and the one-shot result.
//!
//! The request's phase lives in [`Shared`], which its worker also holds; everything else here
//! stays on the runtime's thread. A deadline is the script's: the request reads as timed out
//! the moment it passes, whether or not the OS lookup behind it has returned.

use std::borrow::Cow;
use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::bind::{Call, StackResults};
use crate::error::{Error, Result};
use crate::extension::{ExtensionDescriptor, TagPolicy};
use crate::outcome::{Failure, Outcome};
use crate::stack::{Frame, Scope};
use crate::userdata::Userdata;

use super::resolver::{Phase, Pool, Shared};

/// The longest `Request:wait`, in milliseconds.
pub const MAX_WAIT_MS: u32 = 60_000;

/// `dream.dns.Request`.
pub struct Request {
    pub(crate) shared: Arc<Shared>,
    /// The pool a queued lookup waits in, to withdraw it; none for an address literal.
    pool: Option<Arc<Pool>>,
    host: String,
    ascii: String,
    port: u16,
    pub(crate) deadline: Instant,
    /// Whether the script took the result: a taken request is never reported ready again.
    pub(crate) taken: Rc<Cell<bool>>,
    /// The runtime's count of live requests, released once.
    active: RefCell<Option<Rc<Cell<u32>>>>,
}

// SAFETY: plain Rust state with no Lua references; dropping it cancels a lookup and releases a
// watch without touching the Lua API.
unsafe impl Userdata for Request {
    const NAME: &'static str = "dream.dns.Request";
}

impl Request {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        shared: Arc<Shared>,
        pool: Option<Arc<Pool>>,
        host: String,
        ascii: String,
        port: u16,
        deadline: Instant,
        active: Rc<Cell<u32>>,
    ) -> Request {
        active.set(active.get() + 1);
        Request {
            shared,
            pool,
            host,
            ascii,
            port,
            deadline,
            taken: Rc::new(Cell::new(false)),
            active: RefCell::new(Some(active)),
        }
    }

    fn release(&self) {
        if let Some(active) = self.active.borrow_mut().take() {
            active.set(active.get().saturating_sub(1));
        }
    }

    fn withdraw(&self) {
        if let Some(pool) = &self.pool {
            pool.withdraw(&self.shared);
        }
    }

    /// Enters the timed-out phase once the deadline has passed.
    pub(crate) fn expire(&self, now: Instant) {
        if !self.shared.is_terminal() && now >= self.deadline && self.shared.finish(Phase::TimedOut) {
            self.withdraw();
        }
    }

    /// `pending`, `ready`, `failed`, `cancelled`, `timedOut`, `consumed` or `closed`.
    pub fn status(&self) -> &'static str {
        self.expire(Instant::now());
        match &self.shared.lock().phase {
            Phase::Queued | Phase::Running => "pending",
            Phase::Done(Ok(_)) => "ready",
            Phase::Done(Err(_)) => "failed",
            Phase::Cancelled => "cancelled",
            Phase::TimedOut => "timedOut",
            Phase::Consumed => "consumed",
            Phase::Closed => "closed",
        }
    }

    fn take(&self, call: &Call<'_>) -> Result<Outcome<StackResults>> {
        self.expire(Instant::now());
        let phase = {
            let mut inner = self.shared.lock();
            match inner.phase {
                Phase::Queued | Phase::Running => {
                    return Ok(Outcome::Failed(Failure {
                        message: Cow::Borrowed("dream.dns.Request.take: the lookup is still pending"),
                        kind: "wouldBlock",
                    }));
                }
                Phase::Consumed => {
                    return Err(Error::runtime("dream.dns.Request.take: the result was already taken"));
                }
                Phase::Closed => return Err(Error::runtime("dream.dns.Request.take: the request is closed")),
                _ => std::mem::replace(&mut inner.phase, Phase::Consumed),
            }
        };
        self.taken.set(true);
        self.release();
        let failed =
            |message: String, kind: &'static str| Ok(Outcome::Failed(Failure { message: Cow::Owned(message), kind }));
        match phase {
            Phase::Done(Ok(addresses)) => {
                let mut frame: Frame<'_> = call.frame();
                let table = frame.push_table(addresses.len(), 0)?;
                for (index, address) in addresses.iter().enumerate() {
                    frame.push(address.to_string().as_str())?;
                    table.raw_set_index(&frame, index as i64 + 1)?;
                }
                frame.release();
                Ok(Outcome::Done(StackResults))
            }
            Phase::Done(Err(error)) => failed(format!("dream.dns.resolve: {}: {error}", self.host), error.kind()),
            Phase::Cancelled => failed(format!("dream.dns.resolve: {}: cancelled", self.host), "cancelled"),
            Phase::TimedOut => failed(format!("dream.dns.resolve: {}: timed out", self.host), "timedOut"),
            Phase::Queued | Phase::Running | Phase::Consumed | Phase::Closed => unreachable!("handled above"),
        }
    }

    fn wait(&self, call: &Call<'_>, timeout_ms: f64) -> Result<bool> {
        if !(0.0..=f64::from(MAX_WAIT_MS)).contains(&timeout_ms) {
            return Err(Error::runtime(format!(
                "dream.dns.Request.wait: timeout must be in [0, {MAX_WAIT_MS}] milliseconds, got {timeout_ms}"
            )));
        }
        let mut budget = Duration::from_secs_f64(timeout_ms / 1000.0);
        if let Some(remaining) = crate::runtime::watchdog_remaining(call) {
            budget = budget.min(remaining);
        }
        let until = (Instant::now() + budget).min(self.deadline);
        self.shared.wait_until(until);
        self.expire(Instant::now());
        Ok(self.shared.is_terminal())
    }

    /// Stops waiting for a pending lookup: a queued one is withdrawn, a running one's result
    /// will be thrown away. `take` then answers `cancelled`.
    pub fn cancel(&self) {
        if self.shared.finish(Phase::Cancelled) {
            self.withdraw();
        }
    }

    /// Cancels, drops any result, releases the request's watch and its place in the runtime's
    /// count. Idempotent.
    pub fn close(&self) {
        self.cancel();
        self.shared.lock().phase = Phase::Closed;
        self.taken.set(true);
        self.release();
    }

    fn is_closed(&self) -> bool {
        matches!(self.shared.lock().phase, Phase::Closed)
    }
}

impl Drop for Request {
    fn drop(&mut self) {
        self.cancel();
        self.release();
    }
}

pub(crate) fn describe_request(d: &mut ExtensionDescriptor) {
    let mut request = d.userdata::<Request>(Request::NAME);
    request.tag(TagPolicy::Preferred).doc("One hostname lookup on the resolver's workers; its result is taken once.");
    request
        .method("take", |r: &Request, call: &Call<'_>| r.take(call))
        .signature("(self): ({ string }?, string?, dream_dns_ErrorKind?)")
        .doc("The addresses (\"93.184.215.14:443\", \"[2606:2800::1]:443\"), once; nil, a message and 'wouldBlock' while pending, or the lookup's failure. Taking twice is an error.");
    request
        .method("wait", |r: &Request, call: &Call<'_>, timeout: f64| r.wait(call, timeout))
        .signature("(self, timeoutMs: number): boolean")
        .doc("Waits at most timeoutMs (and never past the request's deadline or the runtime's execution time limit) for the lookup to finish; true once it has. The lookup runs on a worker, not here.");
    request
        .method("cancel", |r: &Request| r.cancel())
        .signature("(self)")
        .doc("Gives up on a pending lookup; take answers 'cancelled'.");
    request
        .method("close", |r: &Request| r.close())
        .signature("(self)")
        .doc("Cancels, drops the result and any watch. Closing twice does nothing.");
    request.getter("status", |r: &Request| r.status()).signature("dream_dns_Status");
    request.getter("host", |r: &Request| r.host.clone()).signature("string").doc("The name as given.");
    request
        .getter("asciiHost", |r: &Request| r.ascii.clone())
        .signature("string")
        .doc("The name resolved: IDNA A-labels, lower case.");
    request.getter("port", |r: &Request| f64::from(r.port)).signature("number");
    request.getter("closed", |r: &Request| r.is_closed()).signature("boolean");
    request
        .metamethod("__tostring", |r: &Request| format!("dream.dns.Request({}:{}, {})", r.ascii, r.port, r.status()));
}
