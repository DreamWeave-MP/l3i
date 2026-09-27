//! The component registration contract.
//!
//! A component crate knows how to expose itself to Luau but never owns the VM. It implements
//! [`LuauModule`]; the host owns the [`Runtime`], decides which modules exist, and calls
//! [`Runtime::register_module`]. The result is a frozen package table the host places wherever
//! its script environment wants it (a global, a `require` loader, a sandbox package list).
//!
//! `ModuleBuilder` is the `PackageBuilder` of `bindfunction.hpp`: every function it binds is
//! named `<module>.<key>` under the host's debug roots, so neither the prefix nor the key is
//! written twice.

use crate::bind::{Binding, function};
use crate::convert::Push;
use crate::error::{Error, Result};
use crate::runtime::Runtime;
use crate::stack::Scope;
use crate::userdata::metatable::MetatableBuilder;
use crate::userdata::{Userdata, tagged, untagged};
use crate::value::{Function, Table, Value};

/// A library of Luau bindings that a host can install into its runtime.
pub trait LuauModule {
    /// Dot-separated package path rooted at one of the host's debug roots,
    /// e.g. `dreamweave.assets`.
    const NAME: &'static str;

    /// Registers functions, values, and userdata types into `module`.
    fn register(runtime: &Runtime, module: &mut ModuleBuilder<'_>) -> Result<()>;
}

/// Builds one package table.
pub struct ModuleBuilder<'r> {
    runtime: &'r Runtime,
    table: Table,
    path: String,
    metatable: Option<Table>,
}

impl<'r> ModuleBuilder<'r> {
    pub(crate) fn new(runtime: &'r Runtime, path: &str, hash_capacity: usize) -> Result<Self> {
        crate::debug_name::require_valid_debug_name(path, runtime.debug_roots())?;
        let table = Table::new(&runtime.stack(), 0, hash_capacity)?;
        Ok(ModuleBuilder { runtime, table, path: path.to_owned(), metatable: None })
    }

    /// The package path.
    pub fn path(&self) -> &str {
        &self.path
    }

    /// The package table being built.
    pub fn table(&self) -> &Table {
        &self.table
    }

    fn member_name(&self, key: &str) -> String {
        format!("{}.{key}", self.path)
    }

    /// Binds `callable` as `<path>.<key>` and stores it in the package.
    pub fn function<F: Binding<M>, M>(&mut self, key: &str, callable: F) -> Result<Function> {
        let bound = function(&self.runtime.stack(), self.runtime.debug_roots(), &self.member_name(key), callable)?;
        self.table.set(&self.runtime.stack(), key, &bound)?;
        Ok(bound)
    }

    /// Stores any pushable value under `key`.
    pub fn set<T: Push + ?Sized>(&mut self, key: &str, value: &T) -> Result<()> {
        self.table.set(&self.runtime.stack(), key, value)
    }

    /// Registers a userdata type (tagged or untagged by its `TAG`) and configures its
    /// metatable. The type's `NAME` must live under the host's debug roots.
    pub fn userdata<T: Userdata>(&mut self, configure: impl FnOnce(&mut MetatableBuilder<'_>) -> Result<()>) -> Result<()> {
        if T::TAG.is_some() { tagged::register::<T>(self.runtime, configure) } else { untagged::register::<T>(self.runtime, configure) }
    }

    /// Binds a metamethod on the package's own metatable, named `<path>.<name>`; the metatable
    /// is created on first use.
    pub fn metamethod<F: Binding<M>, M>(&mut self, name: &str, callable: F) -> Result<Function> {
        let bound = function(&self.runtime.stack(), self.runtime.debug_roots(), &self.member_name(name), callable)?;
        self.set_metafield(name, bound.value())?;
        Ok(bound)
    }

    /// Stores `value` on the package's metatable, creating the metatable on first use.
    pub fn set_metafield(&mut self, name: &str, value: &Value) -> Result<()> {
        let stack = self.runtime.stack();
        if self.metatable.is_none() {
            let metatable = Table::new(&stack, 0, 2)?;
            stack.with_frame(|frame| {
                let package = self.table.push_to(frame)?;
                metatable.push_to(frame)?;
                // SAFETY: package and metatable are on the frame.
                unsafe { crate::raw::ffi::lua_setmetatable(frame.state(), package.index()) };
                Ok(())
            })?;
            self.metatable = Some(metatable);
        }
        self.metatable.as_ref().expect("created above").set(&stack, name, value)
    }

    /// Freezes the package (and its metatable, if any) and returns it. Nothing can be added
    /// afterwards from scripts or from Rust without [`crate::readonly::set_read_only_field`].
    pub fn finish(self) -> Result<Table> {
        if let Some(metatable) = &self.metatable {
            crate::readonly::make_read_only(self.runtime, metatable)?;
        }
        crate::readonly::make_read_only(self.runtime, &self.table)?;
        Ok(self.table)
    }
}

impl Runtime {
    /// Starts a package table at `path` (validated against the debug roots).
    pub fn module(&self, path: &str) -> Result<ModuleBuilder<'_>> {
        ModuleBuilder::new(self, path, 0)
    }

    /// Registers `M` and returns its frozen package table.
    pub fn register_module<M: LuauModule>(&self) -> Result<Table> {
        let mut module = ModuleBuilder::new(self, M::NAME, 0)?;
        M::register(self, &mut module)?;
        module.finish()
    }

    /// Stores `value` as the global `name`.
    pub fn set_global<T: Push + ?Sized>(&self, name: &str, value: &T) -> Result<()> {
        let stack = self.stack();
        stack.with_frame(|frame| {
            frame.push(value)?;
            frame.set_global(name)
        })
    }

    /// The global `name`, pinned.
    pub fn global(&self, name: &str) -> Result<Value> {
        Value::get_global(&self.stack(), name)
    }

    /// Fails unless `name` is a valid debug name under this runtime's roots.
    pub fn require_debug_name(&self, name: &str) -> Result<()> {
        crate::debug_name::require_valid_debug_name(name, self.debug_roots()).map_err(|_| {
            Error::logic(format!("'{name}' is not a debug name under roots {:?}", self.debug_roots()))
        })
    }
}
