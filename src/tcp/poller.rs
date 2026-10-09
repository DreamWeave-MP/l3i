//! `dream.tcp.Poller`: bounded readiness waits over watched sources.
//!
//! The OS reports edges (mio's contract on every platform); the poller keeps the last readiness
//! each handle was told in a cell it shares with the handle, and the handle clears a bit only
//! when an operation reports would-block. A wait therefore reports every handle that may still
//! make progress, which is level-triggered readiness, and it never blocks while one is pending.
//!
//! A source is anything [`Watchable`]: a socket (a TCP listener or stream, or the socket under a
//! TLS stream) or a source with no socket that wakes the poller through its [`mio::Waker`] (a
//! DNS request completing on a worker thread). A source may add [`Synthetic`] readiness the OS
//! cannot see (plaintext a TLS session already decrypted, a request that completed) and a
//! deadline at which it becomes reportable with no event at all; a wait never sleeps past the
//! earliest one.
//!
//! Watches live in slots; a slot's mio token carries its index and a generation, and the queue
//! of events `next()` hands out keeps both, so an event never reaches a watch that replaced the
//! one it was raised for. The waker's token is reserved and never a slot's. Slots hold their
//! source weakly and the source holds the poller weakly.

use std::borrow::Cow;
use std::cell::{Cell, RefCell};
use std::collections::{HashMap, VecDeque};
use std::io;
use std::rc::{Rc, Weak};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::bind::Call;
use crate::convert::Exact;
use crate::error::{Error, Result};
use crate::extension::{ExtensionDescriptor, TagPolicy};
use crate::outcome::{Failure, Outcome};
use crate::stack::ValueView;
use crate::userdata::Userdata;

use super::socket::{CLOSED, Listener, READABLE, Stream, WRITABLE};

/// Interest bits.
pub(crate) const READ: u8 = 1;
pub(crate) const WRITE: u8 = 2;

/// The largest token: tokens are whole numbers a double holds exactly.
const MAX_TOKEN: i64 = 1 << 53;

/// Half of a mio token is the slot index, half its generation.
const INDEX_BITS: u32 = usize::BITS / 2;
const INDEX_MASK: usize = (1 << INDEX_BITS) - 1;

/// The waker's token; [`Slots::mio_token`] never hands it to a slot.
const WAKE_TOKEN: mio::Token = mio::Token(usize::MAX);

/// The readiness bits an interest makes reportable: `closed` goes with reading.
pub(crate) fn mask(interest: u8) -> u8 {
    let mut mask = 0;
    if interest & READ != 0 {
        mask |= READABLE | CLOSED;
    }
    if interest & WRITE != 0 {
        mask |= WRITABLE;
    }
    mask
}

/// Which poller slot watches a source.
pub(crate) struct Watch {
    pub(crate) poller: Weak<Core>,
    pub(crate) slot: u32,
}

/// Something a poller can watch.
pub(crate) trait Watchable {
    /// Where the source records its watch, so that closing it releases the slot.
    fn watch_cell(&self) -> &RefCell<Option<Watch>>;
    /// Registers with `core` under `token`: a socket with the OS, a socketless source by taking
    /// the poller's waker.
    fn register(&self, core: &Core, token: mio::Token) -> io::Result<()>;
    /// Undoes [`Self::register`]; a source that is already closed has nothing to undo.
    fn deregister(&self, core: &Core);
}

/// Readiness a source knows of that the OS does not report.
pub(crate) trait Synthetic {
    /// The bits to report for `interest`, given the readiness the OS last reported (`os`).
    fn reportable(&self, os: u8, interest: u8, now: Instant) -> u8;
    /// When the source becomes reportable without any event, while that is still to come.
    /// Must return `None` once reaching the deadline would report nothing, so a wait never
    /// returns early for it.
    fn deadline(&self) -> Option<Instant>;
}

/// What `watch` registers: the source, the readiness cell it shares, and what it may report.
pub(crate) struct Target {
    pub(crate) source: Rc<dyn Watchable>,
    pub(crate) ready: Rc<Cell<u8>>,
    /// Only ever readable (a listener, a DNS request): watching it for writing is an error.
    pub(crate) read_only: bool,
    /// Held strongly by the slot, so it must not own the source: a collected handle has to be
    /// able to die while watched.
    pub(crate) synthetic: Option<Rc<dyn Synthetic>>,
}

