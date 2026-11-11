//! A one-shot secret for a child: bytes it can read once from a descriptor or handle it inherits.
//!
//! A child's arguments are in `/proc/<pid>/cmdline` and in every process listing; its environment
//! is in `/proc/<pid>/environ` and is copied to everything it starts; a file is a file. None of
//! them is a place for a credential. What is left is a pipe.
//!
//! The secret is written, whole, into a new pipe, and the write end is closed. The child inherits
//! the read end; its number (a descriptor on Unix, a handle's value on Windows), and nothing
//! else, is put in an environment variable the caller names. Whoever reads it first gets the bytes
//! and then end of file: the child itself, or a helper it starts (a Git credential helper, say),
//! which inherits it in its turn. The bytes are never in an argument, an environment value or a
//! file, and once read they are in no pipe either.
//!
//! What this does not do, so that nobody relies on it:
//!
//! * It does not hide the secret from other processes of the same user. On Linux such a process
//!   can open `/proc/<child>/fd/<n>` and read the pipe before the child does, or hold it open for
//!   writing so that the child never meets end of file. A reader should read at once, treat an
//!   empty read as a secret that was taken, and be run with a time limit.
//! * It does not overwrite the script's own copy: the Luau string or buffer the secret came from
//!   lives until it is collected. Only the copy made here is overwritten.
//! * It does not stop the child passing the descriptor on: everything the child starts inherits
//!   it unless the child closes it, including something that outlives the child.
//!
//! Keeping it to one child:
//!
//! * Unix. Both ends are close-on-exec from the moment they exist where the system can make them
//!   so in one step (`pipe2`), so a child another thread starts meanwhile inherits neither; the
//!   flag is cleared on the read end in this child alone, between `fork` and `exec`. Where there
//!   is no `pipe2` (macOS), a child started by another thread between making the pipe and marking
//!   it inherits both ends: it could read the secret later, and while it lives the intended reader
//!   never meets end of file. Neither end is ever one of the standard three descriptors, which the
//!   child's own standard streams are put on.
//! * Windows. A handle is inherited by any child started while it is marked inheritable, and the
//!   standard library offers no stable way to name the handles one child gets. So every child this
//!   module's caller starts is started under one lock, and the read handle is inheritable only
//!   inside it: no other child of this library can inherit it. A child anything else in the host
//!   starts in that moment can, an ordinary `std::process::Command` in another crate included:
//!   the standard library's own lock does not know about this handle. The lock also means this
//!   library starts one child at a time on Windows, so a start that the system holds up (a
//!   program on a stalled share, a scanner) holds up the others until it returns. The pipe is
//!   made and filled before the lock is taken, and the write cannot wait.
//!
//! On every system a process of the same user can take the secret before the child does: through
//! `/proc/<child>/fd/<n>` on Linux, by duplicating the handle out of the child on Windows.
//!
//! The parent's copy of the read end is closed as soon as the child has been started, and its copy
//! of the bytes is overwritten when it is dropped. The secret must fit the pipe without waiting,
//! since nothing is reading yet: at most [`MAX_BYTES`].

use std::process::Command;

/// The most bytes a secret may have: what every supported system's pipe takes in one write that
/// cannot block.
pub(super) const MAX_BYTES: usize = 4096;

/// What the options asked for: the variable to name the descriptor in, and the bytes.
pub(super) struct Secret {
    pub(super) env: std::ffi::OsString,
    pub(super) data: Vec<u8>,
}

impl Drop for Secret {
    fn drop(&mut self) {
        for byte in &mut self.data {
            // SAFETY: `byte` is a live, exclusively borrowed element of the vector. The write is
            // volatile so that it is not removed as a store to memory about to be freed.
            unsafe { std::ptr::write_volatile(byte, 0) };
        }
    }
}

/// What must be held from just before a child is started until just after: the parent's end of an
/// attached secret and, on Windows, the lock every start is made under. Drop it once `spawn` has
/// returned, whatever it returned.
pub(super) struct Starting {
    #[cfg(unix)]
    _read: Option<std::os::fd::OwnedFd>,
    #[cfg(windows)]
    _inner: windows::Starting,
}

/// Prepares `command` to be started, with `secret` for the child to inherit when there is one.
/// Call it for every start, secret or not: on Windows it is also what keeps a child without a
/// secret from inheriting another's.
///
/// # Errors
///
/// When the pipe cannot be made or the secret does not fit it; and, on a system that is neither
/// Unix nor Windows, whenever there is a secret.
pub(super) fn prepare(command: &mut Command, secret: Option<&Secret>) -> std::io::Result<Starting> {
    #[cfg(unix)]
    {
        Ok(Starting { _read: secret.map(|secret| unix::attach(command, secret)).transpose()? })
    }
    #[cfg(windows)]
    {
        Ok(Starting { _inner: windows::prepare(command, secret)? })
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = command;
        if secret.is_some() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                "a secret needs an inherited descriptor or handle, which this platform does not have",
            ));
        }
        Ok(Starting {})
    }
}

