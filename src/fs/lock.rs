//! `dream.fs.Lock`: an operating-system lock on a file, the handle `fs.lock` returns.
//!
//! The lock is the platform's own (`flock` on Unix, `LockFileEx` on Windows, through
//! [`File::try_lock`]), taken on a file opened for it and held for as long as that file is open.
//! So it ends however the holder does: `unlock`, the handle collected, the runtime shut down, or
//! the process gone, when the system releases it. A lock never outlives its holder and never
//! times out, which is what makes it safe to build on.
//!
//! It is exclusive unless asked to be shared, and never waits: a lock held elsewhere is answered
//! at once (`nil`, the message, `wouldBlock`), and the caller decides whether to try again. On Unix
//! it is advisory: it excludes other locks on the file, not reads or writes of it.

use std::cell::RefCell;
use std::fs::{File, OpenOptions, TryLockError};
use std::io;
use std::rc::Rc;

use crate::extension::{ExtensionDescriptor, TagPolicy};
use crate::options::Options;
use crate::outcome::Failure;
use crate::error::Result;
use crate::stack::{Scope, ValueView};
use crate::userdata::Userdata;

use super::{Outcome, display, done};

/// How `fs.lock` locks: `{ shared?, createParents? }`.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct LockOptions {
    shared: bool,
    create_parents: bool,
}

impl LockOptions {
    pub(crate) fn read(scope: &impl Scope, options: Option<ValueView<'_>>) -> Result<LockOptions> {
        let mut lock = LockOptions::default();
        let Some(options) = options.filter(|view| !view.is_nil()) else {
            return Ok(lock);
        };
        Options::read(scope, options, "dream.fs.lock", |o| {
            lock.shared = o.or("shared", false)?;
            lock.create_parents = o.or("createParents", false)?;
            Ok(lock)
        })
    }
}

/// The open file a lock is held through: `None` once released.
#[derive(Debug)]
pub(crate) struct LockFile(RefCell<Option<File>>);

impl LockFile {
    /// Releases the lock and closes its file; true if it was held.
    fn release(&self) -> bool {
        let Some(file) = self.0.borrow_mut().take() else {
            return false;
        };
        // Closing the file releases the lock too; unlocking first makes it immediate on every
        // platform, and a failure here changes nothing, since the close follows.
        let _ = file.unlock();
        true
    }
}

impl crate::runtime::shared::Resource for LockFile {
    fn close_for_shutdown(&self) {
        self.release();
    }
}

/// `dream.fs.Lock`: an operating-system lock on one file.
pub struct Lock {
    /// Shared with the runtime's resource list, so `Runtime::shutdown` releases it.
    file: Rc<LockFile>,
    shared: bool,
    name: Box<[u8]>,
}

impl std::fmt::Debug for Lock {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Lock")
            .field("name", &display(&self.name))
            .field("shared", &self.shared)
            .field("held", &self.file.0.borrow().is_some())
            .finish_non_exhaustive()
    }
}

// SAFETY: a file handle and plain data; no Lua references. Dropping closes the handle, which
// releases the lock, with no Lua API.
unsafe impl Userdata for Lock {
    const NAME: &'static str = "dream.fs.Lock";
}

impl Lock {
    /// Opens `path` (creating it) and takes its lock without waiting.
    pub(crate) fn take(path: &[u8], options: LockOptions) -> Result<Outcome<Lock>> {
        let host = super::host_path("lock", path)?;
        if options.create_parents
            && let Some(parent) = host.parent().filter(|parent| !parent.as_os_str().is_empty())
        {
            done!(Outcome::of(std::fs::create_dir_all(parent), "lock", path));
        }
        let file = done!(Outcome::of(
            OpenOptions::new().read(true).write(true).create(true).truncate(false).open(&host),
            "lock",
            path
        ));
        let taken = if options.shared { file.try_lock_shared() } else { file.try_lock() };
        Ok(match taken {
            Ok(()) => Outcome::Done(Lock {
                file: Rc::new(LockFile(RefCell::new(Some(file)))),
                shared: options.shared,
                name: path.into(),
            }),
            Err(TryLockError::WouldBlock) => Outcome::Failed(Failure::message(
                format!("dream.fs.lock: {}: held by another lock", display(path)),
                &io::Error::from(io::ErrorKind::WouldBlock),
            )),
            Err(TryLockError::Error(error)) => Outcome::Failed(Failure::new("lock", path, &error)),
        })
    }

    /// The held file, for the runtime's resource list.
    pub(crate) fn resource(&self) -> std::rc::Weak<dyn crate::runtime::shared::Resource> {
        let file: Rc<dyn crate::runtime::shared::Resource> = self.file.clone();
        Rc::downgrade(&file)
    }
}

pub(crate) fn describe_lock(d: &mut ExtensionDescriptor) {
    let mut lock = d.userdata::<Lock>(Lock::NAME);
    lock.tag(TagPolicy::Never).doc(
        "An operating-system lock on one file, held until unlock, the handle is collected, the runtime shuts down, or the process ends.",
    );
    lock.method("unlock", |l: &Lock| l.file.release())
        .signature("(self): boolean")
        .doc("Releases the lock; true if it was held, false if it was already released.");
    lock.method("held", |l: &Lock| l.file.0.borrow().is_some())
        .signature("(self): boolean")
        .doc("Whether this handle still holds its lock.");
    lock.method("shared", |l: &Lock| l.shared)
        .signature("(self): boolean")
        .doc("Whether the lock is shared rather than exclusive.");
    lock.metamethod("__tostring", |l: &Lock| {
        let kind = if l.shared { "shared" } else { "exclusive" };
        format!("dream.fs.Lock({}, {kind})", display(&l.name))
    });
}
