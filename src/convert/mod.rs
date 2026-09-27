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
mod option;
mod scalar;
mod string;
mod vector;

#[cfg(test)]
mod tests;

use crate::error::Result;
use crate::stack::{Scope, ValueView};

pub use buffer::{BufferView, new_buffer};
pub use scalar::Integer;
pub use vector::Vector3;

/// A Rust type readable from one stack slot with one checked conversion.
pub trait FromView<'v>: Sized {
    /// The name diagnostics use for this type ("number", "string", a userdata `__type`).
    const EXPECTED: &'static str;

    /// Converts, or returns the type/range error. This is the only conversion ordinary binding
    /// performs; it must not be preceded by [`FromView::matches`].
    fn from_view(view: ValueView<'v>) -> Result<Self>;

    /// True when `from_view` would succeed. Used only for overload resolution and optional
    /// argument disambiguation; the default runs the conversion and discards the result, and
    /// scalar types override it with a cheaper check.
    fn matches(view: ValueView<'v>) -> bool {
        Self::from_view(view).is_ok()
    }
}

/// A Rust type that can be pushed onto a scope as one Lua value.
pub trait Push {
    fn push_into<'s, S: Scope>(&self, scope: &'s S) -> Result<ValueView<'s>>;
}

impl<'v> FromView<'v> for ValueView<'v> {
    const EXPECTED: &'static str = "any value";

    /// Borrows the slot itself; a nonexistent slot is an error, nil is a value.
    fn from_view(view: ValueView<'v>) -> Result<Self> {
        if view.type_of() == crate::stack::Type::None {
            return Err(crate::error::Error::logic("Cannot read a nonexistent Lua stack value"));
        }
        Ok(view)
    }

    fn matches(view: ValueView<'v>) -> bool {
        view.type_of() != crate::stack::Type::None
    }
}

impl Push for ValueView<'_> {
    fn push_into<'s, S: Scope>(&self, scope: &'s S) -> Result<ValueView<'s>> {
        crate::stack::push_copy(scope.state(), *self)?;
        Ok(scope.top_value())
    }
}

impl<T: Push + ?Sized> Push for &T {
    fn push_into<'s, S: Scope>(&self, scope: &'s S) -> Result<ValueView<'s>> {
        (**self).push_into(scope)
    }
}

impl Push for crate::value::Value {
    fn push_into<'s, S: Scope>(&self, scope: &'s S) -> Result<ValueView<'s>> {
        self.push_to_scope(scope)
    }
}

impl Push for crate::value::Table {
    fn push_into<'s, S: Scope>(&self, scope: &'s S) -> Result<ValueView<'s>> {
        self.value().push_to_scope(scope)
    }
}

impl Push for crate::value::Function {
    fn push_into<'s, S: Scope>(&self, scope: &'s S) -> Result<ValueView<'s>> {
        self.value().push_to_scope(scope)
    }
}

impl<'v> FromView<'v> for crate::value::Value {
    const EXPECTED: &'static str = "any value";

    fn from_view(view: ValueView<'v>) -> Result<Self> {
        crate::value::Value::store(view)
    }

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
