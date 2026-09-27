use super::{FromView, Push};
use crate::error::{Error, Result};
use crate::raw::ffi;
use crate::stack::{Scope, Type, ValueView};

/// The bytes of a `string` slot, borrowed for the view's lifetime. Luau strings are immutable
/// and never moved by the collector, and the slot keeps the string reachable.
fn string_bytes<'v>(view: ValueView<'v>) -> Result<&'v [u8]> {
    if !view.is_string() {
        return Err(view.type_error(Type::String));
    }
    let mut length = 0usize;
    // SAFETY: the view proved the slot exists and holds a string; the pointer is valid for
    // `length` bytes while the slot lives, which the returned lifetime encodes.
    unsafe {
        let text = ffi::lua_tolstring(view.state(), view.index(), &mut length);
        if text.is_null() {
            return Err(view.type_error(Type::String));
        }
        Ok(std::slice::from_raw_parts(text.cast::<u8>(), length))
    }
}

impl<'v> FromView<'v> for &'v [u8] {
    const EXPECTED: &'static str = "string";

    fn from_view(view: ValueView<'v>) -> Result<Self> {
        string_bytes(view)
    }

    fn matches(view: ValueView<'v>) -> bool {
        view.is_string()
    }
}

impl<'v> FromView<'v> for &'v str {
    const EXPECTED: &'static str = "string";

    /// Borrowed; a string that is not valid UTF-8 is an error, never a lossy copy.
    fn from_view(view: ValueView<'v>) -> Result<Self> {
        std::str::from_utf8(string_bytes(view)?).map_err(|_| Error::runtime("Lua string is not valid UTF-8"))
    }

    fn matches(view: ValueView<'v>) -> bool {
        string_bytes(view).is_ok_and(|bytes| std::str::from_utf8(bytes).is_ok())
    }
}

impl<'v> FromView<'v> for String {
    const EXPECTED: &'static str = "string";

    fn from_view(view: ValueView<'v>) -> Result<Self> {
        <&str>::from_view(view).map(str::to_owned)
    }

    fn matches(view: ValueView<'v>) -> bool {
        <&str>::matches(view)
    }
}

impl<'v> FromView<'v> for Vec<u8> {
    const EXPECTED: &'static str = "string";

    fn from_view(view: ValueView<'v>) -> Result<Self> {
        string_bytes(view).map(<[u8]>::to_vec)
    }

    fn matches(view: ValueView<'v>) -> bool {
        view.is_string()
    }
}

fn push_bytes<'s, S: Scope>(scope: &'s S, bytes: &[u8]) -> Result<ValueView<'s>> {
    // SAFETY: lua_pushlstring copies `bytes.len()` bytes; an empty slice's pointer is not read.
    unsafe { ffi::lua_pushlstring(scope.state(), bytes.as_ptr().cast(), bytes.len()) };
    Ok(scope.top_value())
}

impl Push for str {
    fn push<'s, S: Scope>(&self, scope: &'s S) -> Result<ValueView<'s>> {
        push_bytes(scope, self.as_bytes())
    }
}

impl Push for String {
    fn push<'s, S: Scope>(&self, scope: &'s S) -> Result<ValueView<'s>> {
        push_bytes(scope, self.as_bytes())
    }
}

impl Push for [u8] {
    fn push<'s, S: Scope>(&self, scope: &'s S) -> Result<ValueView<'s>> {
        push_bytes(scope, self)
    }
}

impl Push for Vec<u8> {
    fn push<'s, S: Scope>(&self, scope: &'s S) -> Result<ValueView<'s>> {
        push_bytes(scope, self)
    }
}
