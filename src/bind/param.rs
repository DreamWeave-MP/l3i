//! Parameter kinds and per-slot materialisation (`bindfunction.hpp`: `materializeOne`,
//! `probeOne`, `inspectOptionalArgument`).

use std::ffi::c_int;

use super::Call;
use super::diagnostics;
use crate::convert::{BufferView, FromView, Integer, Vector3};
use crate::error::{Error, Result};
use crate::stack::{Scope, Type, ValueView};
use crate::userdata::{Userdata, check_receiver, receiver};
use crate::value::{Function, Table, Value};

/// How a parameter consumes Lua arguments.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ParamKind {
    /// Exactly one argument, one checked conversion.
    Regular,
    /// Fed from Rust (`&Call`); consumes no argument.
    Injected,
    /// Zero or one argument: absent and nil are `None`.
    Optional,
    /// Every remaining argument, each converted; must be last.
    VarArgs,
    /// Every remaining argument, borrowed and unconverted; must be last.
    ArgView,
}

/// A parameter type with the call lifetime erased. `Item<'c>` is the concrete type handed to
/// the callable for a call living `'c`; this indirection is what lets `|view: ValueView, s:
/// &str|` be written without naming lifetimes.
pub trait Param {
    type Item<'c>: ParamItem<'c>;
}

/// One parameter of a call living `'c`.
pub trait ParamItem<'c>: Sized {
    const KIND: ParamKind;
    /// Human-readable expected type for diagnostics.
    const EXPECTED: &'static str;

    /// Converts one slot. Only [`ParamKind::Regular`] and the inner type of an optional use it.
    fn read_slot(view: ValueView<'c>) -> Result<Self>;

    /// True when [`ParamItem::read_slot`] would succeed on `view`.
    fn matches(view: ValueView<'c>) -> bool;

    /// Read-only mirror of `materialize`'s cursor rules, for overload probing.
    fn probe(call: &'c Call<'c>, cursor: &mut c_int, top: c_int, allow_mismatch: bool) -> bool {
        if *cursor > top {
            return false;
        }
        let matched = Self::matches(call.arg(*cursor));
        *cursor += 1;
        let _ = allow_mismatch;
        matched
    }

    /// Converts the argument(s) at `cursor`, advancing it and the diagnostic position.
    fn materialize(
        call: &'c Call<'c>,
        cursor: &mut c_int,
        top: c_int,
        position: &mut c_int,
        debug_name: &str,
        allow_mismatch: bool,
    ) -> Result<Self> {
        let _ = allow_mismatch;
        *position += 1;
        if *cursor > top {
            // The C++ binder raised the missing-argument error inside the bad-argument catch,
            // so the two messages nest; keep that wording.
            let missing = diagnostics::missing_argument(debug_name, top);
            return Err(diagnostics::bad_argument(debug_name, *position, Self::EXPECTED, &missing));
        }
        let value = Self::read_slot(call.arg(*cursor))
            .map_err(|cause| diagnostics::bad_argument(debug_name, *position, Self::EXPECTED, &cause))?;
        *cursor += 1;
        Ok(value)
    }
}

// ---------------------------------------------------------------------------------------------
// Regular owned parameters, one conversion through FromView
// ---------------------------------------------------------------------------------------------

/// Implements [`Param`] for an owned type that already implements [`FromView`] for every
/// lifetime. Hosts use it for their own converter types.
#[macro_export]
macro_rules! impl_param_from_view {
    ($($t:ty),* $(,)?) => {$(
        impl $crate::bind::Param for $t {
            type Item<'c> = $t;
        }

        impl<'c> $crate::bind::ParamItem<'c> for $t {
            const KIND: $crate::bind::ParamKind = $crate::bind::ParamKind::Regular;
            const EXPECTED: &'static str = <$t as $crate::convert::FromView<'c>>::EXPECTED;

            fn read_slot(view: $crate::stack::ValueView<'c>) -> $crate::Result<Self> {
                <$t as $crate::convert::FromView<'c>>::from_view(view)
            }

            fn matches(view: $crate::stack::ValueView<'c>) -> bool {
                <$t as $crate::convert::FromView<'c>>::matches(view)
            }
        }
    )*};
}

impl_param_from_view!(
    bool, i8, i16, i32, i64, isize, u8, u16, u32, u64, usize, f32, f64, String, Vec<u8>, Integer, Vector3, Value,
);

impl Param for Table {
    type Item<'c> = Table;
}

impl<'c> ParamItem<'c> for Table {
    const KIND: ParamKind = ParamKind::Regular;
    const EXPECTED: &'static str = "table";
    fn read_slot(view: ValueView<'c>) -> Result<Self> {
        if !view.is_table() {
            return Err(view.type_error(Type::Table));
        }
        Table::from_value(Value::store(view)?)
    }
    fn matches(view: ValueView<'c>) -> bool {
        view.is_table()
    }
}

impl Param for Function {
    type Item<'c> = Function;
}

impl<'c> ParamItem<'c> for Function {
    const KIND: ParamKind = ParamKind::Regular;
    const EXPECTED: &'static str = "function";
    fn read_slot(view: ValueView<'c>) -> Result<Self> {
        if !view.is_function() {
            return Err(view.type_error(Type::Function));
        }
        Function::from_value(Value::store(view)?)
    }
    fn matches(view: ValueView<'c>) -> bool {
        view.is_function()
    }
}

// ---------------------------------------------------------------------------------------------
// Borrowed regular parameters
// ---------------------------------------------------------------------------------------------

macro_rules! borrowed_params {
    ($($erased:ty => $item:ty, $expected:literal;)*) => {$(
        impl Param for $erased {
            type Item<'c> = $item;
        }

        impl<'c> ParamItem<'c> for $item {
            const KIND: ParamKind = ParamKind::Regular;
            const EXPECTED: &'static str = $expected;
            fn read_slot(view: ValueView<'c>) -> Result<Self> {
                <$item as FromView<'c>>::from_view(view)
            }
            fn matches(view: ValueView<'c>) -> bool {
                <$item as FromView<'c>>::matches(view)
            }
        }
    )*};
}

borrowed_params! {
    &'_ str => &'c str, "string";
    &'_ [u8] => &'c [u8], "string";
    BufferView<'_> => BufferView<'c>, "buffer";
}

/// A borrowed view of one argument slot: no conversion, no pin, valid for the call.
impl Param for ValueView<'_> {
    type Item<'c> = ValueView<'c>;
}

