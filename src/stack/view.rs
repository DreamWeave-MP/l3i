use std::ffi::c_int;

use super::{Stack, TableView};
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
            Type::Number | Type::Integer => "number",
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

/// A borrowed view of one stack slot. Copying a view copies the index, not the value.
#[derive(Clone, Copy, Debug)]
pub struct ValueView<'s> {
    stack: Stack<'s>,
    index: c_int,
}

impl<'s> ValueView<'s> {
    pub(crate) fn new(stack: Stack<'s>, index: c_int) -> Self {
        ValueView { stack, index }
    }

    #[inline]
    pub fn stack(&self) -> &Stack<'s> {
        &self.stack
    }

    #[inline]
    pub fn index(&self) -> c_int {
        self.index
    }

    pub fn type_of(&self) -> Type {
        if self.index == 0 {
            return Type::None;
        }
        // SAFETY: the stack is live for 's; lua_type tolerates any acceptable index.
        Type::from_raw(unsafe { ffi::lua_type(self.stack.state(), self.index) })
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

    pub fn as_table(&self) -> Result<TableView<'s>> {
        if !self.is_table() {
            return Err(self.type_error(Type::Table));
        }
        Ok(TableView::new(*self))
    }

    /// Formats the same message Luau's `luaL_typeerror` would for this slot, minus the
    /// function-name lookup: `<expected> expected, got <actual>`.
    pub(crate) fn type_error(&self, expected: Type) -> Error {
        Error::runtime(format!("{} expected, got {}", expected.name(), self.type_of().name()))
    }
}
