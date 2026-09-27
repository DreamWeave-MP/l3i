//! Calling Lua functions from Rust (`components/luau/call.hpp`, `FunctionView::invoke`).
//!
//! Every call goes through `lua_pcall`, so a callee error never unwinds into Rust: it comes
//! back as `Err`, with the stack restored by the frame. The invocation API is non-yielding; a
//! callee that yields through it is an error (Luau raises "attempt to yield across
//! metamethod/C-call boundary" itself on the main thread).
//!
//! Two error wordings are kept from the C++ binder on purpose: borrowed-view invocation says
//! `Lua error at stack index N: <message>`, pinned-function calls say `Lua error: <message>`.

use std::ffi::c_int;

use crate::convert::{FromView, Push};
use crate::error::{Error, Result};
use crate::raw::ffi;
use crate::stack::{Frame, Scope, Type, ValueView};
use crate::value::{Function, Value};

/// Arguments for a call: a tuple of [`Push`] values.
pub trait PushArgs {
    const COUNT: c_int;
    fn push_all<S: Scope>(&self, scope: &S) -> Result<()>;
}

impl PushArgs for () {
    const COUNT: c_int = 0;
    fn push_all<S: Scope>(&self, _: &S) -> Result<()> {
        Ok(())
    }
}

macro_rules! push_args_tuples {
    ($(($($name:ident),+) = $count:literal;)*) => {$(
        impl<$($name: Push),+> PushArgs for ($($name,)+) {
            const COUNT: c_int = $count;
            #[allow(non_snake_case)]
            fn push_all<S: Scope>(&self, scope: &S) -> Result<()> {
                let ($($name,)+) = self;
                $( $name.push_into(scope)?; )+
                Ok(())
            }
        }
    )*};
}

push_args_tuples! {
    (A) = 1;
    (A, B) = 2;
    (A, B, C) = 3;
    (A, B, C, D) = 4;
    (A, B, C, D, E) = 5;
    (A, B, C, D, E, F) = 6;
    (A, B, C, D, E, F, G) = 7;
    (A, B, C, D, E, F, G, H) = 8;
}

/// Results of a call, read from the frame slots `first..first + count`.
pub trait CallResults: Sized {
    /// Result count requested from `lua_pcall`; Lua pads with nil or truncates.
    const COUNT: c_int;
    fn read(frame: &Frame<'_>, first: c_int) -> Result<Self>;
}

impl CallResults for () {
    const COUNT: c_int = 0;
    fn read(_: &Frame<'_>, _: c_int) -> Result<()> {
        Ok(())
    }
}

/// Single-value results. Borrowing types (`&str`, views) are excluded on purpose: the result
/// slots are popped when the call frame closes.
macro_rules! single_results {
    ($($t:ty),* $(,)?) => {$(
        impl CallResults for $t {
            const COUNT: c_int = 1;
            fn read(frame: &Frame<'_>, first: c_int) -> Result<Self> {
                frame.at(first).read::<$t>()
            }
        }
    )*};
}

single_results!(
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
    Vec<u8>,
    crate::convert::Integer,
    crate::convert::Vector3,
    Value,
);

impl<T> CallResults for Option<T>
where
    for<'a> Option<T>: FromView<'a>,
{
    const COUNT: c_int = 1;
    fn read(frame: &Frame<'_>, first: c_int) -> Result<Self> {
        frame.at(first).read::<Option<T>>()
    }
}

impl CallResults for Function {
    const COUNT: c_int = 1;
    fn read(frame: &Frame<'_>, first: c_int) -> Result<Self> {
        Function::from_value(Value::store(frame.at(first))?)
    }
}

impl CallResults for crate::value::Table {
    const COUNT: c_int = 1;
    fn read(frame: &Frame<'_>, first: c_int) -> Result<Self> {
        crate::value::Table::from_value(Value::store(frame.at(first))?)
    }
}

macro_rules! tuple_results {
    ($(($($name:ident $index:tt),+) = $count:literal;)*) => {$(
        impl<$($name: CallResults),+> CallResults for ($($name,)+) {
            const COUNT: c_int = $count;
            fn read(frame: &Frame<'_>, first: c_int) -> Result<Self> {
                Ok(($( $name::read(frame, first + $index)?, )+))
            }
        }
    )*};
}

tuple_results! {
    (A 0, B 1) = 2;
    (A 0, B 1, C 2) = 3;
    (A 0, B 1, C 2, D 3) = 4;
    (A 0, B 1, C 2, D 3, E 4) = 5;
    (A 0, B 1, C 2, D 3, E 4, F 5) = 6;
}

/// How a failed `lua_pcall` is reported.
#[derive(Clone, Copy)]
enum ErrorStyle {
    /// `Lua error at stack index N: message` (`FunctionView::invoke`).
    BorrowedView,
    /// `Lua error: message` (`Lua::call` family).
    Pinned,
}

/// The error object on top of the frame as text: its string, or its type name.
fn error_text(frame: &Frame<'_>) -> String {
    let top = frame.at(-1);
    match top.read::<&str>() {
        Ok(text) => text.to_owned(),
        Err(_) => match top.read::<&[u8]>() {
            Ok(bytes) => String::from_utf8_lossy(bytes).into_owned(),
            Err(_) => crate::diagnostics::object_type_name(top),
        },
    }
}

/// The function must be at `function_index` with `nargs` arguments above it, all inside
/// `frame`. Runs `lua_pcall` and reports failure in `style`. On success the results occupy
/// `function_index..` and the count is returned.
fn perform_pcall(
    frame: &Frame<'_>,
    function_index: c_int,
    nargs: c_int,
    nresults: c_int,
    style: ErrorStyle,
) -> Result<c_int> {
    // SAFETY: function and arguments are on the frame; pcall never unwinds into Rust.
    let status = unsafe { ffi::lua_pcall(frame.state(), nargs, nresults, 0) };
    if status == ffi::LUA_OK {
        return Ok(frame.top() - function_index + 1);
    }
    if status == ffi::LUA_YIELD {
        return Err(Error::runtime("Lua function yielded through a non-yielding invocation"));
    }
    let message = if frame.top() >= function_index {
        error_text(frame)
    } else if status == ffi::LUA_ERRMEM {
        "out of memory".to_owned()
    } else {
        status.to_string()
    };
    Err(match style {
        ErrorStyle::BorrowedView => Error::runtime(format!("Lua error at stack index {function_index}: {message}")),
        ErrorStyle::Pinned => Error::runtime(format!("Lua error: {message}")),
    })
}

/// A borrowed view of a function slot.
#[derive(Clone, Copy, Debug)]
pub struct FunctionView<'v> {
    value: ValueView<'v>,
}

