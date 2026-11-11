//! A one-shot secret for a child: bytes it can read once from a descriptor it inherits.
//!
//! A child's arguments are in `/proc/<pid>/cmdline` and in every process listing; its environment
//! is in `/proc/<pid>/environ` and is copied to everything it starts; a file is a file. None of
//! them is a place for a credential. What is left is a pipe.
//!
//! The secret is written, whole, into a pipe nobody else has, and the write end is closed. The
//! child inherits the read end; the number of that descriptor, and nothing else, is put in an
//! environment variable the caller names. Whoever reads the descriptor first gets the bytes and
//! then end of file: the child itself, or a helper it starts (a Git credential helper, say), which
//! inherits the descriptor in its turn. The bytes are never in an argument, an environment value or
//! a file, and once read they are in no pipe either.
//!
//! Only this child gets the descriptor. Both ends are close-on-exec from the moment they exist, so
//! a child another thread starts meanwhile inherits neither; the flag is cleared on the read end
//! in this child alone, between `fork` and `exec`. The parent's copy of the read end is closed as
//! soon as the child has been started, and its copy of the bytes is overwritten when it is dropped.
//!
//! The secret must fit the pipe without waiting, since nothing is reading yet: at most
//! [`MAX_BYTES`]. Unix only; elsewhere attaching answers `Unsupported`.

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

/// The parent's end of an attached secret. Drop it once the child has been started, or has failed
/// to start: that closes the last descriptor this process holds on the pipe.
pub(super) struct Channel {
    #[cfg(unix)]
    _read: std::os::fd::OwnedFd,
}

/// Prepares `command` to start a child that inherits `secret`.
///
/// # Errors
///
/// When the pipe cannot be made, when the secret does not fit it, or, off Unix, always.
#[cfg(unix)]
pub(super) fn attach(command: &mut Command, secret: &Secret) -> std::io::Result<Channel> {
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    use std::os::unix::process::CommandExt;

    let mut fds: [libc::c_int; 2] = [0; 2];
    #[cfg(any(target_os = "linux", target_os = "android", target_os = "freebsd"))]
    // SAFETY: pipe2 writes two new descriptors into the array it is given.
    let made = unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) };
    #[cfg(not(any(target_os = "linux", target_os = "android", target_os = "freebsd")))]
    // SAFETY: pipe writes two new descriptors into the array it is given.
    let made = unsafe { libc::pipe(fds.as_mut_ptr()) };
    if made != 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: both descriptors were just created and belong to nothing else.
    let (read, write) = unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) };
    // Where there is no pipe2, a child another thread starts between `pipe` and here inherits both
    // ends, and with them the secret once it is written. That window cannot be closed without
    // pipe2; it is why the write happens only after the flags are set.
    #[cfg(not(any(target_os = "linux", target_os = "android", target_os = "freebsd")))]
    for end in [&read, &write] {
        // SAFETY: `end` is a descriptor this function owns; F_SETFD only sets its flags.
        if unsafe { libc::fcntl(end.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) } < 0 {
            return Err(std::io::Error::last_os_error());
        }
    }
    // The pipe is empty and nothing reads it yet, so a write that would wait must fail instead.
    // SAFETY: `write` is a descriptor this function owns; these calls only read and set its flags.
    let flags = unsafe { libc::fcntl(write.as_raw_fd(), libc::F_GETFL) };
    // SAFETY: as above.
    if flags < 0 || unsafe { libc::fcntl(write.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: the pointer and length are those of a live slice; `write` is an open descriptor.
    let written = unsafe { libc::write(write.as_raw_fd(), secret.data.as_ptr().cast(), secret.data.len()) };
    if written < 0 {
        return Err(std::io::Error::last_os_error());
    }
    if written.cast_unsigned() != secret.data.len() {
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidInput, "the secret does not fit a pipe"));
    }
    // Closed before the child exists: the reader meets end of file after the last byte.
    drop(write);

    let descriptor = read.as_raw_fd();
    command.env(&secret.env, descriptor.to_string());
    // SAFETY: the closure runs in the child between fork and exec and calls only `fcntl`, which is
    // async-signal-safe; it allocates nothing and touches no lock. `descriptor` is open in the
    // child because the parent holds `read` until after the child is started.
    unsafe {
        command.pre_exec(move || {
            if libc::fcntl(descriptor, libc::F_SETFD, 0) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    Ok(Channel { _read: read })
}

/// Prepares `command` to start a child that inherits `secret`.
///
/// # Errors
///
/// Always, off Unix: there is no descriptor to inherit by number.
#[cfg(not(unix))]
pub(super) fn attach(_command: &mut Command, _secret: &Secret) -> std::io::Result<Channel> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "a secret channel needs an inherited descriptor, which this platform does not have",
    ))
}
