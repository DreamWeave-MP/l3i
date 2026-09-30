//! Standard-library and VM utility entry points that the C++ binder never needed but Luau
//! offers: opening libraries one at a time, Luau's own sandboxing helpers, `luaL_register`,
//! `luaL_findtable`, table cloning and clearing, concatenation, raw equality and ordering, the
//! `luaL_Strbuf` string builder, and the experimental inliner toggle.

use std::ffi::{CString, c_int};

use crate::error::{Error, Result};
use crate::raw::ffi;
use crate::runtime::Runtime;
use crate::stack::{Frame, Scope, TableView, ValueView};

/// One of Luau's standard libraries (`luaopen_*`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Library {
    /// The base functions (`print`, `pcall`, `getmetatable`, ...), into the globals table.
    Base,
    Coroutine,
    Table,
    Os,
    String,
    Bit32,
    Buffer,
    Utf8,
    Math,
    Debug,
    Vector,
    Integer,
    /// Luau's experimental `class` library.
    Class,
}

impl Library {
    /// Every library `luaL_openlibs` opens, in its order.
    pub const STANDARD: [Library; 12] = [
        Library::Base,
        Library::Coroutine,
        Library::Table,
        Library::Os,
        Library::String,
        Library::Math,
        Library::Debug,
        Library::Utf8,
        Library::Bit32,
        Library::Buffer,
        Library::Vector,
        Library::Integer,
    ];

    /// The global the library is installed as (empty for `Base`).
    pub fn global_name(self) -> &'static str {
        match self {
            Library::Base => "",
            Library::Coroutine => "coroutine",
            Library::Table => "table",
            Library::Os => "os",
            Library::String => "string",
            Library::Bit32 => "bit32",
            Library::Buffer => "buffer",
            Library::Utf8 => "utf8",
            Library::Math => "math",
            Library::Debug => "debug",
            Library::Vector => "vector",
            Library::Integer => "integer",
            Library::Class => "class",
        }
    }

    fn opener(self) -> ffi::lua_CFunction {
        match self {
            Library::Base => ffi::luaopen_base,
            Library::Coroutine => ffi::luaopen_coroutine,
            Library::Table => ffi::luaopen_table,
            Library::Os => ffi::luaopen_os,
            Library::String => ffi::luaopen_string,
            Library::Bit32 => ffi::luaopen_bit32,
            Library::Buffer => ffi::luaopen_buffer,
            Library::Utf8 => ffi::luaopen_utf8,
            Library::Math => ffi::luaopen_math,
            Library::Debug => ffi::luaopen_debug,
            Library::Vector => ffi::luaopen_vector,
            Library::Integer => ffi::luaopen_integer,
            Library::Class => ffi::luaopen_class,
        }
    }
}

impl Runtime {
    /// Opens one standard library (as `luaL_openlibs` would, but selectively). Base must come
    /// first; each opener registers its global and leaves the stack as it was.
    pub fn open_library(&self, library: Library) -> Result<()> {
        let stack = self.stack();
        stack.with_frame(|frame| {
            // SAFETY: the opener is called like `luaL_openlibs` does: pushed with its name as
            // the argument. Openers raise only for out of memory.
            unsafe {
                frame.push_c_function(library.opener(), std::ptr::null());
                frame.push_string(library.global_name());
                frame.raising(2, 0, |state| {
                    ffi::lua_call(state, 1, 0);
                    0
                })
            }
        })
    }

    /// Luau's own sandbox (`luaL_sandbox`): freezes every library table and the globals table
    /// itself, marks it safe, and gives the string metatable read-only protection. After this,
    /// [`crate::thread::Thread::sandbox`] gives threads writable global proxies.
    pub fn sandbox_luau(&self) {
        // SAFETY: live main thread; the helper only flips read-only flags and metatables.
        unsafe { ffi::luaL_sandbox(self.stack().state_ptr()) };
    }

