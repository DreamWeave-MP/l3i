//! Owned Lua values: the middle tier (`components/luau/reference.hpp`, `value.cpp`).
//!
//! A [`Value`] owns a registry pin (`lua_ref`) but never owns the VM. Each value carries a weak
//! handle to its VM's lifetime token, so a value that outlives its [`crate::Runtime`] becomes
//! invalid instead of touching a closed VM (the C++ binder merely documented that rule). Pins
//! are for values that must survive the current stack frame; hot paths use borrowed views.

use std::ffi::{CString, c_int};
use std::rc::Weak;

use crate::error::{Error, Result};
use crate::raw::ffi;
use crate::stack::{Frame, Scope, Type, ValueView, same_vm};

/// A registry-pinned Lua value tied to one VM.
///
/// Cloning creates an independent pin. Equality is raw equality within one VM (two invalid
/// values are equal); values from different VMs are never equal.
pub struct Value {
    /// The VM's main thread, or null for an invalid value.
    owner: *mut ffi::lua_State,
    reference: c_int,
    /// Dead once the owning runtime is dropped; the pin is then already gone with the VM.
    vm: Weak<()>,
}

impl Value {
    /// An invalid value: no VM, no pin.
    pub const fn invalid() -> Value {
        Value { owner: std::ptr::null_mut(), reference: ffi::LUA_NOREF, vm: Weak::new() }
    }

    /// Pins the value `view` names. Nonexistent slots and the registry pseudo-index cannot be
    /// pinned.
    pub fn store(view: ValueView<'_>) -> Result<Value> {
        if view.type_of() == Type::None {
            return Err(Error::logic("Cannot store a nonexistent Lua stack value"));
        }
        if view.index() == ffi::LUA_REGISTRYINDEX {
            return Err(Error::logic("Cannot store the Lua registry pseudo-index"));
        }
        // SAFETY: the view proves the slot exists on its live thread; lua_ref pins without
        // popping and the registry is shared by every thread of the VM.
        unsafe {
            let vm = crate::runtime::vm_lifetime(view.state());
            if vm.strong_count() == 0 {
                return Err(Error::logic("Cannot pin a value on a VM that no Runtime owns"));
            }
            let owner = ffi::lua_mainthread(view.state());
            let reference = ffi::lua_ref(view.state(), view.index());
            Ok(Value { owner, reference, vm })
        }
    }

    /// A new empty table, pinned, with the stack of `scope` left as it was.
    pub fn new_table(scope: &impl Scope, array_capacity: usize, hash_capacity: usize) -> Result<Value> {
        let narr = crate::stack::checked_capacity(array_capacity)?;
        let nrec = crate::stack::checked_capacity(hash_capacity)?;
        let state = scope.state();
        // SAFETY: live state; the table is pinned and popped within this call.
        unsafe {
            ffi::lua_createtable(state, narr, nrec);
            let value = Value::store(scope.top_value())?;
            ffi::lua_pop(state, 1);
            Ok(value)
        }
    }

    /// A pinned C function. `debug_name` must be retained for the VM's life (see
    /// [`crate::debug_name`]) or null.
    ///
    /// # Safety
    /// `debug_name` is null or valid until the VM closes.
    pub unsafe fn new_function(
        scope: &impl Scope,
        function: ffi::lua_CFunction,
        debug_name: *const std::ffi::c_char,
    ) -> Result<Value> {
        let state = scope.state();
        unsafe {
            ffi::lua_pushcfunction(state, function, debug_name);
            let value = Value::store(scope.top_value())?;
            ffi::lua_pop(state, 1);
            Ok(value)
        }
    }

    /// The global `name`, pinned (nil pins as a valid nil value).
    pub fn get_global(scope: &impl Scope, name: &str) -> Result<Value> {
        let name = CString::new(name).map_err(|_| Error::logic("Global name cannot contain NUL"))?;
        let state = scope.state();
        // SAFETY: globals lookup on the raw globals table cannot invoke metamethods... unless
        // the host set one; hosts that do must read globals through a frame instead.
        unsafe {
            ffi::lua_rawgetfield(state, ffi::LUA_GLOBALSINDEX, name.as_ptr());
            let value = Value::store(scope.top_value())?;
            ffi::lua_pop(state, 1);
            Ok(value)
        }
    }