/// What a poller may hold and hand out.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Limits {
    pub(crate) events: u32,
    pub(crate) watches: u32,
    pub(crate) wait_ms: u32,
}

impl Default for Limits {
    fn default() -> Self {
        Limits { events: 256, watches: 1024, wait_ms: 1000 }
    }
}

/// One watch.
struct Slot {
    used: bool,
    generation: u32,
    token: i64,
    interest: u8,
    read_only: bool,
    ready: Rc<Cell<u8>>,
    source: Weak<dyn Watchable>,
    synthetic: Option<Rc<dyn Synthetic>>,
}

impl Slot {
    fn empty() -> Slot {
        let source: Weak<dyn Watchable> = Weak::<super::socket::Io>::new();
        Slot {
            used: false,
            generation: 0,
            token: 0,
            interest: 0,
            read_only: false,
            ready: Rc::new(Cell::new(0)),
            source,
            synthetic: None,
        }
    }

    /// The bits this watch reports now.
    fn reportable(&self, now: Instant) -> u8 {
        let os = self.ready.get();
        match &self.synthetic {
            Some(synthetic) => synthetic.reportable(os, self.interest, now),
            None => os & mask(self.interest),
        }
    }

    /// Whether this slot's source may make progress. A slot whose source is gone (collected
    /// while the poller was busy) is never ready.
    fn is_ready(&self, now: Instant) -> bool {
        self.used && self.source.strong_count() != 0 && self.reportable(now) != 0
    }
}

#[derive(Default)]
struct Slots {
    entries: Vec<Slot>,
    free: Vec<u32>,
    by_token: HashMap<i64, u32>,
    /// `(index, generation)` of the events `next()` has still to hand out.
    queue: VecDeque<(u32, u32)>,
    /// Where the next wait starts its scan, so a long run of ready handles cannot starve the rest.
    cursor: usize,
}

impl Slots {
    fn mio_token(index: u32, generation: u32) -> mio::Token {
        mio::Token(((generation as usize) << INDEX_BITS) | index as usize)
    }

    fn find(&self, what: &str, token: i64) -> Result<u32> {
        self.by_token
            .get(&token)
            .copied()
            .ok_or_else(|| Error::runtime(format!("dream.tcp.Poller.{what}: token {token} is not watching a handle")))
    }

    fn free(&mut self, index: u32) {
        let slot = &mut self.entries[index as usize];
        if !slot.used {
            return;
        }
        self.by_token.remove(&slot.token);
        let generation = slot.generation.wrapping_add(1);
        *slot = Slot::empty();
        slot.generation = generation;
        self.free.push(index);
    }

    /// Frees the slots whose sources were collected while the poller could not be told.
    fn sweep(&mut self) {
        let gone: Vec<u32> = (0..self.entries.len() as u32)
            .filter(|&index| {
                let slot = &self.entries[index as usize];
                slot.used && slot.source.strong_count() == 0
            })
            .collect();
        for index in gone {
            self.free(index);
        }
    }

    /// Records what the OS reported for each live watch. The waker's event carries no
    /// readiness: it only ends the OS wait, and the scan that follows sees what changed.
    fn apply(&self, events: &mio::Events) {
        for event in events {
            if event.token() == WAKE_TOKEN {
                continue;
            }
            let token = event.token().0;
            let index = token & INDEX_MASK;
            let generation = token >> INDEX_BITS;
            let Some(slot) = self.entries.get(index) else { continue };
            if !slot.used || (slot.generation as usize) & INDEX_MASK != generation {
                continue;
            }
            let mut bits = 0;
            if event.is_readable() || event.is_read_closed() {
                bits |= READABLE;
            }
            if event.is_writable() || event.is_write_closed() {
                bits |= WRITABLE;
            }
            if event.is_read_closed() {
                bits |= CLOSED;
            }
            if event.is_error() {
                // The next operation reports the error, whichever way the script looks.
                bits |= READABLE | WRITABLE | CLOSED;
            }
            slot.ready.set(slot.ready.get() | bits);
        }
    }

    /// Whether anything is reportable now, and the earliest synthetic deadline still to come.
    fn survey(&self, now: Instant) -> (bool, Option<Instant>) {
        let mut earliest: Option<Instant> = None;
        for slot in &self.entries {
            if !slot.used || slot.source.strong_count() == 0 {
                continue;
            }
            if slot.reportable(now) != 0 {
                return (true, None);
            }
            if let Some(deadline) = slot.synthetic.as_ref().and_then(|synthetic| synthetic.deadline()) {
                earliest = Some(earliest.map_or(deadline, |earliest| earliest.min(deadline)));
            }
        }
        (false, earliest)
    }

