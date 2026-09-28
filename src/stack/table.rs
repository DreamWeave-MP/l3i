use std::ffi::c_int;

use super::{Frame, ValueView};
use crate::convert::{FromView, Push};
use crate::diagnostics::{describe_value, truncate_diagnostic};
use crate::error::{Error, Result};
use crate::raw::ffi;

/// Luau raw integer keys are C `int`s (`lua_rawgeti`/`lua_rawseti`).
fn checked_raw_integer_key(key: i64) -> Result<c_int> {
    c_int::try_from(key).map_err(|_| Error::logic("Lua table integer key exceeds the supported range"))
}

/// A borrowed view of a table on the stack.
///
/// Lookups push their result and return a view bound to the frame you pass, exactly as the
/// C++ `TableView::get` left the value on the stack; the frame decides when it is popped.
/// Stores consume the value on top of that frame.
#[derive(Clone, Copy, Debug)]
pub struct TableView<'v> {
    value: ValueView<'v>,
}

impl<'v> TableView<'v> {
    pub(crate) fn new(value: ValueView<'v>) -> Self {
        TableView { value }
    }

    pub fn value(&self) -> ValueView<'v> {
        self.value
    }

    pub fn index(&self) -> c_int {
        self.value.index()
    }

    fn require_live(&self, frame: &Frame<'_>) -> Result<()> {
        if !self.value.exists() {
            return Err(Error::logic("Table view no longer names a live stack slot"));
        }
        if frame.state() != self.value.state() {
            return Err(Error::logic("Frame and table belong to different Lua threads"));
        }
        Ok(())
    }

    /// Honours `__index`; the result is pushed onto `frame`. A raising `__index` unwinds to
    /// Luau inside a call and becomes `Err` at host level.
    pub fn get<'f>(&self, frame: &'f Frame<'_>, key: &str) -> Result<ValueView<'f>> {
        self.require_live(frame)?;
        let state = frame.state();
        // SAFETY: the table slot exists; operands (table copy, key) sit on top for the raising
        // body, which consumes both and leaves one result.
        unsafe {
            if !self.has_metatable(state) {
                // No metatable means no `__index`: a raw read, which cannot raise. (Interning
                // the key can allocate, as every host-level string push does.)
                ffi::lua_pushlstring(state, key.as_ptr().cast(), key.len());
                ffi::lua_rawget(state, self.index());
                return Ok(frame.top_value());
            }
            ffi::lua_pushvalue(state, self.index());
            ffi::lua_pushlstring(state, key.as_ptr().cast(), key.len());
            frame.raising(2, 1, |state| {
                ffi::lua_gettable(state, -2);
                ffi::lua_remove(state, -2);
                1
            })?;
        }
        Ok(frame.top_value())
    }

    /// Bypasses `__index`; the result is pushed onto `frame`. Never raises.
    pub fn raw_get<'f>(&self, frame: &'f Frame<'_>, key: &str) -> Result<ValueView<'f>> {
        self.require_live(frame)?;
        let state = frame.state();
        unsafe {
            ffi::lua_pushlstring(state, key.as_ptr().cast(), key.len());
            ffi::lua_rawget(state, self.index());
        }
        Ok(frame.top_value())
    }

    /// Honours `__newindex`. Consumes the value on top of `frame`.
    pub fn set(&self, frame: &Frame<'_>, key: &str) -> Result<()> {
        self.require_store(frame)?;
        let state = frame.state();
        // SAFETY: operands (value, table copy, key) on top; the body consumes all three.
        unsafe {
            if !frame.is_host_level() && !self.has_metatable(state) && ffi::lua_getreadonly(state, self.index()) == 0 {
                // No `__newindex` and writable: a raw store. Inside a native call a raise (the
                // table growing out of memory) unwinds to Luau like any other, so the operand
                // shuffle of the raising path is skipped; at host level the store stays under
                // `protected_call`, since `lua_rawset` can allocate.
                ffi::lua_pushlstring(state, key.as_ptr().cast(), key.len());
                ffi::lua_insert(state, -2);
                ffi::lua_rawset(state, self.index());
                return Ok(());
            }
            ffi::lua_pushvalue(state, self.index());
            ffi::lua_pushlstring(state, key.as_ptr().cast(), key.len());
            frame.raising(3, 0, |state| {
                // [value, table, key] -> settable(table)[key] = value
                ffi::lua_insert(state, -3);
                ffi::lua_insert(state, -3);
                ffi::lua_settable(state, -3);
                ffi::lua_pop(state, 1);
                0
            })
        }
    }

    /// Bypasses `__newindex`. Consumes the value on top of `frame`. Raises (or fails at host
    /// level) when the table is read-only.
    pub fn raw_set(&self, frame: &Frame<'_>, key: &str) -> Result<()> {
        self.require_store(frame)?;
        let state = frame.state();
        unsafe {
            ffi::lua_pushvalue(state, self.index());
            ffi::lua_pushlstring(state, key.as_ptr().cast(), key.len());
            frame.raising(3, 0, |state| {
                ffi::lua_insert(state, -3);
                ffi::lua_insert(state, -3);
                ffi::lua_rawset(state, -3);
                ffi::lua_pop(state, 1);
                0
            })
        }
    }

    /// `t[key]` for an integer key (pushed as a Lua number), honouring `__index`.
    pub fn get_index<'f>(&self, frame: &'f Frame<'_>, key: i64) -> Result<ValueView<'f>> {
        self.require_live(frame)?;
        let key = checked_raw_integer_key(key)?;
        let state = frame.state();
        unsafe {
            if !self.has_metatable(state) {
                ffi::lua_rawgeti(state, self.index(), key);
                return Ok(frame.top_value());
            }
            ffi::lua_pushvalue(state, self.index());
            ffi::lua_pushnumber(state, f64::from(key));
            frame.raising(2, 1, |state| {
                ffi::lua_gettable(state, -2);
                ffi::lua_remove(state, -2);
                1
            })?;
        }
        Ok(frame.top_value())
    }

    /// Whether the table has a metatable (and so possibly `__index`/`__newindex`). Balanced.
    ///
    /// # Safety
    /// The table slot exists on a live thread.
    #[inline]
    unsafe fn has_metatable(&self, state: *mut ffi::lua_State) -> bool {
        unsafe {
            if ffi::lua_getmetatable(state, self.index()) == 0 {
                return false;
            }
            ffi::lua_pop(state, 1);
            true
        }
    }

    /// `rawget(t, key)` for an integer key. Never raises.
    pub fn raw_get_index<'f>(&self, frame: &'f Frame<'_>, key: i64) -> Result<ValueView<'f>> {
        self.require_live(frame)?;
        let key = checked_raw_integer_key(key)?;
        unsafe { ffi::lua_rawgeti(frame.state(), self.index(), key) };
        Ok(frame.top_value())
    }

    /// `rawset(t, key, top)` for an integer key. Consumes the value on top of `frame`.
    pub fn raw_set_index(&self, frame: &Frame<'_>, key: i64) -> Result<()> {
        self.require_store(frame)?;
        let key = checked_raw_integer_key(key)?;
        let state = frame.state();
        unsafe {
            ffi::lua_pushvalue(state, self.index());
            ffi::lua_insert(state, -2);
            frame.raising(2, 0, |state| {
                ffi::lua_rawseti(state, -2, key);
                ffi::lua_pop(state, 1);
                0
            })
        }
    }

    /// `rawset(t, key, top)` for a number key (any double, including fractions).
    pub fn raw_set_number_key(&self, frame: &Frame<'_>, key: f64) -> Result<()> {
        self.require_store(frame)?;
        let state = frame.state();
        unsafe {
            ffi::lua_pushvalue(state, self.index());
            ffi::lua_pushnumber(state, key);
            frame.raising(3, 0, |state| {
                ffi::lua_insert(state, -3);
                ffi::lua_insert(state, -3);
                ffi::lua_rawset(state, -3);
                ffi::lua_pop(state, 1);
                0
            })
        }
    }

    /// Pushes `value` and stores it under `key`, honouring `__newindex`.
    pub fn set_value<T: Push + ?Sized>(&self, frame: &Frame<'_>, key: &str, value: &T) -> Result<()> {
        value.push_into(frame)?;
        self.set(frame, key)
    }

    /// Pushes `value` and raw-stores it under `key`.
    pub fn raw_set_value<T: Push + ?Sized>(&self, frame: &Frame<'_>, key: &str, value: &T) -> Result<()> {
        value.push_into(frame)?;
        self.raw_set(frame, key)
    }

    /// Looks `key` up in a nested frame and hands the borrowed value to `body`; the frame is
    /// restored afterwards, so chained lookups stay balanced.
    pub fn with_field<R>(
        &self,
        frame: &Frame<'_>,
        key: &str,
        body: impl FnOnce(ValueView<'_>) -> Result<R>,
    ) -> Result<R> {
        frame.with_frame(|lookup| {
            let value = self.get(lookup, key)?;
            body(value)
        })
    }

    /// `t[key]` converted to `T` inside a nested frame.
    pub fn get_as<T: for<'a> FromView<'a>>(&self, frame: &Frame<'_>, key: &str) -> Result<T> {
        // A method path cannot generalise over the view lifetime; the closure is the HRTB.
        #[allow(clippy::redundant_closure_for_method_calls)]
        self.with_field(frame, key, |value| value.read::<T>())
    }

    /// Strictly typed optional read honouring `__index`: nil is `None`, a wrong type is an
    /// error naming `context`, the key, and the offending value (`TableView::getOptional`).
    pub fn get_optional<T: for<'a> FromView<'a>>(
        &self,
        frame: &Frame<'_>,
        key: &str,
        context: &str,
    ) -> Result<Option<T>> {
        frame.with_frame(|lookup| {
            let value = self.get(lookup, key)?;
            checked_optional::<T>(value, key, context)
        })
    }

    /// [`TableView::get_optional`] bypassing `__index`.
    pub fn raw_get_optional<T: for<'a> FromView<'a>>(
        &self,
        frame: &Frame<'_>,
        key: &str,
        context: &str,
    ) -> Result<Option<T>> {
        frame.with_frame(|lookup| {
            let value = self.raw_get(lookup, key)?;
            checked_optional::<T>(value, key, context)
        })
    }

    /// Visits every entry through `lua_rawiter`. The visitor receives the per-entry frame
    /// (for nested lookups) and the key and value views, valid only for that call; it must not
    /// add or remove entries.
    pub fn for_each(
        &self,
        frame: &Frame<'_>,
        mut visitor: impl FnMut(&Frame<'_>, ValueView<'_>, ValueView<'_>) -> Result<()>,
    ) -> Result<()> {
        self.require_live(frame)?;
        let state = frame.state();
        let mut iterator: c_int = 0;
        loop {
            let step = frame.frame();
            // SAFETY: the table slot exists; rawiter pushes key and value (two slots) on success.
            iterator = unsafe { ffi::lua_rawiter(state, self.index(), iterator) };
            if iterator < 0 {
                return Ok(());
            }
            visitor(&step, step.at(-2), step.at(-1))?;
        }
    }

    /// True when `predicate` accepts some key. Iteration stops at the first match.
    pub fn find_key(
        &self,
        frame: &Frame<'_>,
        mut predicate: impl FnMut(ValueView<'_>) -> Result<bool>,
    ) -> Result<bool> {
        self.require_live(frame)?;
        let state = frame.state();
        let mut iterator: c_int = 0;
        loop {
            let step = frame.frame();
            iterator = unsafe { ffi::lua_rawiter(state, self.index(), iterator) };
            if iterator < 0 {
                return Ok(false);
            }
            if predicate(step.at(-2))? {
                return Ok(true);
            }
        }
    }

    /// Length honouring `__len`.
    pub fn len(&self, frame: &Frame<'_>) -> Result<usize> {
        self.require_live(frame)?;
        let state = frame.state();
        let length = frame.with_frame(|call| {
            unsafe {
                ffi::lua_pushvalue(state, self.index());
                call.raising(1, 1, |state| {
                    if ffi::luaL_callmeta(state, -1, c"__len".as_ptr()) != 0 {
                        ffi::lua_remove(state, -2);
                    } else {
                        let raw = ffi::lua_objlen(state, -1);
                        ffi::lua_pop(state, 1);
                        ffi::lua_pushnumber(state, f64::from(raw));
                    }
                    1
                })?;
            }
            call.top_value().read::<usize>()
        })?;
        Ok(length)
    }

    /// Raw border length, ignoring `__len`.
    pub fn raw_len(&self) -> usize {
        if !self.value.exists() {
            return 0;
        }
        // lua_objlen returns int; Luau lengths are never negative.
        unsafe { ffi::lua_objlen(self.value.state(), self.index()).max(0) as usize }
    }

    pub fn is_read_only(&self) -> bool {
        self.value.exists() && unsafe { ffi::lua_getreadonly(self.value.state(), self.index()) != 0 }
    }

    pub fn set_read_only(&self, read_only: bool) -> Result<()> {
        if !self.value.exists() {
            return Err(Error::logic("Table view no longer names a live stack slot"));
        }
        unsafe { ffi::lua_setreadonly(self.value.state(), self.index(), read_only.into()) };
        Ok(())
    }

    fn require_store(&self, frame: &Frame<'_>) -> Result<()> {
        self.require_live(frame)?;
        if self.index() == ffi::LUA_REGISTRYINDEX {
            return Err(Error::logic("Cannot set the Lua registry pseudo-index"));
        }
        frame.require_value("store into a table")
    }
}

/// The C++ `checkedOptionalField`: nil is absent; a present value must convert.
fn checked_optional<T: for<'a> FromView<'a>>(value: ValueView<'_>, key: &str, context: &str) -> Result<Option<T>> {
    if value.is_nil() {
        return Ok(None);
    }
    if !T::matches(value) {
        let prefix = if context.is_empty() { String::new() } else { format!("{context} ") };
        return Err(Error::logic(format!(
            "{prefix}\"{}\" has an invalid value \"{}\"",
            truncate_diagnostic(key, 64),
            describe_value(value, 64)
        )));
    }
    value.read::<T>().map(Some)
}
