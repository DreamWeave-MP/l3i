//! What an operation the OS can refuse answers a script: its results, or `nil`, a message and the
//! error's kind, the way Luau's own `io.open` reports a failure. The script decides what a
//! missing file means; a bad argument is still an error.

use std::borrow::Cow;
use std::ffi::c_int;

use crate::bind::{Call, Return};
use crate::error::Result;
use crate::stack::Scope;

/// The Luau type of the kinds [`kind_of`] names.
#[cfg(any(feature = "fs", feature = "process"))]
pub(crate) const ERROR_KIND_TYPE: &str = "\"notFound\" | \"permissionDenied\" | \"alreadyExists\" | \"isADirectory\" \
    | \"notADirectory\" | \"directoryNotEmpty\" | \"readOnlyFilesystem\" | \"storageFull\" | \"crossesDevices\" \
    | \"invalidInput\" | \"invalidFilename\" | \"unsupported\" | \"other\"";

/// The name a script sees for an I/O error's kind.
#[cfg(any(feature = "fs", feature = "process"))]
pub(crate) fn kind_of(error: &std::io::Error) -> &'static str {
    use std::io::ErrorKind;
    match error.kind() {
        ErrorKind::NotFound => "notFound",
        ErrorKind::PermissionDenied => "permissionDenied",
        ErrorKind::AlreadyExists => "alreadyExists",
        ErrorKind::IsADirectory => "isADirectory",
        ErrorKind::NotADirectory => "notADirectory",
        ErrorKind::DirectoryNotEmpty => "directoryNotEmpty",
        ErrorKind::ReadOnlyFilesystem => "readOnlyFilesystem",
        ErrorKind::StorageFull => "storageFull",
        ErrorKind::CrossesDevices => "crossesDevices",
        ErrorKind::InvalidInput => "invalidInput",
        ErrorKind::InvalidFilename => "invalidFilename",
        ErrorKind::Unsupported => "unsupported",
        _ => "other",
    }
}

/// The Luau type of the kinds [`network_kind_of`] names.
#[cfg(feature = "tcp")]
pub(crate) const NETWORK_ERROR_KIND_TYPE: &str = "\"wouldBlock\" | \"connectionRefused\" | \"connectionReset\" \
    | \"connectionAborted\" | \"notConnected\" | \"brokenPipe\" | \"addressInUse\" | \"addressNotAvailable\" \
    | \"timedOut\" | \"hostUnreachable\" | \"networkUnreachable\" | \"networkDown\" | \"permissionDenied\" \
    | \"limitReached\" | \"invalidInput\" | \"unsupported\" | \"other\"";

/// The name a script sees for a socket error's kind. `limitReached` is never an OS error: it
/// is a bound the script set.
#[cfg(feature = "tcp")]
pub(crate) fn network_kind_of(error: &std::io::Error) -> &'static str {
    use std::io::ErrorKind;
    match error.kind() {
        ErrorKind::WouldBlock => "wouldBlock",
        ErrorKind::ConnectionRefused => "connectionRefused",
        ErrorKind::ConnectionReset => "connectionReset",
        ErrorKind::ConnectionAborted => "connectionAborted",
        ErrorKind::NotConnected => "notConnected",
        ErrorKind::BrokenPipe => "brokenPipe",
        ErrorKind::AddrInUse => "addressInUse",
        ErrorKind::AddrNotAvailable => "addressNotAvailable",
        ErrorKind::TimedOut => "timedOut",
        ErrorKind::HostUnreachable => "hostUnreachable",
        ErrorKind::NetworkUnreachable => "networkUnreachable",
        ErrorKind::NetworkDown => "networkDown",
        ErrorKind::PermissionDenied => "permissionDenied",
        ErrorKind::InvalidInput => "invalidInput",
        ErrorKind::Unsupported => "unsupported",
        _ => "other",
    }
}

/// An I/O call the OS refused: its message and the error's kind. A message that never varies
/// is borrowed, so an expected refusal (a socket with nothing to read) allocates nothing.
#[derive(Debug)]
pub(crate) struct Failure {
    pub(crate) message: Cow<'static, str>,
    pub(crate) kind: &'static str,
}

impl Failure {
    /// `dream.fs.<what>: <path>: <error>`.
    #[cfg(feature = "fs")]
    pub(crate) fn new(what: &str, path: &[u8], error: &std::io::Error) -> Failure {
        Failure::message(format!("dream.fs.{what}: {}: {error}", String::from_utf8_lossy(path)), error)
    }

    /// `message`, with `error`'s kind.
    #[cfg(any(feature = "fs", feature = "process"))]
    pub(crate) fn message(message: String, error: &std::io::Error) -> Failure {
        Failure { message: Cow::Owned(message), kind: kind_of(error) }
    }
}

/// What an operation on the disk answers: its results, or `nil`, the message, and the kind.
pub(crate) enum Outcome<T> {
    Done(T),
    Failed(Failure),
}

impl<T> Outcome<T> {
    /// `result` as an outcome, its error named for `what` on `path`.
    #[cfg(feature = "fs")]
    pub(crate) fn of(result: std::io::Result<T>, what: &str, path: &[u8]) -> Outcome<T> {
        match result {
            Ok(value) => Outcome::Done(value),
            Err(error) => Outcome::Failed(Failure::new(what, path, &error)),
        }
    }
}

impl<T: Return> Return for Outcome<T> {
    fn push_results(self, call: &Call<'_>) -> Result<c_int> {
        match self {
            Outcome::Done(value) => value.push_results(call),
            Outcome::Failed(failure) => {
                call.push(&())?;
                call.push(failure.message.as_ref())?;
                call.push(failure.kind)?;
                Ok(3)
            }
        }
    }
}

/// Unwraps a done outcome, or returns the failure from the enclosing function.
#[cfg(any(feature = "fs", feature = "process"))]
macro_rules! done {
    ($outcome:expr) => {
        match $outcome {
            $crate::outcome::Outcome::Done(value) => value,
            $crate::outcome::Outcome::Failed(failure) => return Ok($crate::outcome::Outcome::Failed(failure)),
        }
    };
}
#[cfg(any(feature = "fs", feature = "process"))]
pub(crate) use done;