    /// Queues up to `max` ready watches, scanning once round from the cursor.
    fn fill(&mut self, max: usize, now: Instant) -> usize {
        self.queue.clear();
        let count = self.entries.len();
        if count == 0 {
            return 0;
        }
        let start = self.cursor % count;
        for step in 0..count {
            let index = (start + step) % count;
            let slot = &self.entries[index];
            if slot.is_ready(now) {
                self.queue.push_back((index as u32, slot.generation));
                if self.queue.len() == max {
                    self.cursor = index + 1;
                    return max;
                }
            }
        }
        self.queue.len()
    }
}

/// The poller's state, shared weakly with the sources it watches.
pub(crate) struct Core {
    poll: RefCell<Option<mio::Poll>>,
    /// A second handle on the poll's registry, so a source closing while the poller waits (it
    /// cannot: no Lua runs inside a wait) or after it closed never needs the poll itself.
    registry: mio::Registry,
    /// Made on the first socketless watch; mio allows one per poll.
    waker: RefCell<Option<Arc<mio::Waker>>>,
    events: RefCell<mio::Events>,
    slots: RefCell<Slots>,
    /// A source released its watch while the slots were borrowed.
    deferred: Cell<bool>,
    limits: Limits,
}

impl Core {
    /// The OS registry sockets register with.
    pub(crate) fn registry(&self) -> &mio::Registry {
        &self.registry
    }

    /// The waker a worker thread uses to end this poller's wait. It holds no reference to the
    /// poller's own state, which never leaves its thread.
    #[cfg_attr(not(any(test, feature = "dns")), allow(dead_code, reason = "socketless sources (dream.dns) take it"))]
    pub(crate) fn waker(&self) -> io::Result<Arc<mio::Waker>> {
        let mut waker = self.waker.borrow_mut();
        if let Some(waker) = waker.as_ref() {
            return Ok(Arc::clone(waker));
        }
        let made = Arc::new(mio::Waker::new(&self.registry, WAKE_TOKEN)?);
        *waker = Some(Arc::clone(&made));
        Ok(made)
    }

    /// Drops the watch in `slot` of a source that is closing and has deregistered itself.
    pub(crate) fn release(&self, slot: u32) {
        // Busy only if a source is collected during a poller call; the next wait sweeps it.
        match self.slots.try_borrow_mut() {
            Ok(mut slots) => slots.free(slot),
            Err(_) => self.deferred.set(true),
        }
    }

    /// Frees the watches of sources collected while the slots were busy.
    fn sweep(&self, slots: &mut Slots) {
        if self.deferred.replace(false) {
            slots.sweep();
        }
    }

    /// Releases every watch: the sources stay open and may be watched again.
    fn release_all(&self) {
        let mut slots = self.slots.borrow_mut();
        for index in 0..slots.entries.len() as u32 {
            let slot = &slots.entries[index as usize];
            if !slot.used {
                continue;
            }
            if let Some(source) = slot.source.upgrade() {
                source.watch_cell().borrow_mut().take();
                source.deregister(self);
            }
            slots.free(index);
        }
        slots.queue.clear();
    }
}

impl Drop for Core {
    fn drop(&mut self) {
        self.release_all();
    }
}

/// `dream.tcp.Poller`.
pub struct Poller {
    core: Rc<Core>,
}

// SAFETY: plain Rust state with no Lua references; dropping it closes an OS poller and clears
// the watch of each source it watched, touching no Lua API.
unsafe impl Userdata for Poller {
    const NAME: &'static str = "dream.tcp.Poller";
}

/// What `next()` returns.
type Next = Option<(f64, bool, bool, bool)>;

/// The source behind a handle a script passed to `watch`.
fn target(handle: ValueView<'_>) -> Result<Target> {
    if let Some(listener) = crate::userdata::receiver::<Listener>(handle) {
        return Ok(listener.target());
    }
    if let Some(stream) = crate::userdata::receiver::<Stream>(handle) {
        return stream.target();
    }
    #[cfg(feature = "dns")]
    if let Some(target) = crate::dns::watch_target(handle) {
        return target;
    }
    Err(handle.field_type_error(
        "dream.tcp.Poller.watch",
        "a dream.tcp.Listener or dream.tcp.Stream, or another dream_tcp_Watchable handle",
    ))
}

