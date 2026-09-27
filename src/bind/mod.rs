//! The typed function binder (`components/lua/bindfunction.hpp`).
//!
//! A Rust closure becomes a Lua function whose parameters are materialised one checked
//! conversion each, positionally, with the C++ binder's rules:
//! - `&Call` is injected and consumes no argument;
//! - `ValueView` borrows one argument slot without conversion;
//! - `Option<T>` maps absent and nil to `None`; a middle optional stays greedy and is skipped
//!   only when a following required parameter can consume the slot (nil disambiguates);
//! - `VarArgs<T>` and `ArgView` are terminators consuming every remaining argument;
//! - fixed arity rejects unused arguments, counts are validated before any conversion, and the
//!   first failing argument is reported with its position and expected type;
//! - `Overload((f1, f2, ...))` tries candidates in order and the first probe match commits.
//!
//! Returns follow [`Return`]: unit, values, `Option`, tuples, `Variadic`, `ResultOrError`,
//! `NilThen`, `StackResults`, and `Result<T>` (whose `Err` is raised).
//!
//! The closure is moved into a Lua-owned userdata (upvalue 1 of the C closure) and dropped by
//! the collector, so captures must not touch the Lua API in `Drop` and must be `'static`.
//! Bound closures are `Fn`, not `FnMut`: a binding can re-enter itself through Lua, so state
//! lives in `Cell`/`RefCell` captures instead. (The C++ binder allowed mutable callables; see
//! QUESTIONABLE.md.)

mod call;
mod diagnostics;
mod param;
mod params;
mod returns;

#[cfg(test)]
mod tests;

use std::ffi::{c_char, c_int, c_void};
use std::ptr;

pub use call::Call;
pub use param::{ArgView, Param, ParamItem, ParamKind, VarArgs};
pub use params::Params;
pub use returns::{NilThen, ResultOrError, Return, StackResults, Variadic};

use crate::debug_name;
use crate::error::{Error, Result};
use crate::raw::{ffi, trampoline};
use crate::stack::Scope;
use crate::userdata::assert_userdata_layout;
use crate::value::{Function, Value};

/// A Rust callable that the binder can expose. `Marker` is inferred from the callable's
/// signature; users never name it.
pub trait Binding<Marker>: 'static {
    /// Parameter kinds in order; builders use them to validate member signatures.
    const PARAM_KINDS: &'static [ParamKind];
    /// The expected-type name of the first parameter, i.e. the receiver type of a method.
    const RECEIVER_NAME: Option<&'static str>;

    /// True when the arguments currently on the stack fit this signature exactly enough to
    /// commit to it during overload resolution.
    fn probe_for_overload(&self, call: &Call<'_>) -> bool;

    /// Materialises the arguments, runs the callable, pushes the results.
    fn invoke(&self, call: &Call<'_>, debug_name: &str) -> Result<c_int>;

    /// Method mode: slot 1 is the receiver, validated with the receiver type's own error and
    /// excluded from argument numbering; remaining parameters start at slot 2.
    fn invoke_method(&self, call: &Call<'_>, debug_name: &str) -> Result<c_int>;
}

/// Ordered overload set: candidates are tried in declaration order and the first whose probe
/// matches commits, so its conversion errors then propagate. Nested overload sets never match.
pub struct Overload<Candidates>(pub Candidates);

macro_rules! binding_impls {
    ($(($($p:ident $m:ident),*) ;)*) => {$(
        impl<Func, Ret, $($p,)*> Binding<fn($($p,)*) -> Ret> for Func
        where
            Func: Fn($($p,)*) -> Ret + for<'c> Fn($(<$p as Param>::Item<'c>,)*) -> Ret + 'static,
            $($p: Param,)*
            Ret: Return,
        {
            const PARAM_KINDS: &'static [ParamKind] = <($($p,)*) as Params>::KINDS;
            const RECEIVER_NAME: Option<&'static str> = <($($p,)*) as Params>::RECEIVER_NAME;

            fn probe_for_overload(&self, call: &Call<'_>) -> bool {
                <($($p,)*) as Params>::probe_for_overload(call)
            }

            #[allow(non_snake_case, unused_variables)]
            fn invoke(&self, call: &Call<'_>, debug_name: &str) -> Result<c_int> {
                let ($($m,)*) = <($($p,)*) as Params>::materialize(call, debug_name)?;
                let result = self($($m,)*);
                result.push_results(call)
            }

            #[allow(non_snake_case, unused_variables)]
            fn invoke_method(&self, call: &Call<'_>, debug_name: &str) -> Result<c_int> {
                let ($($m,)*) = <($($p,)*) as Params>::materialize_method(call, debug_name)?;
                let result = self($($m,)*);
                result.push_results(call)
            }
        }
    )*};
}

binding_impls! {
    ();
    (A a);
    (A a, B b);
    (A a, B b, C c);
    (A a, B b, C c, D d);
    (A a, B b, C c, D d, E e);
    (A a, B b, C c, D d, E e, F f);
    (A a, B b, C c, D d, E e, F f, G g);
    (A a, B b, C c, D d, E e, F f, G g, H h);
}