    /// True while the value holds a pin on a VM that is still open. A value that outlived its
    /// runtime is invalid, not dangling.
    pub fn is_valid(&self) -> bool {
        !self.owner.is_null() && self.reference != ffi::LUA_NOREF && self.vm.strong_count() > 0
    }

    /// Releases the pin, leaving the value invalid. Safe to call twice, and a no-op on a VM
    /// that has already closed.
    pub fn reset(&mut self) {
        if self.is_valid() {
            // SAFETY: the reference came from lua_ref on this VM, which the lifetime token
            // proves is still open, and is released once.
            unsafe { ffi::lua_unref(self.owner, self.reference) };
        }
        self.owner = std::ptr::null_mut();
        self.reference = ffi::LUA_NOREF;
        self.vm = Weak::new();
    }

    /// The registry reference id, for raw `lua_getref` on a thread of this VM.
    pub(crate) fn reference_id(&self) -> c_int {
        self.reference
    }

    /// True when `state` is a thread of this value's VM.
    pub(crate) fn belongs_to(&self, state: *mut ffi::lua_State) -> bool {
        self.is_valid() && same_vm(self.owner, state)
    }

    fn require_valid(&self) -> Result<()> {
        if self.is_valid() {
            return Ok(());
        }
        Err(Error::logic("Cannot use an invalid Lua reference"))
    }

    /// Pushes the value onto `frame`, which may be any thread of the same VM.
    pub fn push_to<'f>(&self, frame: &'f Frame<'_>) -> Result<ValueView<'f>> {
        self.require_valid()?;
        if !self.belongs_to(frame.state()) {
            return Err(Error::logic("Lua reference belongs to a different VM"));
        }
        // SAFETY: the registry is shared by all threads of the VM; getref pushes one value.
        unsafe { ffi::lua_getref(frame.state(), self.reference) };
        Ok(frame.top_value())
    }

    /// Pushes the value onto any scope of the same VM (a native call's result slot, or a frame).
    pub fn push_to_scope<'s, S: Scope>(&self, scope: &'s S) -> Result<ValueView<'s>> {
        self.require_valid()?;
        if !self.belongs_to(scope.state()) {
            return Err(Error::logic("Lua reference belongs to a different VM"));
        }
        // SAFETY: the registry is shared by all threads of the VM; getref pushes one value.
        unsafe { ffi::lua_getref(scope.state(), self.reference) };
        Ok(scope.top_value())
    }

    /// Pushes the value in a temporary frame on `scope` and hands that frame and the view to
    /// `body`, so nested lookups open their frames from it.
    pub fn with_value<R>(
        &self,
        scope: &impl Scope,
        body: impl FnOnce(&Frame<'_>, ValueView<'_>) -> Result<R>,
    ) -> Result<R> {
        self.require_valid()?;
        if !self.belongs_to(scope.state()) {
            return Err(Error::logic("Lua reference belongs to a different VM"));
        }
        scope.with_frame(|frame| {
            let view = self.push_to(frame)?;
            body(frame, view)
        })
    }

    /// The pinned value's type. Uses one balanced push/pop on the owning main thread.
    pub fn type_of(&self) -> Type {
        if !self.is_valid() {
            return Type::None;
        }
        // SAFETY: balanced: one push, one type query, one pop on the main thread.
        unsafe {
            ffi::lua_getref(self.owner, self.reference);
            let raw = ffi::lua_type(self.owner, -1);
            ffi::lua_pop(self.owner, 1);
            Type::from_raw(raw)
        }
    }

    pub fn is_nil(&self) -> bool {
        self.type_of() == Type::Nil
    }
    pub fn is_table(&self) -> bool {
        self.type_of() == Type::Table
    }
    pub fn is_function(&self) -> bool {
        self.type_of() == Type::Function
    }
    pub fn is_userdata(&self) -> bool {
        self.type_of() == Type::Userdata
    }
}

