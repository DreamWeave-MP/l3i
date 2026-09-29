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

use crate::bind::{ArgView, Call, Return};
use crate::convert::Push;
use crate::error::Result;
use crate::raw::{ffi, trampoline};
use crate::stack::{Scope, ValueView};
use crate::userdata::Userdata;
use crate::userdata::iterator::Cursor;
use crate::userdata::metatable::MetatableBuilder;
use crate::value::Table;

/// A random-access native collection shown to scripts as a sequence.
pub trait SequenceSource: 'static {
    /// The userdata type name (`__type`), a debug name such as `dream.archive.Entries`.
    const NAME: &'static str;
    /// What one element becomes in Luau.
    type Item: SequenceItem;
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
    type Item: SequenceItem;
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

/// An element a sequence or stream hands to Lua, pushed by value: every `Push` type qualifies,
/// and so does [`Owned<T>`](crate::userdata::Owned) for any registered `T`, which moves the
/// row into a fresh userdata without needing `Clone`.
pub trait SequenceItem {
    fn push_item<S: Scope>(self, scope: &S) -> Result<()>;
}

macro_rules! sequence_items {
    ($($t:ty),* $(,)?) => {$(
        impl SequenceItem for $t {
            #[inline]
            fn push_item<S: Scope>(self, scope: &S) -> Result<()> {
                Push::push_only(&self, scope)
            }
        }
    )*};
}

sequence_items!(
    bool,
    i8,
    i16,
    i32,
    i64,
    isize,
    u8,
    u16,
    u32,
    u64,
    usize,
    f32,
    f64,
    String,
    &'static str,
    Vec<u8>,
    crate::convert::Integer,
    crate::convert::Bits64,
    crate::convert::Vector3,
    crate::value::Value,
    Table,
    crate::value::Function,
);

impl<T: crate::packed::PackedScalar> SequenceItem for crate::packed::Packed<T> {
    #[inline]
    fn push_item<S: Scope>(self, scope: &S) -> Result<()> {
        Push::push_only(&self, scope)
    }
}

impl<T: Userdata> SequenceItem for crate::userdata::Owned<T> {
    #[inline]
    fn push_item<S: Scope>(self, scope: &S) -> Result<()> {
        crate::userdata::push_owned(scope, self.0).map(drop)
    }
}

impl<T: SequenceItem> SequenceItem for Option<T> {
    #[inline]
    fn push_item<S: Scope>(self, scope: &S) -> Result<()> {
        match self {
            Some(item) => item.push_item(scope),
            None => Push::push_only(&(), scope),
        }
    }
}

/// One step of a view's iterator: the next control value and the element, pushed by value.
pub struct IterStep<T: SequenceItem>(pub i64, pub T);

impl<T: SequenceItem> Return for IterStep<T> {
    fn push_results(self, call: &Call<'_>) -> Result<c_int> {
        Push::push_only(&self.0, call)?;
        self.1.push_item(call)?;
        Ok(2)
    }
}

/// `__index` for sequences: an integer key reads the element (nil past the end); anything else
/// goes to the methods table kept as upvalue 1. The receiver and the key are read straight from
/// Luau's value layout: a tagged receiver is one tag-to-type compare, the key one tag test.
unsafe extern "C-unwind" fn sequence_index<S: SequenceSource>(state: *mut ffi::lua_State) -> c_int {
    unsafe {
        trampoline::enter(state, || {
            let call = Call::from_raw(state);
            if let (Some(receiver), Some(key)) = (call.raw_arg(1), call.raw_arg(2))
                && (key.tag() == ffi::LUA_TINTEGER || key.tag() == ffi::LUA_TNUMBER)
            {
                let sequence: &Sequence<S> = match tagged_payload::<Sequence<S>>(state, receiver) {
                    Some(sequence) => sequence,
                    None => crate::userdata::check_receiver::<Sequence<S>>(call.arg(1))?,
                };
                // An exact integer key selects an element; a fractional or out-of-range number is
                // no element (nil), as in a table, never a rounded neighbour.
                let index =
                    <crate::convert::Exact<i64> as crate::convert::FromView<'_>>::from_raw_arg(key, || call.arg(2))
                        .ok()
                        .map(|index| index.0);
                match index
                    .and_then(|i| i.checked_sub(1))
                    .and_then(|i| usize::try_from(i).ok())
                    .and_then(|i| sequence.0.get(i))
                {
                    Some(item) => item.push_item(&call)?,
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

/// The payload of `raw` when it is a userdata carrying the tag this VM gave `T`: no API call,
/// one array read and a `TypeId` compare. `None` for anything else (an untagged `T` included;
/// the caller falls back to the full receiver check).
///
/// # Safety
/// `raw` is a live slot of `state`.
unsafe fn tagged_payload<'a, T: Userdata>(state: *mut ffi::lua_State, raw: &crate::convert::RawValue) -> Option<&'a T> {
    if raw.tag() != ffi::LUA_TUSERDATA {
        return None;
    }
    // SAFETY: the tag says userdata.
    let (tag, data) = unsafe { raw.userdata() };
    if tag == 0 {
        return None;
    }
    // SAFETY: a live state (forwarded contract).
    let shared = unsafe { crate::runtime::shared_for(state) }?;
    if shared.type_of_tag(c_int::from(tag)) != Some(std::any::TypeId::of::<T>()) {
        return None;
    }
    // SAFETY: a tagged userdata of this VM's tag for `T` holds a `T` at its data pointer, for
    // as long as the slot keeps it alive.
    Some(unsafe { &*data.cast::<T>() })
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
    ty.array_iterator(|sequence: &Sequence<S>, control: i64| -> Option<IterStep<S::Item>> {
        let index = usize::try_from(control).ok()?;
        sequence.0.get(index).map(|item| IterStep(control + 1, item))
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
                item.push_item(frame)?;
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
        |cursor: Cursor<'_, S::Cursor>, control: Option<i64>| -> Option<IterStep<S::Item>> {
            S::next(&cursor).map(|item| IterStep(control.unwrap_or(0) + 1, item))
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