    /// `luaL_register`: creates (or reuses) the global table `name`, fills it with C functions,
    /// and returns it pinned. Function names must be plain identifiers; debug names are the
    /// `name.function` spelling, retained for the VM's life.
    pub fn register_library(
        &self,
        name: &str,
        functions: &[(&str, ffi::lua_CFunction)],
    ) -> Result<crate::value::Table> {
        if name.is_empty() {
            return Err(Error::logic("register_library needs a library name; use set_global for bare functions"));
        }
        let library = CString::new(name).map_err(|_| Error::logic("Library name contains NUL"))?;
        let stack = self.stack();
        stack.with_frame(|frame| {
            // SAFETY: luaL_findtable creates or finds the table under `name` in the globals
            // (raising for a non-table), leaving it on the frame; each function is retained
            // with a debug name and raw-set into it.
            unsafe {
                let state = frame.state();
                frame.raising(0, 1, |state| {
                    let conflict = ffi::luaL_findtable(state, ffi::LUA_GLOBALSINDEX, library.as_ptr(), 8);
                    if !conflict.is_null() {
                        ffi::luaL_errorL(state, c"name conflict for library '%s'".as_ptr(), library.as_ptr());
                    }
                    1
                })?;
                let table = frame.top_value().as_table()?;
                for (function_name, function) in functions {
                    let debug_name = crate::debug_name::retain(state, &format!("{name}.{function_name}"), &[name])?;
                    frame.push_c_function(*function, debug_name);
                    table.raw_set(frame, function_name)?;
                }
                crate::value::Table::from_value(crate::value::Value::store(table.value())?)
            }
        })
    }

    /// `luaL_findtable`: the table at dotted `path` under the globals, created along the way
    /// when missing; an error names the first segment that is not a table.
    pub fn find_table(&self, path: &str) -> Result<crate::value::Table> {
        let path_c = CString::new(path).map_err(|_| Error::logic("Table path contains NUL"))?;
        let stack = self.stack();
        stack.with_frame(|frame| {
            unsafe {
                frame.raising(0, 1, |state| {
                    let conflict = ffi::luaL_findtable(state, ffi::LUA_GLOBALSINDEX, path_c.as_ptr(), 0);
                    if !conflict.is_null() {
                        ffi::luaL_errorL(state, c"'%s' is not a table".as_ptr(), conflict);
                    }
                    1
                })?;
            }
            crate::value::Table::from_value(crate::value::Value::store(frame.top_value())?)
        })
    }

    /// Enables or disables Luau's experimental JIT-style inliner for this VM
    /// (`luau_enable_jit_inliner`).
    pub fn set_jit_inliner(&self, enabled: bool) {
        // SAFETY: live main thread; a flag write on the global state.
        unsafe {
            if enabled {
                ffi::luau_enable_jit_inliner(self.stack().state_ptr());
            } else {
                ffi::luau_disable_jit_inliner(self.stack().state_ptr());
            }
        }
    }
}

impl<'v> TableView<'v> {
    /// Pushes a shallow copy of this table (`lua_clonetable`): same array and hash parts, same
    /// metatable, not read-only.
    pub fn clone_table<'f>(&self, frame: &'f Frame<'_>) -> Result<TableView<'f>> {
        self.require_live_for(frame)?;
        // SAFETY: the table slot exists; clonetable pushes the copy.
        unsafe { ffi::lua_clonetable(frame.state(), self.index()) };
        Ok(TableView::new(frame.top_value()))
    }

    /// Removes every key (`lua_cleartable`), keeping the capacity. Raises (or fails at host
    /// level) for a read-only table.
    pub fn clear(&self, frame: &Frame<'_>) -> Result<()> {
        self.require_live_for(frame)?;
        // SAFETY: a copy of the table travels into the protected call as its one argument
        // (absolute indexes of the outer frame are meaningless inside it); cleartable raises
        // only for read-only tables.
        unsafe {
            ffi::lua_pushvalue(frame.state(), self.index());
            frame.raising(1, 0, |state| {
                ffi::lua_cleartable(state, 1);
                ffi::lua_pop(state, 1);
                0
            })
        }
    }

    fn require_live_for(&self, frame: &Frame<'_>) -> Result<()> {
        if !self.value().exists() {
            return Err(Error::logic("Table view no longer names a live stack slot"));
        }
        if frame.state() != self.value().state() {
            return Err(Error::logic("Table view belongs to another thread"));
        }
        Ok(())
    }
}