impl Clone for Value {
    /// An independent pin of the same value.
    fn clone(&self) -> Value {
        if !self.is_valid() {
            return Value::invalid();
        }
        // SAFETY: balanced push/pin/pop on the owning main thread.
        unsafe {
            ffi::lua_getref(self.owner, self.reference);
            let reference = ffi::lua_ref(self.owner, -1);
            ffi::lua_pop(self.owner, 1);
            Value { owner: self.owner, reference, vm: self.vm.clone() }
        }
    }
}

impl Drop for Value {
    fn drop(&mut self) {
        self.reset();
    }
}

impl Default for Value {
    fn default() -> Value {
        Value::invalid()
    }
}

impl PartialEq for Value {
    fn eq(&self, other: &Value) -> bool {
        if self.owner != other.owner {
            return false;
        }
        if !self.is_valid() || !other.is_valid() {
            return self.is_valid() == other.is_valid();
        }
        if self.reference == other.reference {
            return true;
        }
        // SAFETY: balanced: two pushes, rawequal, two pops on the shared main thread.
        unsafe {
            ffi::lua_getref(self.owner, self.reference);
            ffi::lua_getref(self.owner, other.reference);
            let equal = ffi::lua_rawequal(self.owner, -1, -2) != 0;
            ffi::lua_pop(self.owner, 2);
            equal
        }
    }
}

impl std::fmt::Debug for Value {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.is_valid() {
            write!(f, "Value({:?}, ref {})", self.type_of(), self.reference)
        } else {
            f.write_str("Value(invalid)")
        }
    }
}

/// A pinned value known to be a table.
#[derive(Clone, Debug, PartialEq, Default)]
pub struct Table(Value);

impl Table {
    pub fn new(scope: &impl Scope, array_capacity: usize, hash_capacity: usize) -> Result<Table> {
        Ok(Table(Value::new_table(scope, array_capacity, hash_capacity)?))
    }

    pub fn from_value(value: Value) -> Result<Table> {
        if value.type_of() != Type::Table {
            return Err(Error::runtime(format!("expected table, got {}", value.type_of().name())));
        }
        Ok(Table(value))
    }

    pub fn value(&self) -> &Value {
        &self.0
    }

    pub fn into_value(self) -> Value {
        self.0
    }

    pub fn push_to<'f>(&self, frame: &'f Frame<'_>) -> Result<crate::stack::TableView<'f>> {
        self.0.push_to(frame)?.as_table()
    }

    /// Cold tier: `t[key]` converted to `T`, with the stack of `scope` left as it was.
    pub fn get<T: for<'a> crate::convert::FromView<'a>>(&self, scope: &impl Scope, key: &str) -> Result<T> {
        self.0.with_value(scope, |frame, view| view.as_table()?.get_as::<T>(frame, key))
    }

    /// Cold tier: `t[key] = value`, honouring `__newindex`.
    pub fn set<T: crate::convert::Push + ?Sized>(&self, scope: &impl Scope, key: &str, value: &T) -> Result<()> {
        self.0.with_value(scope, |frame, view| view.as_table()?.set_value(frame, key, value))
    }
}

/// A pinned value known to be a function.
#[derive(Clone, Debug, PartialEq, Default)]
pub struct Function(Value);

impl Function {
    pub fn from_value(value: Value) -> Result<Function> {
        if value.type_of() != Type::Function {
            return Err(Error::runtime(format!("expected function, got {}", value.type_of().name())));
        }
        Ok(Function(value))
    }

    pub fn value(&self) -> &Value {
        &self.0
    }

    pub fn into_value(self) -> Value {
        self.0
    }

