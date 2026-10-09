//! A request as a `@dream/tcp` poller source: readable once it reaches a terminal phase or
//! its deadline, until the script takes it.
//!
//! The source has no socket. Watching it hands the poller's [`mio::Waker`] to the request's
//! shared state under the request's lock, and the worker reads it under the same lock when it
//! finishes, so a completion either sees the waker and wakes the poller, or happened first and
//! is seen by the poller's scan: none is lost. The deadline is the poller's to keep, through
//! [`Synthetic::deadline`], so a timeout is reported on time with no thread or timer.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::Arc;
use std::time::Instant;

use crate::error::{Error, Result};
use crate::stack::ValueView;
use crate::tcp::poller::{Core, READ, Synthetic, Target, Watch, Watchable};
use crate::tcp::socket::READABLE;

use super::request::Request;
use super::resolver::{Shared, Wake};

/// The request's side of a watch.
pub(crate) struct Source {
    watch: RefCell<Option<Watch>>,
    shared: Arc<Shared>,
}

impl Source {
    pub(crate) fn new(shared: &Arc<Shared>) -> Rc<Source> {
        Rc::new(Source { watch: RefCell::new(None), shared: Arc::clone(shared) })
    }

    /// Releases the watch, if any. Idempotent.
    pub(crate) fn detach(&self) {
        let watch = self.watch.borrow_mut().take();
        self.shared.lock().waker = None;
        if let Some(watch) = watch
            && let Some(core) = watch.poller.upgrade()
        {
            core.release(watch.slot);
        }
    }
}

impl Drop for Source {
    fn drop(&mut self) {
        self.detach();
    }
}

impl Watchable for Source {
    fn watch_cell(&self) -> &RefCell<Option<Watch>> {
        &self.watch
    }

    fn register(&self, core: &Core, _: mio::Token) -> std::io::Result<()> {
        let waker: Arc<dyn Wake> = core.waker()?;
        self.shared.lock().waker = Some(waker);
        Ok(())
    }

    fn deregister(&self, _: &Core) {
        self.shared.lock().waker = None;
    }
}

/// What the poller reports for a request.
struct Due {
    shared: Arc<Shared>,
    taken: Rc<Cell<bool>>,
    deadline: Instant,
}

impl Synthetic for Due {
    fn reportable(&self, _: u8, interest: u8, now: Instant) -> u8 {
        if interest & READ == 0 || self.taken.get() {
            return 0;
        }
        if self.shared.is_terminal() || now >= self.deadline { READABLE } else { 0 }
    }

    fn deadline(&self) -> Option<Instant> {
        (!self.taken.get() && !self.shared.is_terminal()).then_some(self.deadline)
    }
}

/// The poller's target for a request handle, or `None` when `handle` is not one.
pub(crate) fn watch_target(handle: ValueView<'_>) -> Option<Result<Target>> {
    let request = crate::userdata::receiver::<Request>(handle)?;
    if request.taken.get() {
        return Some(Err(Error::runtime(
            "dream.tcp.Poller.watch: the request's result was taken or it is closed; there is nothing to wait for",
        )));
    }
    Some(Ok(Target {
        source: Rc::clone(&request.source) as Rc<dyn Watchable>,
        ready: Rc::new(Cell::new(0)),
        read_only: true,
        synthetic: Some(Rc::new(Due {
            shared: Arc::clone(&request.shared),
            taken: Rc::clone(&request.taken),
            deadline: request.deadline,
        })),
    }))
}
