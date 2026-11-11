//! Ending a thread that is blocked on a child's pipe.
//!
//! A reader blocked in `read`, or a writer blocked on a full pipe, ends on its own only when the
//! other end of the pipe is closed. Killing the child's process group closes it in the ordinary
//! case. It does not when a process the child started left that group (`setsid`, `setpgid`) and
//! kept the pipe: nothing this host may signal holds it, and the thread, its descriptor and
//! whatever it buffered would stay until that stranger exits.
//!
//! So on Unix no thread here blocks on a pipe alone. Each waits in `poll` on the pipe and on a
//! [`Cancel`], and `cancel` wakes every one of them at once, whoever holds the pipe. Elsewhere
//! there is no such wait, and a thread blocks until its pipe closes, as before.

use std::io::{Read, Write};

/// What a thread blocked on a pipe also waits on. Cancelling is permanent and wakes every waiter.
pub(super) struct Cancel {
    #[cfg(unix)]
    inner: unix::Pair,
}

/// A pipe end a thread can wait on together with a [`Cancel`].
pub(super) trait Pollable {
    #[cfg(unix)]
    fn descriptor(&self) -> std::os::fd::RawFd;
}

#[cfg(unix)]
macro_rules! pollable {
    ($($pipe:ty),*) => {$(
        impl Pollable for $pipe {
            fn descriptor(&self) -> std::os::fd::RawFd {
                std::os::fd::AsRawFd::as_raw_fd(self)
            }
        }
    )*};
}

#[cfg(not(unix))]
macro_rules! pollable {
    ($($pipe:ty),*) => {$( impl Pollable for $pipe {} )*};
}

pollable!(std::process::ChildStdout, std::process::ChildStderr, std::process::ChildStdin);

impl Cancel {
    /// A new, uncancelled one. On Unix this is a pipe of its own, so it can fail as opening one can.
    pub(super) fn new() -> std::io::Result<Self> {
        Ok(Self {
            #[cfg(unix)]
            inner: unix::Pair::new()?,
        })
    }

    /// Wakes every thread waiting on this, now and from now on.
    pub(super) fn cancel(&self) {
        #[cfg(unix)]
        self.inner.cancel();
    }

    /// Reads from `pipe` into `chunk`, as `read` does, or answers `None` once cancelled.
    pub(super) fn read(&self, pipe: &mut (impl Read + Pollable), chunk: &mut [u8]) -> std::io::Result<Option<usize>> {
        loop {
            #[cfg(unix)]
            if !self.inner.wait(pipe.descriptor(), libc::POLLIN)? {
                return Ok(None);
            }
            match pipe.read(chunk) {
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                result => return result.map(Some),
            }
        }
    }

    /// Lets writes to `pipe` return instead of blocking on a full pipe, so that [`Cancel::write_all`]
    /// can be cancelled. Call once, before the first write.
    pub(super) fn prepare_writer(pipe: &impl Pollable) -> std::io::Result<()> {
        #[cfg(unix)]
        unix::nonblocking(pipe.descriptor())?;
        #[cfg(not(unix))]
        let _ = pipe;
        Ok(())
    }

    /// Writes all of `bytes` to `pipe`, or answers `false` once cancelled, with some of them unwritten.
    pub(super) fn write_all(&self, pipe: &mut (impl Write + Pollable), mut bytes: &[u8]) -> std::io::Result<bool> {
        while !bytes.is_empty() {
            match pipe.write(bytes) {
                Ok(0) => return Err(std::io::ErrorKind::WriteZero.into()),
                Ok(count) => bytes = &bytes[count..],
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                #[cfg(unix)]
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    if !self.inner.wait(pipe.descriptor(), libc::POLLOUT)? {
                        return Ok(false);
                    }
                }
                Err(error) => return Err(error),
            }
        }
        Ok(true)
    }
}

#[cfg(unix)]
mod unix {
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
    use std::sync::{Mutex, PoisonError};

    /// A pipe nobody reads from: one byte written to it makes its read end readable for good.
    pub(super) struct Pair {
        read: OwnedFd,
        write: Mutex<Option<OwnedFd>>,
    }

    fn flag(fd: RawFd, get: libc::c_int, set: libc::c_int, bit: libc::c_int) -> std::io::Result<()> {
        // SAFETY: `fd` is a descriptor this module owns or was handed; fcntl with these commands
        // only reads and writes its flags.
        let flags = unsafe { libc::fcntl(fd, get) };
        if flags < 0 {
            return Err(std::io::Error::last_os_error());
        }
        // SAFETY: as above.
        if unsafe { libc::fcntl(fd, set, flags | bit) } < 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(())
    }

    pub(super) fn nonblocking(fd: RawFd) -> std::io::Result<()> {
        flag(fd, libc::F_GETFL, libc::F_SETFL, libc::O_NONBLOCK)
    }

    impl Pair {
        pub(super) fn new() -> std::io::Result<Self> {
            let mut fds: [libc::c_int; 2] = [0; 2];
            // SAFETY: pipe writes two new descriptors into the array it is given.
            if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
                return Err(std::io::Error::last_os_error());
            }
            // SAFETY: both descriptors were just created and belong to nothing else.
            let (read, write) = unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) };
            // A child started in the meantime must not inherit either end. Cancelling writes a
            // byte rather than closing the write end, so a copy a child did inherit in the gap
            // between `pipe` and here could not hold a cancellation back.
            for end in [&read, &write] {
                flag(end.as_raw_fd(), libc::F_GETFD, libc::F_SETFD, libc::FD_CLOEXEC)?;
            }
            nonblocking(write.as_raw_fd())?;
            Ok(Self { read, write: Mutex::new(Some(write)) })
        }

        pub(super) fn cancel(&self) {
            let mut write = self.write.lock().unwrap_or_else(PoisonError::into_inner);
            if let Some(end) = write.take() {
                // SAFETY: `end` is the open write end of our own pipe, and the byte is a local.
                // The pipe is empty, so the write cannot block; its result changes nothing, since
                // dropping `end` below closes the write end and that wakes the waiters too.
                unsafe { libc::write(end.as_raw_fd(), [1_u8].as_ptr().cast(), 1) };
            }
        }

        /// Waits until `fd` is ready for `events` (`true`) or this is cancelled (`false`).
        pub(super) fn wait(&self, fd: RawFd, events: libc::c_short) -> std::io::Result<bool> {
            loop {
                let mut fds = [
                    libc::pollfd { fd: self.read.as_raw_fd(), events: libc::POLLIN, revents: 0 },
                    libc::pollfd { fd, events, revents: 0 },
                ];
                // SAFETY: `fds` is a live array of two pollfd, and poll only writes their revents.
                let ready = unsafe { libc::poll(fds.as_mut_ptr(), 2, -1) };
                if ready < 0 {
                    let error = std::io::Error::last_os_error();
                    if error.kind() == std::io::ErrorKind::Interrupted {
                        continue;
                    }
                    return Err(error);
                }
                // Cancellation wins over readiness: a cancelled thread must not start another read.
                if fds[0].revents != 0 {
                    return Ok(false);
                }
                if fds[1].revents != 0 {
                    return Ok(true);
                }
            }
        }
    }
}
