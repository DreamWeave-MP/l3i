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
    /// The watchdog stopped the call: its execution time limit ran out. Distinct from `Runtime`
    /// so a host can tell a timeout from a script's own error, whatever the script caught.
    ExecutionTimeLimit(String),
    /// The call needed memory past the runtime's memory limit, refused at the allocation.
    MemoryLimit(String),
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

    /// What `function` raises in a runtime whose policy doesn't grant `capability`, worded the same
    /// for every module and host.
    pub fn denied(function: &str, capability: &str) -> Self {
        Error::Permission(format!("{function}: needs the '{capability}' capability, which this runtime does not grant"))
    }

    /// True when a runtime limit, not the script or the host, ended the call.
    pub fn is_limit(&self) -> bool {
        matches!(self, Error::ExecutionTimeLimit(_) | Error::MemoryLimit(_))
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Logic(message)
            | Error::Runtime(message)
            | Error::Permission(message)
            | Error::ExecutionTimeLimit(message)
            | Error::MemoryLimit(message) => f.write_str(message),
            Error::LuaErrorOnStack => f.write_str("Lua error"),
        }
    }
}

impl std::error::Error for Error {}