impl Poller {
    pub(crate) fn new(limits: Limits) -> io::Result<Poller> {
        let poll = mio::Poll::new()?;
        let registry = poll.registry().try_clone()?;
        Ok(Poller {
            core: Rc::new(Core {
                poll: RefCell::new(Some(poll)),
                registry,
                waker: RefCell::new(None),
                events: RefCell::new(mio::Events::with_capacity(limits.events as usize)),
                slots: RefCell::new(Slots::default()),
                deferred: Cell::new(false),
                limits,
            }),
        })
    }

    fn open(&self, what: &str) -> Result<()> {
        if self.core.poll.borrow().is_none() {
            return Err(Error::runtime(format!("dream.tcp.Poller.{what}: the poller is closed")));
        }
        Ok(())
    }

    fn watch(&self, handle: ValueView<'_>, token: Exact<i64>, interest: &str) -> Result<Outcome<bool>> {
        const WHAT: &str = "watch";
        self.open(WHAT)?;
        let target = target(handle)?;
        let interest = parse_interest(WHAT, interest, target.read_only)?;
        let token = check_token(WHAT, token)?;
        self.watch_target(target, token, interest)
    }

    /// Watches `target` under `token`; the checks every handle shares.
    pub(crate) fn watch_target(&self, target: Target, token: i64, interest: u8) -> Result<Outcome<bool>> {
        if target.source.watch_cell().borrow().is_some() {
            return Err(Error::runtime(
                "dream.tcp.Poller.watch: the handle is already watched (by this or another poller); unwatch it first",
            ));
        }
        let mut slots = self.core.slots.borrow_mut();
        self.core.sweep(&mut slots);
        if slots.by_token.contains_key(&token) {
            return Err(Error::runtime(format!("dream.tcp.Poller.watch: token {token} is already watching a handle")));
        }
        if slots.by_token.len() >= self.core.limits.watches as usize {
            slots.sweep();
            if slots.by_token.len() >= self.core.limits.watches as usize {
                return Err(Error::runtime(format!(
                    "dream.tcp.Poller.watch: the poller already watches maxWatches ({}) handles",
                    self.core.limits.watches
                )));
            }
        }
        let index = match slots.free.pop() {
            Some(index) => index,
            None => {
                let index = u32::try_from(slots.entries.len()).expect("bounded by maxWatches");
                slots.entries.push(Slot::empty());
                index
            }
        };
        // The one generation whose token would be the waker's is skipped.
        if Slots::mio_token(index, slots.entries[index as usize].generation) == WAKE_TOKEN {
            let slot = &mut slots.entries[index as usize];
            slot.generation = slot.generation.wrapping_add(1);
        }
        let generation = slots.entries[index as usize].generation;
        if let Err(error) = target.source.register(&self.core, Slots::mio_token(index, generation)) {
            slots.free.push(index);
            return Ok(Outcome::Failed(Failure {
                message: Cow::Owned(format!("dream.tcp.Poller.watch: {error}")),
                kind: crate::outcome::network_kind_of(&error),
            }));
        }
        slots.entries[index as usize] = Slot {
            used: true,
            generation,
            token,
            interest,
            read_only: target.read_only,
            ready: target.ready,
            source: Rc::downgrade(&target.source),
            synthetic: target.synthetic,
        };
        slots.by_token.insert(token, index);
        *target.source.watch_cell().borrow_mut() = Some(Watch { poller: Rc::downgrade(&self.core), slot: index });
        Ok(Outcome::Done(true))
    }

    fn modify(&self, token: Exact<i64>, interest: &str) -> Result<()> {
        const WHAT: &str = "modify";
        self.open(WHAT)?;
        let token = check_token(WHAT, token)?;
        let mut slots = self.core.slots.borrow_mut();
        let index = slots.find(WHAT, token)?;
        let slot = &mut slots.entries[index as usize];
        slot.interest = parse_interest(WHAT, interest, slot.read_only)?;
        Ok(())
    }

    fn unwatch(&self, token: Exact<i64>) -> Result<()> {
        const WHAT: &str = "unwatch";
        self.open(WHAT)?;
        let token = check_token(WHAT, token)?;
        let mut slots = self.core.slots.borrow_mut();
        let index = slots.find(WHAT, token)?;
        if let Some(source) = slots.entries[index as usize].source.upgrade() {
            source.watch_cell().borrow_mut().take();
            source.deregister(&self.core);
        }
        slots.free(index);
        Ok(())
    }