    pub fn push_to<'f>(&self, frame: &'f Frame<'_>) -> Result<ValueView<'f>> {
        self.0.push_to(frame)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::Runtime;

    fn string_value(runtime: &Runtime, text: &str) -> Value {
        let stack = runtime.stack();
        let frame = stack.frame();
        let view = frame.push_string(text);
        Value::store(view).unwrap()
    }

    #[test]
    fn references_outlive_repeated_full_collections_and_reset_releases_the_pin() {
        let runtime = Runtime::new().unwrap();
        let mut table = Value::new_table(&runtime.stack(), 0, 0).unwrap();
        let copy = table.clone();
        assert_eq!(table, copy);

        // A weak-valued table observes whether the pinned table stays alive.
        runtime.exec("weak = setmetatable({}, {__mode = 'v'})").unwrap();
        {
            let stack = runtime.stack();
            let frame = stack.frame();
            let weak = Value::get_global(&frame, "weak").unwrap().push_to(&frame).unwrap().as_table().unwrap();
            copy.push_to(&frame).unwrap();
            weak.raw_set(&frame, "value").unwrap();
        }
        table.reset();
        assert!(!table.is_valid());
        for _ in 0..3 {
            runtime.collect_garbage();
        }
        runtime.exec("assert(weak.value ~= nil, 'copy still pins the table')").unwrap();

        drop(copy);
        runtime.collect_garbage();
        runtime.exec("assert(weak.value == nil, 'last pin released')").unwrap();
    }

    #[test]
    fn lifecycle_reset_and_equality() {
        let runtime = Runtime::new().unwrap();
        let mut moved = string_value(&runtime, "shared");
        assert!(moved.is_valid());
        moved.reset();
        assert!(!moved.is_valid());
        moved.reset(); // double reset is safe

        let left = string_value(&runtime, "x");
        let right = string_value(&runtime, "x"); // independent pin, same value
        assert_eq!(left, right);
        let other = string_value(&runtime, "y");
        assert_ne!(left, other);
        assert_eq!(Value::invalid(), Value::default());
        assert_ne!(left, Value::invalid());

        let foreign_runtime = Runtime::new().unwrap();
        let foreign = string_value(&foreign_runtime, "x");
        assert_ne!(left, foreign);
        assert_ne!(right, foreign);
        assert_ne!(Value::invalid(), foreign);
    }

    #[test]
    fn values_cannot_cross_vms() {
        let runtime = Runtime::new().unwrap();
        let other = Runtime::new().unwrap();
        let value = string_value(&runtime, "x");
        let stack = other.stack();
        let frame = stack.frame();
        assert_eq!(value.push_to(&frame).unwrap_err(), Error::logic("Lua reference belongs to a different VM"));
        assert!(value.with_value(&stack, |_, _| Ok(())).is_err());
        assert_eq!(frame.len(), 0);
    }

    #[test]
    fn store_rejects_nonexistent_slots_and_the_registry() {
        let runtime = Runtime::new().unwrap();
        let stack = runtime.stack();
        let frame = stack.frame();
        assert!(Value::store(frame.at(1)).is_err());
        assert!(Value::store(frame.at(ffi::LUA_REGISTRYINDEX)).is_err());
        let nil = Value::store(frame.push_nil()).unwrap();
        assert!(nil.is_valid());
        assert!(nil.is_nil());
    }

    #[test]
    fn typed_wrappers_check_and_push() {
        let runtime = Runtime::new().unwrap();
        let stack = runtime.stack();
        let table = Table::new(&stack, 0, 1).unwrap();
        assert!(table.value().is_table());
        assert!(Function::from_value(table.value().clone()).is_err());
        {
            let frame = stack.frame();
            let view = table.push_to(&frame).unwrap();
            frame.push_number(42.0);
            view.raw_set(&frame, "answer").unwrap();
        }
        runtime.collect_garbage();
        let answer = table
            .value()
            .with_value(&stack, |_, view| {
                let frame_value = view.as_table()?;
                Ok(frame_value.index())
            })
            .unwrap();
        assert!(answer > 0);
        assert_eq!(stack.top(), 0, "with_value restored the stack");
    }
}
