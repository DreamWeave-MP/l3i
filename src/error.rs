use std::fmt;

/// Errors produced by the binder.
///
/// `Logic` mirrors the C++ binder's `std::logic_error`: a programming mistake found at
/// registration or through API misuse. `Runtime` mirrors `std::runtime_error`: an ordinary
/// failure that becomes a Lua error message when it reaches a native entry point.
#[derive(Debug)]
pub enum Error {
    Logic(String),
    Runtime(String),
    Lua(mlua::Error),
    /// The Lua error object is already on top of the stack. The native entry point that
    /// receives this re-raises that object unchanged instead of replacing it with a message.
    LuaErrorOnStack,
}

pub type Result<T> = std::result::Result<T, Error>;

impl Error {
    pub fn logic(message: impl Into<String>) -> Self {
        Error::Logic(message.into())
    }

    pub fn runtime(message: impl Into<String>) -> Self {
        Error::Runtime(message.into())
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Logic(message) | Error::Runtime(message) => f.write_str(message),
            Error::Lua(error) => write!(f, "{error}"),
            Error::LuaErrorOnStack => f.write_str("Lua error"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Lua(error) => Some(error),
            _ => None,
        }
    }
}

impl From<mlua::Error> for Error {
    fn from(error: mlua::Error) -> Self {
        Error::Lua(error)
    }
}

impl From<Error> for mlua::Error {
    fn from(error: Error) -> Self {
        match error {
            Error::Lua(error) => error,
            other => mlua::Error::runtime(other.to_string()),
        }
    }
}
