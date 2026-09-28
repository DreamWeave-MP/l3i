//! Native sequence and stream views (`L3I_EXTENSION_RUNTIME_ARCHITECTURE.md` §34, §35).
//!
//! A [`Sequence`] wraps a Rust collection and shows it to scripts as `#items`, `items[i]`
//! (1-based, nil past the end), `for item in items do`, and `items:toTable()`. The backing
//! collection stays native: only the item a script touches is pushed. A [`Stream`] is the
//! cursor-backed variant for results that cannot be indexed (directory walks, filtered
//! queries): `for item in stream do` opens a private cursor per loop, nothing else.
//!
//! Both are ordinary userdata types, so an extension declares them like any other
//! ([`crate::extension::ExtensionDescriptor::sequence`]) and they follow the plan's tag policy.

use std::ffi::c_int;

use crate::bind::{ArgView, Call};
use crate::convert::Push;
use crate::error::Result;
use crate::raw::{ffi, trampoline};
use crate::stack::{Scope, Type, ValueView};
use crate::userdata::Userdata;
use crate::userdata::iterator::Cursor;
use crate::userdata::metatable::MetatableBuilder;
use crate::value::Table;

/// A random-access native collection shown to scripts as a sequence.
pub trait SequenceSource: 'static {
    /// The userdata type name (`__type`), a debug name such as `dream.archive.Entries`.
    const NAME: &'static str;
    /// What one element becomes in Luau.
    type Item: Push;
    fn len(&self) -> usize;
    fn is_empty(&self) -> bool {
        self.len() == 0
    }
    /// The element at `index` (0-based), or `None` past the end.
    fn get(&self, index: usize) -> Option<Self::Item>;
}

/// The userdata payload of a sequence over `S`.
pub struct Sequence<S>(pub S);

// SAFETY: a `Sequence<S>` is plain Rust data with no Lua references; its destructor never
// touches the Lua API, as the `Userdata` contract requires.
unsafe impl<S: SequenceSource> Userdata for Sequence<S> {
    const NAME: &'static str = S::NAME;
}

/// A collection consumed through a per-loop cursor.
pub trait StreamSource: 'static {
    const NAME: &'static str;
    type Item: Push;
    /// Per-loop iteration state (interior mutability inside).
    type Cursor: 'static;
    fn open(&self) -> Self::Cursor;
    /// The next element, or `None` at the end.
    fn next(cursor: &Self::Cursor) -> Option<Self::Item>;
}

/// The userdata payload of a stream over `S`.
pub struct Stream<S>(pub S);

// SAFETY: as `Sequence<S>`.
unsafe impl<S: StreamSource> Userdata for Stream<S> {
    const NAME: &'static str = S::NAME;
}

/// `__index` for sequences: an integer key reads the element (nil past the end); anything else
/// goes to the methods table kept as upvalue 1.
unsafe extern "C-unwind" fn sequence_index<S: SequenceSource>(state: *mut ffi::lua_State) -> c_int {
    unsafe {
        trampoline::enter(state, || {
            let key = ValueView::resolve(state, 2);
            if key.type_of() == Type::Number || key.type_of() == Type::Integer {
                let call = Call::from_raw(state);
                let sequence = crate::userdata::check_receiver::<Sequence<S>>(call.arg(1))?;
                // An exact integer key selects an element; a fractional or out-of-range number is
                // no element (nil), as in a table, never a rounded neighbour.
                let index = key.read::<crate::convert::Exact<i64>>().ok().map(|index| index.0);
                match index.and_then(|i| i.checked_sub(1)).and_then(|i| usize::try_from(i).ok()).and_then(|i| sequence.0.get(i)) {
                    Some(item) => {
                        item.push_into(&call)?;
                    }
                    None => ffi::lua_pushnil(state),
                }
                return Ok(1);
            }
            ffi::lua_pushvalue(state, 2);
            ffi::lua_rawget(state, ffi::lua_upvalueindex(1));
            Ok(1)
        })
    }
}

/// Configures `ty` (whose `__type` is `S::NAME`) as a sequence: `toTable`, `__len`, `__iter`,
/// and integer `__index` over the methods table. Call after any extra methods.
pub fn configure_sequence<S: SequenceSource>(ty: &mut MetatableBuilder<'_>) -> Result<()> {
    configure_sequence_with_entry::<S>(ty).map(drop)
}

/// [`configure_sequence`], returning `toTable`'s direct entry for the extension planner.
pub(crate) fn configure_sequence_with_entry<S: SequenceSource>(
    ty: &mut MetatableBuilder<'_>,
) -> Result<crate::bind::MemberEntry> {
    let entry = ty.method_with_entry("toTable", |sequence: &Sequence<S>, call: &Call| to_table::<S>(call, sequence))?;
    ty.metamethod("__len", |sequence: &Sequence<S>, _operand: ArgView| sequence.0.len() as i64)?;
    ty.array_iterator(|sequence: &Sequence<S>, control: i64| -> Option<(i64, S::Item)> {
        let index = usize::try_from(control).ok()?;
        sequence.0.get(index).map(|item| (control + 1, item))
    })?;
    let type_name = ty.type_name()?;
    ty.install_wrapper(c"__index", sequence_index::<S>, &format!("{type_name}.__index"), true)?;
    Ok(entry)
}

/// Materialises the sequence into a fresh array table.
fn to_table<S: SequenceSource>(scope: &impl Scope, sequence: &Sequence<S>) -> Result<Table> {
    let len = sequence.0.len();
    let table = Table::new(scope, len, 0)?;
    scope.with_frame(|frame| {
        let view = table.push_to(frame)?;
        for index in 0..len {
            if let Some(item) = sequence.0.get(index) {
                item.push_into(frame)?;
                view.raw_set_index(frame, (index + 1) as i64)?;
            }
        }
        Ok(())
    })?;
    Ok(table)
}

/// Configures `ty` as a stream: `__iter` opening one cursor per loop.
pub fn configure_stream<S: StreamSource>(ty: &mut MetatableBuilder<'_>) -> Result<()> {
    ty.cursor_iterator(
        |call: &Call<'_>| Ok(crate::userdata::check_receiver::<Stream<S>>(call.arg(1))?.0.open()),
        |cursor: Cursor<'_, S::Cursor>, control: Option<i64>| -> Option<(i64, S::Item)> {
            S::next(&cursor).map(|item| (control.unwrap_or(0) + 1, item))
        },
    )
}

impl<S: SequenceSource> Sequence<S> {
    /// Pushes a sequence over `source` (the type must be registered in this VM).
    pub fn push<'s>(scope: &'s impl Scope, source: S) -> Result<ValueView<'s>> {
        crate::userdata::push_owned(scope, Sequence(source))
    }
}

impl<S: StreamSource> Stream<S> {
    /// Pushes a stream over `source` (the type must be registered in this VM).
    pub fn push<'s>(scope: &'s impl Scope, source: S) -> Result<ValueView<'s>> {
        crate::userdata::push_owned(scope, Stream(source))
    }
}