    fn wait(&self, call: &Call<'_>, timeout_ms: f64) -> Result<Outcome<f64>> {
        let limit = self.core.limits.wait_ms;
        if !(0.0..=f64::from(limit)).contains(&timeout_ms) {
            return Err(Error::runtime(format!(
                "dream.tcp.Poller.wait: timeout must be in [0, {limit}] milliseconds (the poller's maxWaitMs), got {timeout_ms}"
            )));
        }
        let mut budget = Duration::from_secs_f64(timeout_ms / 1000.0);
        // A wait never outlasts the runtime's execution time limit: the watchdog raises once
        // Luau runs again.
        if let Some(remaining) = crate::runtime::watchdog_remaining(call) {
            budget = budget.min(remaining);
        }
        let deadline = Instant::now() + budget;
        let mut poll = self.core.poll.borrow_mut();
        let poll = poll.as_mut().ok_or_else(|| Error::runtime("dream.tcp.Poller.wait: the poller is closed"))?;
        let mut events = self.core.events.borrow_mut();
        let mut slots = self.core.slots.borrow_mut();
        self.core.sweep(&mut slots);
        let max = self.core.limits.events as usize;
        loop {
            let now = Instant::now();
            let (pending, earliest) = slots.survey(now);
            // A source's own deadline ends the OS wait early, so it is reported on time with no
            // thread or timer behind it.
            let until = earliest.map_or(deadline, |earliest| earliest.min(deadline));
            let timeout = if pending { Duration::ZERO } else { until.saturating_duration_since(now) };
            match poll.poll(&mut events, Some(timeout)) {
                Ok(()) => slots.apply(&events),
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(error) => {
                    return Ok(Outcome::Failed(Failure {
                        message: Cow::Owned(format!("dream.tcp.Poller.wait: {error}")),
                        kind: crate::outcome::network_kind_of(&error),
                    }));
                }
            }
            let now = Instant::now();
            let queued = slots.fill(max, now);
            // Events for interest the script does not hold, and wakes for sources that are not
            // reportable yet, end the OS wait without making anything reportable; keep waiting
            // out the budget.
            if queued > 0 || now >= deadline {
                return Ok(Outcome::Done(queued as f64));
            }
        }
    }

    fn next(&self) -> Result<Next> {
        self.open("next")?;
        let now = Instant::now();
        let mut slots = self.core.slots.borrow_mut();
        while let Some((index, generation)) = slots.queue.pop_front() {
            let slot = &slots.entries[index as usize];
            // Unwatched, closed or rewatched since the wait, or drained by an operation.
            if !slot.used || slot.generation != generation {
                continue;
            }
            let bits = slot.reportable(now);
            if bits == 0 {
                continue;
            }
            return Ok(Some((slot.token as f64, bits & READABLE != 0, bits & WRITABLE != 0, bits & CLOSED != 0)));
        }
        Ok(None)
    }

    /// Releases every watch, the OS poller and the event buffer; a closed poller keeps only
    /// its userdata until collected. The handles stay open. Idempotent.
    pub fn close(&self) {
        self.core.release_all();
        self.core.poll.borrow_mut().take();
        self.core.waker.borrow_mut().take();
        *self.core.events.borrow_mut() = mio::Events::with_capacity(0);
        let mut slots = self.core.slots.borrow_mut();
        slots.entries = Vec::new();
        slots.free = Vec::new();
        slots.by_token = HashMap::new();
    }
}

fn parse_interest(what: &str, interest: &str, read_only: bool) -> Result<u8> {
    let bits = match interest {
        "read" => READ,
        "write" => WRITE,
        "readwrite" => READ | WRITE,
        other => {
            return Err(Error::runtime(format!(
                "dream.tcp.Poller.{what}: interest must be 'read', 'write' or 'readwrite', got '{other}'"
            )));
        }
    };
    if read_only && bits & WRITE != 0 {
        return Err(Error::runtime(format!(
            "dream.tcp.Poller.{what}: a listener is only ever readable (so is a request); watch it for 'read'"
        )));
    }
    Ok(bits)
}

