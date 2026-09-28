use super::{FromView, Push};
use crate::error::Result;
use crate::stack::{Scope, ValueView};

impl<'v, T: FromView<'v>> FromView<'v> for Option<T> {
    const EXPECTED: &'static str = T::EXPECTED;

    /// Nil is `None`; anything else must convert as `T`. A nonexistent slot is not nil here:
    /// absence is the argument binder's concern, which handles absent/nil/mismatch itself.
    #[inline]
    fn from_view(view: ValueView<'v>) -> Result<Self> {
        if view.is_nil() {
            return Ok(None);
        }
        T::from_view(view).map(Some)
    }

    #[inline]
    fn matches(view: ValueView<'v>) -> bool {
        view.is_nil() || T::matches(view)
    }
}

impl<T: Push> Push for Option<T> {
    #[inline]
    fn push_into<'s, S: Scope>(&self, scope: &'s S) -> Result<ValueView<'s>> {
        match self {
            Some(value) => value.push_into(scope),
            None => ().push_into(scope),
        }
    }
}
