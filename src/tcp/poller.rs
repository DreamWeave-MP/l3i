//! `dream.tcp.Poller`: bounded readiness waits over watched listeners and streams.
//!
//! The OS reports edges (mio's contract on every platform); the poller keeps the last readiness
//! each handle was told in a cell it shares with the handle, and the handle clears a bit only
//! when an operation reports would-block. A wait therefore reports every handle that may still
//! make progress, which is level-triggered readiness, and it never blocks while one is pending.
//!
//! Watches live in slots; a slot's mio token carries its index and a generation, and the queue
//! of events `next()` hands out keeps both, so an event never reaches a watch that replaced the
//! one it was raised for. Slots hold the handle weakly and the handle holds the poller weakly.

use std::borrow::Cow;
use std::cell::{Cell, RefCell};
use std::collections::{HashMap, VecDeque};
use std::io;
use std::rc::{Rc, Weak};
use std::time::{Duration, Instant};

use crate::bind::Call;
use crate::convert::Exact;
use crate::error::{Error, Result};
use crate::extension::{ExtensionDescriptor, TagPolicy};
use crate::outcome::{Failure, Outcome};
use crate::stack::ValueView;
use crate::userdata::Userdata;

use super::socket::{CLOSED, Io, Listener, READABLE, Socket, Stream, WRITABLE, Watch};

/// Interest bits.
const READ: u8 = 1;
const WRITE: u8 = 2;

/// The largest token: tokens are whole numbers a double holds exactly.
const MAX_TOKEN: i64 = 1 << 53;

/// Half of a mio token is the slot index, half its generation.
const INDEX_BITS: u32 = usize::BITS / 2;
const INDEX_MASK: usize = (1 << INDEX_BITS) - 1;

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
    listener: bool,
    ready: Rc<Cell<u8>>,
    io: Weak<Io>,
}

impl Slot {
    /// The readiness bits the interest makes reportable: `closed` goes with reading.
    fn reportable(&self) -> u8 {
        let mut mask = 0;
        if self.interest & READ != 0 {
            mask |= READABLE | CLOSED;
        }
        if self.interest & WRITE != 0 {
            mask |= WRITABLE;
        }
        self.ready.get() & mask
    }

    /// Whether this slot's handle may make progress. A slot whose handle is gone (collected
    /// while the poller was busy) is never ready.
    fn is_ready(&self) -> bool {
        self.used && self.reportable() != 0 && self.io.strong_count() != 0
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
        slot.used = false;
        slot.generation = slot.generation.wrapping_add(1);
        slot.io = Weak::new();
        slot.ready = Rc::new(Cell::new(0));
        self.free.push(index);
    }

    /// Frees the slots whose handles were collected while the poller could not be told.
    fn sweep(&mut self) {
        let gone: Vec<u32> = (0..self.entries.len() as u32)
            .filter(|&index| {
                let slot = &self.entries[index as usize];
                slot.used && slot.io.strong_count() == 0
            })
            .collect();
        for index in gone {
            self.free(index);
        }
    }