fn check_token(what: &str, token: Exact<i64>) -> Result<i64> {
    if !(0..=MAX_TOKEN).contains(&token.0) {
        return Err(Error::runtime(format!(
            "dream.tcp.Poller.{what}: token must be a whole number in [0, 2^53], got {}",
            token.0
        )));
    }
    Ok(token.0)
}

pub(super) fn describe_poller(d: &mut ExtensionDescriptor) {
    let mut poller = d.userdata::<Poller>(Poller::NAME);
    poller.tag(TagPolicy::Preferred).doc(
        "Level-triggered readiness for watched listeners, streams and the handles other extensions add, under caller-chosen tokens. Never calls Luau.",
    );
    poller
        .method("watch", |p: &Poller, handle: ValueView<'_>, token: Exact<i64>, interest: &str| {
            p.watch(handle, token, interest)
        })
        .signature("(self, handle: dream_tcp_Watchable, token: number, interest: dream_tcp_Interest): (boolean?, string?, dream_tcp_ErrorKind?)")
        .doc("Watches an open handle under a token unique in this poller. A handle is watched by one poller at a time; a listener or a request only for 'read'.");
    poller
        .method("modify", |p: &Poller, token: Exact<i64>, interest: &str| p.modify(token, interest))
        .signature("(self, token: number, interest: dream_tcp_Interest)")
        .doc("Changes what a watch reports, without a system call: watch 'write' only while bytes are waiting.");
    poller
        .method("unwatch", |p: &Poller, token: Exact<i64>| p.unwatch(token))
        .signature("(self, token: number)")
        .doc("Stops watching; the token may be used again and no queued event of the old watch is reported.");
    poller
        .method("wait", |p: &Poller, call: &Call<'_>, timeout: f64| p.wait(call, timeout))
        .signature("(self, timeoutMs: number): (number?, string?, dream_tcp_ErrorKind?)")
        .doc("Waits at most timeoutMs (0 checks without waiting; at most maxWaitMs, and never past the runtime's execution time limit) and queues up to maxEvents ready watches. Returns how many were queued.");
    poller
        .method("next", |p: &Poller| p.next())
        .signature("(self): (number?, boolean, boolean, boolean)")
        .doc("The next queued event as token, readable, writable, closed, or nil when the queue is drained.");
    poller
        .method("close", |p: &Poller| p.close())
        .signature("(self)")
        .doc("Releases every watch and the OS poller; the handles stay open. Closing twice does nothing.");
    poller.getter("watching", |p: &Poller| p.core.slots.borrow().by_token.len() as f64).signature("number");
    poller.getter("closed", |p: &Poller| p.core.poll.borrow().is_none()).signature("boolean");
    poller.metamethod("__tostring", |p: &Poller| {
        format!(
            "dream.tcp.Poller({} watched{})",
            p.core.slots.borrow().by_token.len(),
            if p.core.poll.borrow().is_none() { ", closed" } else { "" }
        )
    });
}

#[cfg(test)]
mod tests {
    //! The seam other extensions build on, without them: a socketless source woken from another
    //! thread, a synthetic deadline, and the waker's token kept apart from every slot's.

    use std::sync::atomic::{AtomicBool, Ordering};

    use super::*;

    /// A socketless source completed by another thread through the poller's waker.
    struct Flag {
        watch: RefCell<Option<Watch>>,
        done: Arc<AtomicBool>,
        waker: RefCell<Option<Arc<mio::Waker>>>,
        due: Rc<Due>,
    }

    /// The flag's readiness, apart from it as the poller requires.
    struct Due {
        done: Arc<AtomicBool>,
        deadline: Option<Instant>,
    }

    impl Watchable for Flag {
        fn watch_cell(&self) -> &RefCell<Option<Watch>> {
            &self.watch
        }
        fn register(&self, core: &Core, _: mio::Token) -> io::Result<()> {
            *self.waker.borrow_mut() = Some(core.waker()?);
            Ok(())
        }
        fn deregister(&self, _: &Core) {
            self.waker.borrow_mut().take();
        }
    }

    impl Synthetic for Due {
        fn reportable(&self, _: u8, interest: u8, now: Instant) -> u8 {
            let due = self.done.load(Ordering::Acquire) || self.deadline.is_some_and(|deadline| now >= deadline);
            if due && interest & READ != 0 { READABLE } else { 0 }
        }
        fn deadline(&self) -> Option<Instant> {
            if self.done.load(Ordering::Acquire) { None } else { self.deadline }
        }
    }