impl Frame<'_> {
    /// Concatenates the top `count` values into one string, honouring `__concat`
    /// (`lua_concat`); the result replaces them.
    pub fn concat(&self, count: c_int) -> Result<ValueView<'_>> {
        if count < 0 || count > self.len() {
            return Err(Error::logic("concat needs that many values on the frame"));
        }
        // SAFETY: `count` operands are on top; concat consumes them and pushes one result.
        unsafe {
            self.raising(count, 1, |state| {
                ffi::lua_concat(state, count);
                1
            })?;
        }
        Ok(self.top_value())
    }

    /// `a == b` honouring `__eq` (`lua_equal`).
    pub fn equal(&self, a: ValueView<'_>, b: ValueView<'_>) -> Result<bool> {
        self.compare(a, b, |state| unsafe { ffi::lua_equal(state, -2, -1) })
    }

    /// `a < b` honouring `__lt` (`lua_lessthan`).
    pub fn less_than(&self, a: ValueView<'_>, b: ValueView<'_>) -> Result<bool> {
        self.compare(a, b, |state| unsafe { ffi::lua_lessthan(state, -2, -1) })
    }

    fn compare(&self, a: ValueView<'_>, b: ValueView<'_>, op: fn(*mut ffi::lua_State) -> c_int) -> Result<bool> {
        self.with_frame(|inner| {
            inner.push_value(a)?;
            inner.push_value(b)?;
            let mut answer = 0;
            // SAFETY: both operands are on top; the comparison may run a metamethod, hence the
            // protected path at host level. The result travels through the closure.
            unsafe {
                inner.raising(2, 0, |state| {
                    answer = op(state);
                    ffi::lua_pop(state, 2);
                    0
                })?;
            }
            Ok(answer != 0)
        })
    }
}

/// Luau's `luaL_Strbuf`: builds a string in place, spilling to a mutable Lua string when the
/// inline buffer fills, and pushes the finished string onto the scope it was opened on.
///
/// The builder keeps its spill storage on the stack below anything pushed after it, exactly as
/// Luau's own API does; finish it before popping through it.
pub struct StringBuilder<'s> {
    raw: Box<ffi::luaL_Strbuf>,
    state: *mut ffi::lua_State,
    _scope: std::marker::PhantomData<&'s ()>,
}

impl<'s> StringBuilder<'s> {
    /// Starts a builder on `scope`'s thread.
    pub fn new(scope: &'s impl Scope) -> StringBuilder<'s> {
        let mut raw = Box::new(ffi::luaL_Strbuf {
            p: std::ptr::null_mut(),
            end: std::ptr::null_mut(),
            L: std::ptr::null_mut(),
            storage: std::ptr::null_mut(),
            buffer: [0; ffi::LUA_BUFFERSIZE],
        });
        // SAFETY: the struct is heap-pinned for the builder's life, as Luau requires.
        unsafe { ffi::luaL_buffinit(scope.state(), &mut *raw) };
        StringBuilder { raw, state: scope.state(), _scope: std::marker::PhantomData }
    }

    pub fn push_str(&mut self, text: &str) -> &mut Self {
        // SAFETY: the buffer is initialised; luaL_addlstring copies the bytes.
        unsafe { ffi::luaL_addlstring(&mut *self.raw, text.as_ptr().cast(), text.len()) };
        self
    }

    pub fn push_bytes(&mut self, bytes: &[u8]) -> &mut Self {
        unsafe { ffi::luaL_addlstring(&mut *self.raw, bytes.as_ptr().cast(), bytes.len()) };
        self
    }

    /// Appends `tostring`-style text of the value at `view` (`luaL_addvalueany`).
    pub fn push_value(&mut self, view: ValueView<'_>) -> Result<&mut Self> {
        if view.state() != self.state {
            return Err(Error::logic("String builder and value are on different threads"));
        }
        unsafe { ffi::luaL_addvalueany(&mut *self.raw, view.index()) };
        Ok(self)
    }

    /// Pushes the built string onto the scope and returns a view of it.
    pub fn finish(self) -> ValueView<'s> {
        let mut raw = self.raw;
        // SAFETY: pushresult replaces any spilled storage with the final string.
        unsafe { ffi::luaL_pushresult(&mut *raw) };
        ValueView::resolve(self.state, -1)
    }
}
