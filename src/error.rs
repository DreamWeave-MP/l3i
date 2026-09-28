use std::fmt;

/// Errors produced by the binder.
///
/// `Logic` mirrors the C++ binder's `std::logic_error`: a programming mistake found at
/// registration or through API misuse. `Runtime` mirrors `std::runtime_error`: an ordinary
/// failure that becomes a Lua error message when it reaches a native entry point.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    Logic(String),
    Runtime(String),
    /// The Lua error object is already on top of the stack. The native entry point that
    /// receives this re-raises that object unchanged instead of replacing it with a message.
    LuaErrorOnStack,
    /// A capability the runtime policy did not grant, or a host facility a script may not use.
    /// Distinct from `Runtime` so a denial is never mistaken for an operating-system failure.
    Permission(String),
}

pub type Result<T> = std::result::Result<T, Error>;

impl Error {
    pub fn logic(message: impl Into<String>) -> Self {
        Error::Logic(message.into())
    }

    pub fn runtime(message: impl Into<String>) -> Self {
        Error::Runtime(message.into())
    }

    pub fn permission(message: impl Into<String>) -> Self {
        Error::Permission(message.into())
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Logic(message) | Error::Runtime(message) | Error::Permission(message) => f.write_str(message),
            Error::LuaErrorOnStack => f.write_str("Lua error"),
        }
    }
}

impl std::error::Error for Error {}