impl<'v> FunctionView<'v> {
    pub fn value(&self) -> ValueView<'v> {
        self.value
    }

    pub fn index(&self) -> c_int {
        self.value.index()
    }

    /// Calls the function with `args`, reading `R::COUNT` results, inside a nested frame on
    /// `frame`. Non-yielding.
    pub fn invoke<R: CallResults, A: PushArgs>(&self, frame: &Frame<'_>, args: A) -> Result<R> {
        if self.value.state() != frame.state() {
            return Err(Error::logic("Frame and function belong to different Lua threads"));
        }
        if !self.value.is_function() {
            return Err(self.value.type_error(Type::Function));
        }
        frame.with_frame(|call| {
            // SAFETY: the function slot exists; checkstack covers function, args and results.
            unsafe {
                if ffi::lua_checkstack(call.state(), A::COUNT + 1 + R::COUNT.max(0)) == 0 {
                    return Err(Error::runtime("Unable to grow the Lua stack for a function call"));
                }
                ffi::lua_pushvalue(call.state(), self.value.index());
            }
            let function_index = call.top();
            args.push_all(call)?;
            perform_pcall(call, function_index, A::COUNT, R::COUNT, ErrorStyle::BorrowedView)?;
            R::read(call, function_index)
        })
    }
}

impl<'v> ValueView<'v> {
    pub fn as_function(&self) -> Result<FunctionView<'v>> {
        if !self.is_function() {
            return Err(self.type_error(Type::Function));
        }
        Ok(FunctionView { value: *self })
    }
}

impl Function {
    /// Pushes the function and `args` onto a nested frame of `scope`, calls it, and reads
    /// `R::COUNT` results. Extra results are dropped, missing ones read as nil.
    pub fn invoke<R: CallResults, A: PushArgs>(&self, scope: &impl Scope, args: A) -> Result<R> {
        self.prepare(scope, |call| {
            let function_index = self.value().push_to(call)?.index();
            args.push_all(call)?;
            perform_pcall(call, function_index, A::COUNT, R::COUNT, ErrorStyle::Pinned)?;
            R::read(call, function_index)
        })
    }

    /// Like [`Function::invoke`] with a runtime-sized argument list of pinned values.
    pub fn invoke_with_values<R: CallResults>(&self, scope: &impl Scope, args: &[Value]) -> Result<R> {
        let nargs = c_int::try_from(args.len()).map_err(|_| Error::logic("Too many arguments"))?;
        self.prepare(scope, |call| {
            // SAFETY: live state; checkstack before pushing a runtime-sized list.
            if unsafe { ffi::lua_checkstack(call.state(), nargs + 1 + R::COUNT.max(0)) } == 0 {
                return Err(Error::runtime("Lua error: stack overflow"));
            }
            let function_index = self.value().push_to(call)?.index();
            for arg in args {
                arg.push_to(call)?;
            }
            perform_pcall(call, function_index, nargs, R::COUNT, ErrorStyle::Pinned)?;
            R::read(call, function_index)
        })
    }

    /// Calls with one result and hands the borrowed result to `visitor` while the call frame
    /// is alive. Lua supplies nil when the callee returns nothing.
    pub fn invoke_with<R, A: PushArgs>(
        &self,
        scope: &impl Scope,
        args: A,
        visitor: impl FnOnce(&Frame<'_>, ValueView<'_>) -> Result<R>,
    ) -> Result<R> {
        self.prepare(scope, |call| {
            let function_index = self.value().push_to(call)?.index();
            args.push_all(call)?;
            perform_pcall(call, function_index, A::COUNT, 1, ErrorStyle::Pinned)?;
            visitor(call, call.at(function_index))
        })
    }

    /// Calls with `LUA_MULTRET` and pins every result, left to right. At most 256 results;
    /// more is an error (`too many return values`).
    pub fn invoke_multi<A: PushArgs>(&self, scope: &impl Scope, args: A) -> Result<Vec<Value>> {
        const MAX_RESULTS: c_int = 256;
        self.prepare(scope, |call| {
            let function_index = self.value().push_to(call)?.index();
            args.push_all(call)?;
            let count = perform_pcall(call, function_index, A::COUNT, ffi::LUA_MULTRET, ErrorStyle::Pinned)?;
            if count > MAX_RESULTS {
                return Err(Error::runtime("Lua error: too many return values"));
            }
            let mut results = Vec::with_capacity(count as usize);
            for offset in 0..count {
                results.push(Value::store(call.at(function_index + offset))?);
            }
            Ok(results)
        })
    }

    fn prepare<R>(&self, scope: &impl Scope, body: impl FnOnce(&Frame<'_>) -> Result<R>) -> Result<R> {
        if !self.value().is_valid() {
            return Err(Error::logic("Cannot call an invalid Lua function reference"));
        }
        scope.with_frame(body)
    }
}