    /// Records what the OS reported for each live watch.
    fn apply(&self, events: &mio::Events) {
        for event in events {
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

    /// Queues up to `max` ready watches, scanning once round from the cursor.
    fn fill(&mut self, max: usize) -> usize {
        self.queue.clear();
        let count = self.entries.len();
        if count == 0 {
            return 0;
        }
        let start = self.cursor % count;
        for step in 0..count {
            let index = (start + step) % count;
            let slot = &self.entries[index];
            if slot.is_ready() {
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

/// The poller's state, shared weakly with the handles it watches.
pub(crate) struct Core {
    poll: RefCell<Option<mio::Poll>>,
    /// A second handle on the poll's registry, so a handle closing while the poller waits (it
    /// cannot: no Lua runs inside a wait) or after it closed never needs the poll itself.
    registry: mio::Registry,
    events: RefCell<mio::Events>,
    slots: RefCell<Slots>,
    /// A handle released its watch while the slots were borrowed.
    deferred: Cell<bool>,
    limits: Limits,
}

impl Core {
    /// Drops the watch in `slot` of a handle that is closing.
    pub(super) fn release(&self, slot: u32, socket: &mut Socket) {
        // A failure means the registration is gone already; closing the socket ends it anyway.
        let _ = socket.deregister(&self.registry);
        // Busy only if a handle is collected during a poller call; the next wait sweeps it.
        match self.slots.try_borrow_mut() {
            Ok(mut slots) => slots.free(slot),
            Err(_) => self.deferred.set(true),
        }
    }

    /// Frees the watches of handles collected while the slots were busy.
    fn sweep(&self, slots: &mut Slots) {
        if self.deferred.replace(false) {
            slots.sweep();
        }
    }

    /// Releases every watch: the handles stay open and may be watched again.
    fn release_all(&self) {
        let mut slots = self.slots.borrow_mut();
        for index in 0..slots.entries.len() as u32 {
            let slot = &slots.entries[index as usize];
            if !slot.used {
                continue;
            }
            if let Some(io) = slot.io.upgrade() {
                io.watch.borrow_mut().take();
                io.with_socket(|socket| socket.deregister(&self.registry));
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
// the watch of each handle it watched, touching no Lua API.
unsafe impl Userdata for Poller {
    const NAME: &'static str = "dream.tcp.Poller";
}

/// What `next()` returns.
type Next = Option<(f64, bool, bool, bool)>;

impl Poller {
    pub(crate) fn new(limits: Limits) -> io::Result<Poller> {
        let poll = mio::Poll::new()?;
        let registry = poll.registry().try_clone()?;
        Ok(Poller {
            core: Rc::new(Core {
                poll: RefCell::new(Some(poll)),
                registry,
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
        let (io, listener) = if let Some(listener) = crate::userdata::receiver::<Listener>(handle) {
            (Rc::clone(listener.io()), true)
        } else if let Some(stream) = crate::userdata::receiver::<Stream>(handle) {
            (Rc::clone(stream.io()), false)
        } else {
            return Err(handle.field_type_error("dream.tcp.Poller.watch", "a dream.tcp.Listener or dream.tcp.Stream"));
        };
        let interest = parse_interest(WHAT, interest, listener)?;
        let token = check_token(WHAT, token)?;
        if io.is_closed() {
            return Err(Error::runtime("dream.tcp.Poller.watch: the handle is closed"));
        }
        if io.watch.borrow().is_some() {
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
                slots.entries.push(Slot {
                    used: false,
                    generation: 0,
                    token: 0,
                    interest: 0,
                    listener: false,
                    ready: Rc::new(Cell::new(0)),
                    io: Weak::new(),
                });
                index
            }
        };
        let generation = slots.entries[index as usize].generation;
        let registered = io
            .with_socket(|socket| socket.register(&self.core.registry, Slots::mio_token(index, generation)))
            .expect("checked open above");
        if let Err(error) = registered {
            slots.free.push(index);
            return Ok(Outcome::Failed(Failure {
                message: Cow::Owned(format!("dream.tcp.Poller.watch: {error}")),
                kind: crate::outcome::network_kind_of(&error),
            }));
        }
        let slot = &mut slots.entries[index as usize];
        *slot = Slot {
            used: true,
            generation,
            token,
            interest,
            listener,
            ready: Rc::clone(&io.ready),
            io: Rc::downgrade(&io),
        };
        slots.by_token.insert(token, index);
        *io.watch.borrow_mut() = Some(Watch { poller: Rc::downgrade(&self.core), slot: index });
        Ok(Outcome::Done(true))
    }

    fn modify(&self, token: Exact<i64>, interest: &str) -> Result<()> {
        const WHAT: &str = "modify";
        self.open(WHAT)?;
        let token = check_token(WHAT, token)?;
        let mut slots = self.core.slots.borrow_mut();
        let index = slots.find(WHAT, token)?;
        let slot = &mut slots.entries[index as usize];
        slot.interest = parse_interest(WHAT, interest, slot.listener)?;
        Ok(())
    }

    fn unwatch(&self, token: Exact<i64>) -> Result<()> {
        const WHAT: &str = "unwatch";
        self.open(WHAT)?;
        let token = check_token(WHAT, token)?;
        let mut slots = self.core.slots.borrow_mut();
        let index = slots.find(WHAT, token)?;
        if let Some(io) = slots.entries[index as usize].io.upgrade() {
            io.watch.borrow_mut().take();
            io.with_socket(|socket| socket.deregister(&self.core.registry));
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
            let pending = slots.entries.iter().any(Slot::is_ready);
            let timeout = if pending { Duration::ZERO } else { deadline.saturating_duration_since(Instant::now()) };
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
            let queued = slots.fill(max);
            // Events for interest the script does not hold wake the OS wait without making
            // anything reportable; keep waiting out the budget.
            if queued > 0 || Instant::now() >= deadline {
                return Ok(Outcome::Done(queued as f64));
            }
        }
    }

    fn next(&self) -> Result<Next> {
        self.open("next")?;
        let mut slots = self.core.slots.borrow_mut();
        while let Some((index, generation)) = slots.queue.pop_front() {
            let slot = &slots.entries[index as usize];
            // Unwatched, closed or rewatched since the wait, or drained by an operation.
            if !slot.used || slot.generation != generation {
                continue;
            }
            let bits = slot.reportable();
            if bits == 0 {
                continue;
            }
            return Ok(Some((slot.token as f64, bits & READABLE != 0, bits & WRITABLE != 0, bits & CLOSED != 0)));
        }
        Ok(None)
    }

    /// Releases every watch and the OS poller. The handles stay open. Idempotent.
    pub fn close(&self) {
        self.core.release_all();
        self.core.poll.borrow_mut().take();
    }
}

fn parse_interest(what: &str, interest: &str, listener: bool) -> Result<u8> {
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
    if listener && bits & WRITE != 0 {
        return Err(Error::runtime(format!(
            "dream.tcp.Poller.{what}: a listener is only ever readable; watch it for 'read'"
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
        "Level-triggered readiness for watched listeners and streams, under caller-chosen tokens. Never calls Luau.",
    );
    poller
        .method("watch", |p: &Poller, handle: ValueView<'_>, token: Exact<i64>, interest: &str| {
            p.watch(handle, token, interest)
        })
        .signature("(self, handle: dream_tcp_Listener | dream_tcp_Stream, token: number, interest: dream_tcp_Interest): (boolean?, string?, dream_tcp_ErrorKind?)")
        .doc("Watches an open handle under a token unique in this poller. A handle is watched by one poller at a time; a listener only for 'read'.");
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