#[cfg(unix)]
mod unix {
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    use std::os::unix::process::CommandExt;
    use std::process::Command;

    use super::Secret;

    /// A descriptor at 3 or above for `end`: `end` itself, or a close-on-exec copy when the host
    /// had closed one of its standard streams and the pipe was given its place. In the child the
    /// standard three are overwritten by its own streams before anything of ours runs, and in the
    /// host they are what other code reads and writes.
    fn above_standard(end: OwnedFd) -> std::io::Result<OwnedFd> {
        if end.as_raw_fd() > libc::STDERR_FILENO {
            return Ok(end);
        }
        // SAFETY: `end` is an open descriptor this function owns; F_DUPFD_CLOEXEC returns a new
        // descriptor at or above 3 with close-on-exec set, or an error.
        let moved = unsafe { libc::fcntl(end.as_raw_fd(), libc::F_DUPFD_CLOEXEC, libc::STDERR_FILENO + 1) };
        if moved < 0 {
            return Err(std::io::Error::last_os_error());
        }
        // SAFETY: `moved` was just created and belongs to nothing else. `end` is closed on return.
        Ok(unsafe { OwnedFd::from_raw_fd(moved) })
    }

    pub(super) fn attach(command: &mut Command, secret: &Secret) -> std::io::Result<OwnedFd> {
        let mut fds: [libc::c_int; 2] = [0; 2];
        #[cfg(any(
            target_os = "linux",
            target_os = "android",
            target_os = "freebsd",
            target_os = "netbsd",
            target_os = "openbsd",
            target_os = "dragonfly",
            target_os = "illumos",
            target_os = "solaris"
        ))]
        // SAFETY: pipe2 writes two new descriptors into the array it is given.
        let made = unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) };
        #[cfg(not(any(
            target_os = "linux",
            target_os = "android",
            target_os = "freebsd",
            target_os = "netbsd",
            target_os = "openbsd",
            target_os = "dragonfly",
            target_os = "illumos",
            target_os = "solaris"
        )))]
        // SAFETY: pipe writes two new descriptors into the array it is given.
        let made = unsafe { libc::pipe(fds.as_mut_ptr()) };
        if made != 0 {
            return Err(std::io::Error::last_os_error());
        }
        // SAFETY: both descriptors were just created and belong to nothing else.
        let (read, write) = unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) };
        // Where there is no pipe2, a child another thread starts between `pipe` and here inherits
        // both ends. That window cannot be closed without pipe2; the secret is written only after
        // the flags are set, so what such a child holds is the pipe, not yet the bytes.
        #[cfg(not(any(
            target_os = "linux",
            target_os = "android",
            target_os = "freebsd",
            target_os = "netbsd",
            target_os = "openbsd",
            target_os = "dragonfly",
            target_os = "illumos",
            target_os = "solaris"
        )))]
        for end in [&read, &write] {
            // SAFETY: `end` is a descriptor this function owns; F_SETFD only sets its flags.
            if unsafe { libc::fcntl(end.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) } < 0 {
                return Err(std::io::Error::last_os_error());
            }
        }
        let (read, write) = (above_standard(read)?, above_standard(write)?);
        // The pipe is empty and nothing reads it yet, so a write that would wait must fail instead.
        // SAFETY: `write` is a descriptor this function owns; these calls read and set its flags.
        let flags = unsafe { libc::fcntl(write.as_raw_fd(), libc::F_GETFL) };
        // SAFETY: as above.
        if flags < 0 || unsafe { libc::fcntl(write.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
            return Err(std::io::Error::last_os_error());
        }
        let written = loop {
            // SAFETY: the pointer and length are those of a live slice; `write` is open. The host
            // holds the read end, so the write cannot raise SIGPIPE.
            let written = unsafe { libc::write(write.as_raw_fd(), secret.data.as_ptr().cast(), secret.data.len()) };
            if written >= 0 {
                break written.cast_unsigned();
            }
            let error = std::io::Error::last_os_error();
            if error.kind() != std::io::ErrorKind::Interrupted {
                return Err(error);
            }
        };
        if written != secret.data.len() {
            // Whatever part was written goes with the pipe, which nothing else has.
            return Err(std::io::Error::new(std::io::ErrorKind::InvalidInput, "the secret does not fit a pipe"));
        }
        // Closed before the child exists: the reader meets end of file after the last byte.
        drop(write);

        let descriptor = read.as_raw_fd();
        command.env(&secret.env, descriptor.to_string());
        // SAFETY: the closure runs in the child between fork and exec, after the child's standard
        // streams have been put on descriptors 0 to 2, and calls only `fcntl`, which is
        // async-signal-safe; it allocates nothing and touches no lock. `descriptor` is 3 or above
        // and open in the child because the parent holds `read` until the child is started.
        unsafe {
            command.pre_exec(move || {
                if libc::fcntl(descriptor, libc::F_SETFD, 0) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        Ok(read)
    }
}

#[cfg(windows)]
mod windows {
    use std::ffi::c_void;
    use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle, RawHandle};
    use std::process::Command;
    use std::sync::{Mutex, MutexGuard, PoisonError};

    use super::Secret;

    const HANDLE_FLAG_INHERIT: u32 = 1;
    const PIPE_NOWAIT: u32 = 1;
    /// The pipe's buffer: the largest secret twice over, so the one write into it fits.
    const PIPE_BYTES: u32 = 8192;

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn CreatePipe(read: *mut RawHandle, write: *mut RawHandle, attributes: *const c_void, size: u32) -> i32;
        fn SetHandleInformation(handle: RawHandle, mask: u32, flags: u32) -> i32;
        fn SetNamedPipeHandleState(
            handle: RawHandle,
            mode: *const u32,
            collection: *const u32,
            timeout: *const u32,
        ) -> i32;
        fn WriteFile(
            handle: RawHandle,
            buffer: *const u8,
            length: u32,
            written: *mut u32,
            overlapped: *mut c_void,
        ) -> i32;
    }

    /// Every child is started under this, so that a handle marked inheritable for one start is not
    /// inheritable during another.
    static STARTS: Mutex<()> = Mutex::new(());

    pub(super) struct Starting {
        // Declared, and so dropped, before the lock: the handle stops being inheritable, then is
        // closed, and only then may another child be started.
        read: Option<OwnedHandle>,
        _lock: MutexGuard<'static, ()>,
    }

    impl Drop for Starting {
        fn drop(&mut self) {
            if let Some(read) = &self.read {
                // SAFETY: `read` is an open handle this value owns. A failure leaves it inheritable
                // for the instant before it is closed just below, still under the lock.
                unsafe { SetHandleInformation(read.as_raw_handle(), HANDLE_FLAG_INHERIT, 0) };
            }
        }
    }

    /// A pipe holding `secret`, its write end closed, and the read end, which is not inheritable
    /// yet. Names the read end's value in `command`'s environment.
    fn filled(command: &mut Command, secret: &Secret) -> std::io::Result<OwnedHandle> {
        let too_large = || std::io::Error::new(std::io::ErrorKind::InvalidInput, "the secret does not fit a pipe");
        let (mut read, mut write): (RawHandle, RawHandle) = (std::ptr::null_mut(), std::ptr::null_mut());
        // SAFETY: CreatePipe writes two new handles through the pointers it is given; with no
        // security attributes neither is inheritable.
        if unsafe { CreatePipe(&raw mut read, &raw mut write, std::ptr::null(), PIPE_BYTES) } == 0 {
            return Err(std::io::Error::last_os_error());
        }
        // SAFETY: both handles were just created and belong to nothing else.
        let (read, write) = unsafe { (OwnedHandle::from_raw_handle(read), OwnedHandle::from_raw_handle(write)) };
        // Nothing reads the pipe yet, so a write that would wait must fail instead.
        let mode = PIPE_NOWAIT;
        // SAFETY: `write` is an open pipe handle; the mode is read through a pointer to a local.
        if unsafe {
            SetNamedPipeHandleState(write.as_raw_handle(), &raw const mode, std::ptr::null(), std::ptr::null())
        } == 0
        {
            return Err(std::io::Error::last_os_error());
        }
        let length = u32::try_from(secret.data.len()).map_err(|_| too_large())?;
        let mut taken: u32 = 0;
        // SAFETY: the pointer and length are those of a live slice of at most MAX_BYTES; `write` is
        // open; no overlapped structure is used.
        let succeeded = unsafe {
            WriteFile(write.as_raw_handle(), secret.data.as_ptr(), length, &raw mut taken, std::ptr::null_mut())
        };
        if succeeded == 0 {
            return Err(std::io::Error::last_os_error());
        }
        if taken != length {
            // Whatever part was written goes with the pipe, which nothing else has.
            return Err(too_large());
        }
        // Closed before the child exists: the reader meets end of file after the last byte.
        drop(write);
        // A handle's value is what the child opens it by; it is the same number in the child.
        command.env(&secret.env, (read.as_raw_handle() as usize).to_string());
        Ok(read)
    }

    pub(super) fn prepare(command: &mut Command, secret: Option<&Secret>) -> std::io::Result<Starting> {
        // Made and filled before the lock is taken: the handles are not inheritable yet, so no
        // child can inherit them, and a slow call here holds up no other start.
        let read = secret.map(|secret| filled(command, secret)).transpose()?;
        let lock = STARTS.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(read) = &read {
            // SAFETY: `read` is an open handle this function owns.
            if unsafe { SetHandleInformation(read.as_raw_handle(), HANDLE_FLAG_INHERIT, HANDLE_FLAG_INHERIT) } == 0 {
                return Err(std::io::Error::last_os_error());
            }
        }
        Ok(Starting { read, _lock: lock })
    }
}
