//! Return adapters (`bindfunction.hpp`: `pushReturn` and the wrapper types).

use std::ffi::c_int;

use super::Call;
use crate::convert::{Integer, Push, Vector3};
use crate::error::Result;
use crate::stack::Scope;
use crate::value::{Function, Table, Value};

/// A callable's result, pushed as zero or more Lua values.
pub trait Return {
    /// Pushes the results onto the call and returns how many there are.
    fn push_results(self, call: &Call<'_>) -> Result<c_int>;
}

impl Return for () {
    #[inline]
    fn push_results(self, _: &Call<'_>) -> Result<c_int> {
        Ok(0)
    }
}

macro_rules! single_returns {
    ($($t:ty),* $(,)?) => {$(
        impl Return for $t {
            #[inline]
            fn push_results(self, call: &Call<'_>) -> Result<c_int> {
                Push::push_only(&self, call)?;
                Ok(1)
            }
        }
    )*};
}

single_returns!(
    bool,
    i8,
    i16,
    i32,
    i64,
    isize,
    u8,
    u16,
    u32,
    u64,
    usize,
    f32,
    f64,
    String,
    &'static str,
    Vec<u8>,
    Integer,
    crate::convert::Bits64,
    Vector3,
    Value,
    Table,
    Function,
);

/// `None` is one nil result; `Some` pushes the inner value's results.
impl<T: Return> Return for Option<T> {
    #[inline]
    fn push_results(self, call: &Call<'_>) -> Result<c_int> {
        match self {
            Some(value) => value.push_results(call),
            None => {
                call.push(&())?;
                Ok(1)
            }
        }
    }
}

/// `Err` is raised as the Lua error; `Ok` pushes the inner results.
impl<T: Return> Return for Result<T> {
    #[inline]
    fn push_results(self, call: &Call<'_>) -> Result<c_int> {
        self?.push_results(call)
    }
}

macro_rules! tuple_returns {
    ($(($($name:ident),+) = $count:literal;)*) => {$(
        impl<$($name: Push),+> Return for ($($name,)+) {
            #[allow(non_snake_case)]
            #[inline]
            fn push_results(self, call: &Call<'_>) -> Result<c_int> {
                let ($($name,)+) = self;
                $( call.push(&$name)?; )+
                Ok($count)
            }
        }
    )*};
}

tuple_returns! {
    (A, B) = 2;
    (A, B, C) = 3;
    (A, B, C, D) = 4;
    (A, B, C, D, E) = 5;
    (A, B, C, D, E, F) = 6;
}

/// Multi-return: every element becomes its own result.
#[derive(Debug, Default)]
pub struct Variadic<T>(pub Vec<T>);

impl<T: Push> Return for Variadic<T> {
    #[inline]
    fn push_results(self, call: &Call<'_>) -> Result<c_int> {
        call.stack().check(c_int::try_from(self.0.len()).unwrap_or(c_int::MAX))?;
        for item in &self.0 {
            call.push(item)?;
        }
        Ok(self.0.len() as c_int)
    }
}

/// Call result wrapper: success pushes one value, failure pushes nil plus the message.
#[derive(Debug)]
pub enum ResultOrError<T> {
    Success(T),
    Failure(String),
}

impl<T: Push> Return for ResultOrError<T> {
    #[inline]
    fn push_results(self, call: &Call<'_>) -> Result<c_int> {
        match self {
            ResultOrError::Success(value) => {
                call.push(&value)?;
                Ok(1)
            }
            ResultOrError::Failure(message) => {
                call.push(&())?;
                call.push(&message)?;
                Ok(2)
            }
        }
    }
}

/// Conditional two-value return: the value on success, `(nil, value)` on failure.
#[derive(Debug)]
pub struct NilThen<T> {
    pub success: bool,
    pub value: T,
}

impl<T> NilThen<T> {
    pub fn success(value: T) -> Self {
        NilThen { success: true, value }
    }
    pub fn failure(value: T) -> Self {
        NilThen { success: false, value }
    }
}

impl<T: Push> Return for NilThen<T> {
    #[inline]
    fn push_results(self, call: &Call<'_>) -> Result<c_int> {
        if !self.success {
            call.push(&())?;
        }
        call.push(&self.value)?;
        Ok(if self.success { 1 } else { 2 })
    }
}

/// Marker return for callables that push all results onto the call themselves.
#[derive(Debug, Default, Clone, Copy)]
pub struct StackResults;

impl Return for StackResults {
    #[inline]
    fn push_results(self, call: &Call<'_>) -> Result<c_int> {
        Ok(call.result_count())
    }
}

/// Yields the inner results to the coroutine resuming this function (`lua_yield`). The bound
/// function must be running inside a coroutine with no C-call boundary in between; otherwise
/// Luau raises "attempt to yield across metamethod/C-call boundary".
pub struct Yield<T: Return>(pub T);

impl<T: Return> Return for Yield<T> {
    #[inline]
    fn push_results(self, call: &Call<'_>) -> Result<c_int> {
        let count = self.0.push_results(call)?;
        // SAFETY: the results are on the stack; lua_yield records them and returns the value the
        // C function must return to Luau.
        Ok(unsafe { crate::raw::ffi::lua_yield(call.state(), count) })
    }
}

/// Requests a debugger break (`lua_break`): the running thread stops with `LUA_BREAK` and can
/// be resumed by the host.
pub struct Break;

impl Return for Break {
    #[inline]
    fn push_results(self, call: &Call<'_>) -> Result<c_int> {
        // SAFETY: lua_break only records the request and returns the C function's return value.
        Ok(unsafe { crate::raw::ffi::lua_break(call.state()) })
    }
}