impl<'c> ParamItem<'c> for ValueView<'c> {
    const KIND: ParamKind = ParamKind::Regular;
    const EXPECTED: &'static str = "any value";
    fn read_slot(view: ValueView<'c>) -> Result<Self> {
        if view.type_of() == Type::None {
            return Err(Error::logic("Cannot read a nonexistent Lua stack value"));
        }
        Ok(view)
    }
    /// Any present value, including nil.
    fn matches(view: ValueView<'c>) -> bool {
        view.type_of() != Type::None
    }
}

/// A userdata argument (tagged or untagged), borrowed for the call.
impl<T: Userdata> Param for &'_ T {
    type Item<'c> = &'c T;
}

impl<'c, T: Userdata> ParamItem<'c> for &'c T {
    const KIND: ParamKind = ParamKind::Regular;
    const EXPECTED: &'static str = T::NAME;
    fn read_slot(view: ValueView<'c>) -> Result<Self> {
        check_receiver::<T>(view)
    }
    fn matches(view: ValueView<'c>) -> bool {
        receiver::<T>(view).is_some()
    }
}

// ---------------------------------------------------------------------------------------------
// Injected call context
// ---------------------------------------------------------------------------------------------

impl Param for &'_ Call<'_> {
    type Item<'c> = &'c Call<'c>;
}

impl<'c> ParamItem<'c> for &'c Call<'c> {
    const KIND: ParamKind = ParamKind::Injected;
    const EXPECTED: &'static str = "<injected>";
    fn read_slot(_: ValueView<'c>) -> Result<Self> {
        Err(Error::logic("An injected parameter has no argument slot"))
    }
    fn matches(_: ValueView<'c>) -> bool {
        false
    }
    fn probe(_: &'c Call<'c>, _: &mut c_int, _: c_int, _: bool) -> bool {
        true
    }
    fn materialize(call: &'c Call<'c>, _: &mut c_int, _: c_int, _: &mut c_int, _: &str, _: bool) -> Result<Self> {
        Ok(call)
    }
}

// ---------------------------------------------------------------------------------------------
// Optional
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq)]
enum OptionalState {
    Absent,
    Nil,
    Value,
    Mismatch,
}

/// `inspectOptionalArgument`: a borrowed-view inner type accepts any present value.
fn inspect_optional<'c, Inner: ParamItem<'c>>(call: &'c Call<'c>, cursor: c_int, top: c_int, allow_mismatch: bool) -> OptionalState {
    if cursor > top {
        return OptionalState::Absent;
    }
    let view = call.arg(cursor);
    if view.is_nil() {
        return OptionalState::Nil;
    }
    if Inner::matches(view) {
        return OptionalState::Value;
    }
    if allow_mismatch { OptionalState::Absent } else { OptionalState::Mismatch }
}

impl<T: Param> Param for Option<T> {
    type Item<'c> = Option<T::Item<'c>>;
}