macro_rules! overload_impls {
    ($(($($f:ident $m:ident $i:tt),+) ;)*) => {$(
        impl<$($f, $m,)+> Binding<Overload<($($m,)+)>> for Overload<($($f,)+)>
        where
            $($f: Binding<$m>, $m: 'static,)+
        {
            const PARAM_KINDS: &'static [ParamKind] = &[];
            const RECEIVER_NAME: Option<&'static str> = None;

            fn probe_for_overload(&self, _: &Call<'_>) -> bool {
                // An overload set never matches wholesale when nested inside another.
                false
            }

            fn invoke(&self, call: &Call<'_>, debug_name: &str) -> Result<c_int> {
                $(
                    if self.0.$i.probe_for_overload(call) {
                        return self.0.$i.invoke(call, debug_name);
                    }
                )+
                Err(diagnostics::no_matching_overload(debug_name))
            }

            fn invoke_method(&self, _: &Call<'_>, debug_name: &str) -> Result<c_int> {
                Err(Error::logic(format!("{debug_name}: overload sets cannot be bound as methods")))
            }
        }
    )*};
}

overload_impls! {
    (F0 M0 0);
    (F0 M0 0, F1 M1 1);
    (F0 M0 0, F1 M1 1, F2 M2 2);
    (F0 M0 0, F1 M1 1, F2 M2 2, F3 M3 3);
    (F0 M0 0, F1 M1 1, F2 M2 2, F3 M3 3, F4 M4 4);
}

/// Lua-owned closure state: the callable and its retained debug name.
#[repr(C)]
struct Context<F> {
    callable: F,
    debug_name: *const c_char,
}

/// Runs the callable's `Drop` when Luau frees the closure context. Never touches Lua.
unsafe extern "C" fn destroy_context<F>(_: *mut ffi::lua_State, userdata: *mut c_void) {
    // SAFETY: only `push_closure::<F>` creates userdata with this destructor, and it wrote a
    // valid `Context<F>` before anything could observe the slot.
    let outcome = std::panic::catch_unwind(|| unsafe { ptr::drop_in_place(userdata.cast::<Context<F>>()) });
    if outcome.is_err() {
        std::process::abort();
    }
}

/// Reads the context from upvalue 1 and runs `body` with it under the trampoline.
unsafe fn with_context<F: Binding<M>, M>(
    state: *mut ffi::lua_State,
    body: impl FnOnce(&F, &Call<'_>, &str) -> Result<c_int>,
) -> c_int {
    unsafe {
        trampoline::enter(state, || {
            let context = ffi::lua_touserdata(state, ffi::lua_upvalueindex(1)).cast::<Context<F>>();
            if context.is_null() {
                return Err(Error::logic("Invalid native Lua binding context"));
            }
            // SAFETY: upvalue 1 is the context userdata `push_closure` created for this thunk.
            let context = &*context;
            let debug_name = std::ffi::CStr::from_ptr(context.debug_name).to_str().unwrap_or("?");
            let call = Call::from_raw(state);
            body(&context.callable, &call, debug_name)
        })
    }
}

/// The C closure every function binding runs through.
unsafe extern "C-unwind" fn thunk<F: Binding<M>, M>(state: *mut ffi::lua_State) -> c_int {
    unsafe { with_context::<F, M>(state, |callable, call, name| callable.invoke(call, name)) }
}

/// The C closure every method binding runs through.
unsafe extern "C-unwind" fn method_thunk<F: Binding<M>, M>(state: *mut ffi::lua_State) -> c_int {
    unsafe { with_context::<F, M>(state, |callable, call, name| callable.invoke_method(call, name)) }
}

/// Allocates the context userdata, moves `callable` into it, and pushes the C closure.
///
/// # Safety
/// `state` is live with room for two values; `debug_name` is retained for the VM's life.
unsafe fn push_closure<F: Binding<M>, M>(
    state: *mut ffi::lua_State,
    callable: F,
    debug_name: *const c_char,
    entry: ffi::lua_CFunction,
) -> Result<()> {
    const { assert_userdata_layout::<Context<F>>() };
    // SAFETY: allocate, then initialise immediately: Luau owns the destructor as soon as
    // lua_newuserdatadtor returns, and the destructor only runs on a fully written Context.
    unsafe {
        let storage = ffi::lua_newuserdatadtor(state, std::mem::size_of::<Context<F>>(), destroy_context::<F>);
        if storage.is_null() {
            return Err(Error::runtime("Unable to allocate binding closure context"));
        }
        ptr::write(storage.cast::<Context<F>>(), Context { callable, debug_name });
        ffi::lua_pushcclosure(state, entry, debug_name, 1);
    }
    Ok(())
}

/// Pushes a function-mode closure. `debug_name` must already be retained.
///
/// # Safety
/// As [`push_closure`].
pub(crate) unsafe fn function_closure<F: Binding<M>, M>(state: *mut ffi::lua_State, callable: F, debug_name: *const c_char) -> Result<()> {
    unsafe { push_closure(state, callable, debug_name, thunk::<F, M>) }
}

/// Pushes a method-mode closure. `debug_name` must already be retained.
///
/// # Safety
/// As [`push_closure`].
pub(crate) unsafe fn method_closure<F: Binding<M>, M>(state: *mut ffi::lua_State, callable: F, debug_name: *const c_char) -> Result<()> {
    unsafe { push_closure(state, callable, debug_name, method_thunk::<F, M>) }
}

/// Binds `callable` as a Lua function named `debug_name` (validated against `roots` and
/// retained for the VM's life) and returns it pinned. The stack of `scope` is left as it was.
pub fn function<F: Binding<M>, M>(scope: &impl Scope, roots: &[&str], debug_name: &str, callable: F) -> Result<Function> {
    scope.with_frame(|frame| {
        // SAFETY: frame state is live; retain rebalances the stack itself.
        unsafe {
            let retained = debug_name::retain(frame.state(), debug_name, roots)?;
            function_closure(frame.state(), callable, retained)?;
        }
        Function::from_value(Value::store(frame.top_value())?)
    })
}

impl crate::runtime::Runtime {
    /// Binds `callable` as a Lua function named `debug_name` under this runtime's debug roots.
    pub fn bind_function<F: Binding<M>, M>(&self, debug_name: &str, callable: F) -> Result<Function> {
        function(&self.stack(), self.debug_roots(), debug_name, callable)
    }
}
