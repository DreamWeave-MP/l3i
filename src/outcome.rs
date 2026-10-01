//! What an operation the OS can refuse answers a script: its results, or `nil`, a message and the
//! error's kind, the way Luau's own `io.open` reports a failure. The script decides what a
//! missing file means; a bad argument is still an error.

use std::ffi::c_int;

use crate::bind::{Call, Return};
use crate::error::Result;
use crate::stack::Scope;

/// The Luau type of the kinds [`kind_of`] names.
pub(crate) const ERROR_KIND_TYPE: &str = "\"notFound\" | \"permissionDenied\" | \"alreadyExists\" | \"isADirectory\" \
    | \"notADirectory\" | \"directoryNotEmpty\" | \"readOnlyFilesystem\" | \"storageFull\" | \"crossesDevices\" \
    | \"invalidInput\" | \"invalidFilename\" | \"unsupported\" | \"other\"";

/// The name a script sees for an I/O error's kind.
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

/// An I/O call the OS refused: its message and the error's kind.
#[derive(Debug)]
pub(crate) struct Failure {
    pub(crate) message: String,
    pub(crate) kind: &'static str,
}

impl Failure {
    /// `dream.fs.<what>: <path>: <error>`.
    pub(crate) fn new(what: &str, path: &[u8], error: &std::io::Error) -> Failure {
        Failure::message(format!("dream.fs.{what}: {}: {error}", String::from_utf8_lossy(path)), error)
    }

    /// `message`, with `error`'s kind.
    pub(crate) fn message(message: String, error: &std::io::Error) -> Failure {
        Failure { message, kind: kind_of(error) }
    }
}

/// What an operation on the disk answers: its results, or `nil`, the message, and the kind.
pub(crate) enum Outcome<T> {
    Done(T),
    Failed(Failure),
}

impl<T> Outcome<T> {
    /// `result` as an outcome, its error named for `what` on `path`.
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
                call.push(failure.message.as_str())?;
                call.push(failure.kind)?;
                Ok(3)
            }
        }
    }
}

/// Unwraps a done outcome, or returns the failure from the enclosing function.
macro_rules! done {
    ($outcome:expr) => {
        match $outcome {
            $crate::outcome::Outcome::Done(value) => value,
            $crate::outcome::Outcome::Failed(failure) => return Ok($crate::outcome::Outcome::Failed(failure)),
        }
    };
}
pub(crate) use done;