impl<'c, T: ParamItem<'c>> ParamItem<'c> for Option<T> {
    const KIND: ParamKind = ParamKind::Optional;
    const EXPECTED: &'static str = T::EXPECTED;

    /// Used when this optional is itself the element type of `VarArgs`.
    fn read_slot(view: ValueView<'c>) -> Result<Self> {
        if view.is_nil() {
            return Ok(None);
        }
        T::read_slot(view).map(Some)
    }

    fn matches(view: ValueView<'c>) -> bool {
        view.is_nil() || T::matches(view)
    }

    fn probe(call: &'c Call<'c>, cursor: &mut c_int, top: c_int, allow_mismatch: bool) -> bool {
        let state = inspect_optional::<T>(call, *cursor, top, allow_mismatch);
        if matches!(state, OptionalState::Nil | OptionalState::Value) {
            *cursor += 1;
        }
        state != OptionalState::Mismatch
    }

    /// A middle optional is omitted only when a following required parameter can consume the
    /// current value; matching optionals stay greedy and nil is the explicit way to skip one.
    fn materialize(
        call: &'c Call<'c>,
        cursor: &mut c_int,
        top: c_int,
        position: &mut c_int,
        debug_name: &str,
        allow_mismatch: bool,
    ) -> Result<Self> {
        match inspect_optional::<T>(call, *cursor, top, allow_mismatch) {
            OptionalState::Absent => Ok(None),
            OptionalState::Nil => {
                *position += 1;
                *cursor += 1;
                Ok(None)
            }
            OptionalState::Value | OptionalState::Mismatch => {
                *position += 1;
                let value = T::read_slot(call.arg(*cursor))
                    .map_err(|cause| diagnostics::bad_argument(debug_name, *position, T::EXPECTED, &cause))?;
                *cursor += 1;
                Ok(Some(value))
            }
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Terminators
// ---------------------------------------------------------------------------------------------

/// Every remaining argument converted as `T`. Must be the final parameter.
#[derive(Debug)]
pub struct VarArgs<T> {
    /// Physical Lua stack slot of the first collected argument (includes a method receiver;
    /// binder diagnostics number arguments separately).
    pub start_slot: c_int,
    pub values: Vec<T>,
}

impl<T: Param> Param for VarArgs<T> {
    type Item<'c> = VarArgs<T::Item<'c>>;
}

impl<'c, T: ParamItem<'c>> ParamItem<'c> for VarArgs<T> {
    const KIND: ParamKind = ParamKind::VarArgs;
    const EXPECTED: &'static str = T::EXPECTED;

    fn read_slot(_: ValueView<'c>) -> Result<Self> {
        Err(Error::logic("VarArgs consumes the remaining arguments, not one slot"))
    }
    fn matches(_: ValueView<'c>) -> bool {
        false
    }

    fn probe(call: &'c Call<'c>, cursor: &mut c_int, top: c_int, _: bool) -> bool {
        let inner_optional = T::KIND == ParamKind::Optional;
        while *cursor <= top {
            let view = call.arg(*cursor);
            let present = !inner_optional || !view.is_nil();
            if present && !T::matches(view) {
                return false;
            }
            *cursor += 1;
        }
        true
    }

    fn materialize(call: &'c Call<'c>, cursor: &mut c_int, top: c_int, position: &mut c_int, debug_name: &str, _: bool) -> Result<Self> {
        let start_slot = *cursor;
        let mut values = Vec::with_capacity((top - *cursor + 1).max(0) as usize);
        while *cursor <= top {
            *position += 1;
            let value = T::read_slot(call.arg(*cursor))
                .map_err(|cause| diagnostics::bad_argument(debug_name, *position, T::EXPECTED, &cause))?;
            values.push(value);
            *cursor += 1;
        }
        Ok(VarArgs { start_slot, values })
    }
}

/// Borrowed view over every remaining argument; heterogeneous and lazy. Valid only for the
/// call, for forwarding and inspection sites.
#[derive(Clone, Copy)]
pub struct ArgView<'c> {
    call: &'c Call<'c>,
    from: c_int,
    to_exclusive: c_int,
}

impl<'c> ArgView<'c> {
    pub fn len(&self) -> usize {
        (self.to_exclusive - self.from).max(0) as usize
    }

    pub fn is_empty(&self) -> bool {
        self.from >= self.to_exclusive
    }

    /// Physical stack slot of the first viewed argument.
    pub fn first_slot(&self) -> c_int {
        self.from
    }

    /// 0-based within the view.
    pub fn get(&self, index: usize) -> ValueView<'c> {
        self.call.arg(self.from + index as c_int)
    }

    pub fn read<T: FromView<'c>>(&self, index: usize) -> Result<T> {
        self.get(index).read::<T>()
    }

    /// Pushes copies of every viewed argument onto `scope` (same VM, any thread).
    pub fn copy_to(&self, scope: &impl Scope) -> Result<()> {
        for index in 0..self.len() {
            scope.push(&self.get(index))?;
        }
        Ok(())
    }
}

impl Param for ArgView<'_> {
    type Item<'c> = ArgView<'c>;
}

impl<'c> ParamItem<'c> for ArgView<'c> {
    const KIND: ParamKind = ParamKind::ArgView;
    const EXPECTED: &'static str = "<arguments>";
    fn read_slot(_: ValueView<'c>) -> Result<Self> {
        Err(Error::logic("ArgView borrows the remaining arguments, not one slot"))
    }
    fn matches(_: ValueView<'c>) -> bool {
        false
    }
    fn probe(_: &'c Call<'c>, cursor: &mut c_int, top: c_int, _: bool) -> bool {
        *cursor = top + 1;
        true
    }
    fn materialize(call: &'c Call<'c>, cursor: &mut c_int, top: c_int, _: &mut c_int, _: &str, _: bool) -> Result<Self> {
        let view = ArgView { call, from: *cursor, to_exclusive: top + 1 };
        *cursor = top + 1;
        Ok(view)
    }
}