    fn flag(deadline: Option<Instant>) -> Rc<Flag> {
        let done = Arc::new(AtomicBool::new(false));
        Rc::new(Flag {
            watch: RefCell::new(None),
            done: Arc::clone(&done),
            waker: RefCell::new(None),
            due: Rc::new(Due { done, deadline }),
        })
    }

    fn watch(poller: &Poller, source: &Rc<Flag>, token: i64) {
        let target = Target {
            source: Rc::clone(source) as Rc<dyn Watchable>,
            ready: Rc::new(Cell::new(0)),
            read_only: true,
            synthetic: Some(Rc::clone(&source.due) as Rc<dyn Synthetic>),
        };
        assert!(matches!(poller.watch_target(target, token, READ).unwrap(), Outcome::Done(true)));
    }

    /// The wait loop without a VM: the same survey, OS wait and fill.
    fn wait(poller: &Poller, timeout: Duration) -> usize {
        let deadline = Instant::now() + timeout;
        let mut poll = poller.core.poll.borrow_mut();
        let poll = poll.as_mut().unwrap();
        let mut events = poller.core.events.borrow_mut();
        let mut slots = poller.core.slots.borrow_mut();
        loop {
            let now = Instant::now();
            let (pending, earliest) = slots.survey(now);
            let until = earliest.map_or(deadline, |earliest| earliest.min(deadline));
            let timeout = if pending { Duration::ZERO } else { until.saturating_duration_since(now) };
            poll.poll(&mut events, Some(timeout)).unwrap();
            slots.apply(&events);
            let now = Instant::now();
            let queued = slots.fill(256, now);
            if queued > 0 || now >= deadline {
                return queued;
            }
        }
    }

    #[test]
    fn a_completion_on_another_thread_ends_a_wait_promptly() {
        let poller = Poller::new(Limits::default()).unwrap();
        let source = flag(None);
        watch(&poller, &source, 7);
        let (done, waker) = (Arc::clone(&source.done), source.waker.borrow().clone().unwrap());
        let worker = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(30));
            done.store(true, Ordering::Release);
            waker.wake().unwrap();
        });
        let started = Instant::now();
        assert_eq!(wait(&poller, Duration::from_secs(5)), 1);
        assert!(started.elapsed() < Duration::from_secs(1), "{:?}", started.elapsed());
        worker.join().unwrap();
        assert_eq!(poller.next().unwrap(), Some((7.0, true, false, false)));
        // Completed before it is watched: reported at once, no wake needed.
        let early = flag(None);
        early.done.store(true, Ordering::Release);
        watch(&poller, &early, 8);
        assert_eq!(wait(&poller, Duration::ZERO), 2);
    }

    #[test]
    fn a_deadline_ends_a_wait_with_no_event() {
        let poller = Poller::new(Limits::default()).unwrap();
        let source = flag(Some(Instant::now() + Duration::from_millis(40)));
        watch(&poller, &source, 1);
        let started = Instant::now();
        assert_eq!(wait(&poller, Duration::from_secs(5)), 1);
        let waited = started.elapsed();
        assert!(waited >= Duration::from_millis(35) && waited < Duration::from_secs(1), "{waited:?}");
    }

    #[test]
    fn the_wake_token_is_never_a_slot_and_releasing_a_source_frees_its_slot() {
        assert_ne!(Slots::mio_token(0, 0), WAKE_TOKEN);
        assert_ne!(Slots::mio_token(65_535, u32::MAX), WAKE_TOKEN);
        let poller = Poller::new(Limits::default()).unwrap();
        let source = flag(None);
        watch(&poller, &source, 3);
        // A wake for nothing reportable ends the OS wait but queues nothing.
        source.waker.borrow().as_ref().unwrap().wake().unwrap();
        assert_eq!(wait(&poller, Duration::from_millis(20)), 0);
        let slot = source.watch.borrow().as_ref().unwrap().slot;
        source.watch.borrow_mut().take();
        poller.core.release(slot);
        assert_eq!(poller.core.slots.borrow().by_token.len(), 0);
        // The freed slot's next watch has a new generation, so an old token cannot reach it.
        let again = flag(None);
        watch(&poller, &again, 3);
        assert_eq!(poller.core.slots.borrow().entries[slot as usize].generation, 1);
        drop(again);
        poller.core.slots.borrow_mut().sweep();
        assert_eq!(poller.core.slots.borrow().by_token.len(), 0);
    }
}
