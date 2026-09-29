//! Conversion between Lua values and Rust values (`components/luau/convert.hpp`).
//!
//! Conversion is explicit supported-type dispatch: a type either implements [`FromView`] /
//! [`Push`] with one checked conversion, or it does not convert. There is no probing, no
//! string-to-number coercion, and no silent fallback. The numeric rules are the C++ binder's
//! and are tested against its oracle:
//!
//! - Luau has two numeric runtime types. `Number` (f64) converts to an integer by rounding half
//!   away from zero and must land inside `[-2^digits, 2^digits)`; `Integer` (i64) converts by
//!   range check only. Non-finite numbers never convert to integers.
//! - Rust integers push as `Number` and must be exactly representable as an f64; the explicit
//!   [`Integer`] wrapper pushes a Luau 64-bit integer.
//! - `f32` rejects finite values outside its range; NaN and infinities pass through untouched.
//!   Finiteness is a domain rule for hosts to express with their own converter.
//! - Strings never come from numbers and numbers never come from strings.
//! - [`Vector3`] and [`BufferView`] are first-class: native Luau vectors and buffers are part of
//!   the performance contract, not an afterthought.

mod buffer;
mod exact;
mod option;
pub(crate) mod raw;
mod scalar;
mod string;
mod vector;

#[cfg(test)]
mod tests;

use crate::error::Result;
use crate::stack::{Scope, ValueView};

pub use buffer::{BufferView, BytesView, NewBuffer, new_buffer};
pub use exact::{Bits64, Exact};
pub use raw::RawValue;
pub use scalar::Integer;
pub(crate) use scalar::read_integer64;
pub use vector::Vector3;

/// A Rust type readable from one stack slot with one checked conversion.
pub trait FromView<'v>: Sized {
    /// The name diagnostics use for this type ("number", "string", a userdata `__type`).
    const EXPECTED: &'static str;

    /// Converts, or returns the type/range error. This is the only conversion ordinary binding
    /// performs; it must not be preceded by [`FromView::matches`].
    fn from_view(view: ValueView<'v>) -> Result<Self>;

    /// [`FromView::from_view`] for an argument slot the binder can read directly (`raw` is
    /// that slot; `view` produces the same slot's view, for errors and for the fallback, only
    /// when asked). Types with a cheap direct read override this; the default converts
    /// through the API.
    #[inline]
    fn from_raw_arg(raw: &RawValue, view: impl FnOnce() -> ValueView<'v>) -> Result<Self> {
        let _ = raw;
        Self::from_view(view())
    }

    /// True when `from_view` would succeed. Used only for overload resolution and optional
    /// argument disambiguation; the default runs the conversion and discards the result, and
    /// scalar types override it with a cheaper check.
    #[inline]
    fn matches(view: ValueView<'v>) -> bool {
        Self::from_view(view).is_ok()
    }
}

/// A Rust type that can be pushed onto a scope as one Lua value.
pub trait Push {
    fn push_into<'s, S: Scope>(&self, scope: &'s S) -> Result<ValueView<'s>>;

    /// Pushes without producing a view of the slot; the hot return path uses this so scalar
    /// results cost one `lua_push*` and nothing else.
    #[inline]
    fn push_only<S: Scope>(&self, scope: &S) -> Result<()> {
        self.push_into(scope).map(|_| ())
    }
}

impl<'v> FromView<'v> for ValueView<'v> {
    const EXPECTED: &'static str = "any value";

    /// Borrows the slot itself; a nonexistent slot is an error, nil is a value.
    #[inline]
    fn from_view(view: ValueView<'v>) -> Result<Self> {
        if !view.exists() {
            return Err(crate::error::Error::logic("Cannot read a nonexistent Lua stack value"));
        }
        Ok(view)
    }

    #[inline]
    fn matches(view: ValueView<'v>) -> bool {
        view.type_of() != crate::stack::Type::None
    }
}

impl Push for ValueView<'_> {
    #[inline]
    fn push_into<'s, S: Scope>(&self, scope: &'s S) -> Result<ValueView<'s>> {
        crate::stack::push_copy(scope.state(), *self)?;
        Ok(scope.top_value())
    }
}

impl<T: Push + ?Sized> Push for &T {
    #[inline]
    fn push_into<'s, S: Scope>(&self, scope: &'s S) -> Result<ValueView<'s>> {
        (**self).push_into(scope)
    }
}

impl Push for crate::value::Value {
    #[inline]
    fn push_into<'s, S: Scope>(&self, scope: &'s S) -> Result<ValueView<'s>> {
        self.push_to_scope(scope)
    }
}

impl Push for crate::value::Table {
    #[inline]
    fn push_into<'s, S: Scope>(&self, scope: &'s S) -> Result<ValueView<'s>> {
        self.value().push_to_scope(scope)
    }
}

impl Push for crate::value::Function {
    #[inline]
    fn push_into<'s, S: Scope>(&self, scope: &'s S) -> Result<ValueView<'s>> {
        self.value().push_to_scope(scope)
    }
}

impl<'v> FromView<'v> for crate::value::Value {
    const EXPECTED: &'static str = "any value";

    #[inline]
    fn from_view(view: ValueView<'v>) -> Result<Self> {
        crate::value::Value::store(view)
    }

    #[inline]
    fn matches(view: ValueView<'v>) -> bool {
        view.type_of() != crate::stack::Type::None && view.index() != crate::raw::ffi::LUA_REGISTRYINDEX
    }
}

impl<'v> ValueView<'v> {
    /// One checked conversion of this slot.
    pub fn read<T: FromView<'v>>(self) -> Result<T> {
        T::from_view(self)
    }

    /// True when [`ValueView::read`] would succeed; for overload probing only.
    pub fn is<T: FromView<'v>>(self) -> bool {
        T::matches(self)
    }
}
