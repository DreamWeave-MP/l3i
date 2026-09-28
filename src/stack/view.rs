use std::ffi::c_int;
use std::marker::PhantomData;

use super::TableView;
use crate::error::{Error, Result};
use crate::raw::ffi;

/// Luau's value types, including the Luau-only ones.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(i32)]
pub enum Type {
    None = ffi::LUA_TNONE,
    Nil = ffi::LUA_TNIL,
    Boolean = ffi::LUA_TBOOLEAN,
    LightUserdata = ffi::LUA_TLIGHTUSERDATA,
    Number = ffi::LUA_TNUMBER,
    Integer = ffi::LUA_TINTEGER,
    Vector = ffi::LUA_TVECTOR,
    String = ffi::LUA_TSTRING,
    Table = ffi::LUA_TTABLE,
    Function = ffi::LUA_TFUNCTION,
    Userdata = ffi::LUA_TUSERDATA,
    Thread = ffi::LUA_TTHREAD,
    Buffer = ffi::LUA_TBUFFER,
}

impl Type {
    #[inline(always)]
    pub(crate) fn from_raw(raw: c_int) -> Type {
        match raw {
            ffi::LUA_TNIL => Type::Nil,
            ffi::LUA_TBOOLEAN => Type::Boolean,
            ffi::LUA_TLIGHTUSERDATA => Type::LightUserdata,
            ffi::LUA_TNUMBER => Type::Number,
            ffi::LUA_TINTEGER => Type::Integer,
            ffi::LUA_TVECTOR => Type::Vector,
            ffi::LUA_TSTRING => Type::String,
            ffi::LUA_TTABLE => Type::Table,
            ffi::LUA_TFUNCTION => Type::Function,
            ffi::LUA_TUSERDATA => Type::Userdata,
            ffi::LUA_TTHREAD => Type::Thread,
            ffi::LUA_TBUFFER => Type::Buffer,
            _ => Type::None,
        }
    }

    /// Luau's own name for the type, as `lua_typename` reports it.
    pub fn name(self) -> &'static str {
        match self {
            Type::None => "no value",
            Type::Nil => "nil",
            Type::Boolean => "boolean",
            Type::LightUserdata => "userdata",
            Type::Number => "number",
            Type::Integer => "integer",
            Type::Vector => "vector",
            Type::String => "string",
            Type::Table => "table",
            Type::Function => "function",
            Type::Userdata => "userdata",
            Type::Thread => "thread",
            Type::Buffer => "buffer",
        }
    }
}

/// A borrowed view of one stack slot. `'v` is the scope that keeps the slot alive: the
/// [`super::Frame`] that pushed it, or the [`super::Stack`] for slots below every frame.
/// Copying a view copies the index, not the value.
#[derive(Clone, Copy, Debug)]
pub struct ValueView<'v> {
    state: *mut ffi::lua_State,
    /// Absolute index, a pseudo-index, or 0 for "no value".
    index: c_int,
    /// A stack height the slot is known to lie within (argument views), or 0 when `exists`
    /// must ask Luau.
    known_top: c_int,
    _scope: PhantomData<&'v ()>,
}

impl<'v> ValueView<'v> {
    /// Resolves `index` the way the C++ `Stack::at` did: positive and pseudo indexes are kept
    /// (a positive index above the top still names its argument position for diagnostics, and
    /// reads as [`Type::None`]), negative ones are made absolute against the current top, and
    /// an out-of-range negative index becomes 0.
    #[inline(always)]
    pub(crate) fn resolve(state: *mut ffi::lua_State, index: c_int) -> Self {
        let index = if index <= ffi::LUA_REGISTRYINDEX || index >= 0 {
            index
        } else {
            // SAFETY: state is live for 'v.
            let absolute = unsafe { ffi::lua_gettop(state) } + index + 1;
            if absolute > 0 { absolute } else { 0 }
        };
        ValueView { state, index, known_top: 0, _scope: PhantomData }
    }

    /// A view of argument slot `index` on a native call whose argument count is `top`: the
    /// slot's existence is settled without asking Luau.
    #[inline(always)]
    pub(crate) fn within(state: *mut ffi::lua_State, index: c_int, top: c_int) -> Self {
        debug_assert!(index >= 1 && index <= top);
        ValueView { state, index, known_top: top, _scope: PhantomData }
    }

    #[inline(always)]
    pub(crate) fn state(&self) -> *mut ffi::lua_State {
        self.state
    }

    #[inline(always)]
    pub fn index(&self) -> c_int {
        self.index
    }

    /// True when the slot still exists. A view left behind by an out-of-order frame drop
    /// stops existing instead of aliasing whatever Luau puts there next.
    #[inline(always)]
    pub(crate) fn exists(&self) -> bool {
        if self.index <= ffi::LUA_REGISTRYINDEX {
            return true;
        }
        if self.known_top > 0 {
            // Arguments of a running native call cannot be popped from under it.
            return self.index > 0 && self.index <= self.known_top;
        }
        // SAFETY: state is live for 'v.
        self.index > 0 && self.index <= unsafe { ffi::lua_gettop(self.state) }
    }

    #[inline(always)]
    pub fn type_of(&self) -> Type {
        if !self.exists() {
            return Type::None;
        }
        // SAFETY: `exists` proved the index is acceptable to Luau.
        Type::from_raw(unsafe { ffi::lua_type(self.state, self.index) })
    }

    pub fn is_nil(&self) -> bool {
        self.type_of() == Type::Nil
    }
    pub fn is_boolean(&self) -> bool {
        self.type_of() == Type::Boolean
    }
    /// True for both Luau numbers and Luau 64-bit integers.
    pub fn is_number(&self) -> bool {
        matches!(self.type_of(), Type::Number | Type::Integer)
    }
    pub fn is_integer(&self) -> bool {
        self.type_of() == Type::Integer
    }
    pub fn is_string(&self) -> bool {
        self.type_of() == Type::String
    }
    pub fn is_table(&self) -> bool {
        self.type_of() == Type::Table
    }
    pub fn is_function(&self) -> bool {
        self.type_of() == Type::Function
    }
    pub fn is_light_userdata(&self) -> bool {
        self.type_of() == Type::LightUserdata
    }
    pub fn is_userdata(&self) -> bool {
        self.type_of() == Type::Userdata
    }
    pub fn is_thread(&self) -> bool {
        self.type_of() == Type::Thread
    }
    pub fn is_buffer(&self) -> bool {
        self.type_of() == Type::Buffer
    }
    pub fn is_vector(&self) -> bool {
        self.type_of() == Type::Vector
    }

    pub fn as_table(&self) -> Result<TableView<'v>> {
        if !self.is_table() {
            return Err(self.type_error(Type::Table));
        }
        Ok(TableView::new(*self))
    }

    /// `Lua stack index N: expected <type>, got <type>`, the C++ `luaValueTypeError` wording.
    pub(crate) fn type_error(&self, expected: Type) -> Error {
        Error::runtime(format!(
            "Lua stack index {}: expected {}, got {}",
            self.index,
            expected.name(),
            self.type_of().name()
        ))
    }
}
