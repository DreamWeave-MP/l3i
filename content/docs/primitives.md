+++
title = "Extension primitives"
description = "The allocation-free shapes an extension reads and returns: bytes and buffers, exact integers and bit patterns, packed layouts and packed scalars, strict option tables, flat table walks, precise type errors, and sequence and stream views."
weight = 85

[extra]
kind = "guide"
+++

These are the types the first extensions asked for, all allocation-free at the boundary. Bytes
and buffers live in `l3i::convert`, packed values in `l3i::packed`, option tables in
`l3i::options`, table walks on `l3i::stack::TableView`, and views in `l3i::sequence`. Each works
as a parameter of a bound function or a declared member, and most as a return.

## Bytes and buffers

{{ api_signature(value="enum BytesView<'v> { String(&'v [u8]), Buffer(BufferView<'v>) }") }}

`BytesView` accepts a Lua string or a Luau buffer without normalising: an API that takes "some
bytes" reads through it and never copies to decide. `len()`, `is_empty()`, `read(offset, dst)`
(a bounds-checked copy) and `to_vec()` work on both; anything else is a type error naming
`string or buffer`.

{{ api_signature(value="struct BufferView<'v>") }}

`BufferView` is a borrowed view of a Luau buffer, mutable from scripts and from the host. Safe
access goes through bounds-checked copies, never a Rust slice: a script can pass one buffer to
two parameters, so two views of the same storage are ordinary, and a safe `&[u8]` could otherwise
be held while the bytes are written through the other view or through a call back into Lua.

| Method | Does |
|---|---|
| `len()`, `is_empty()` | The buffer's size |
| `read(offset, &mut [u8])`, `write(offset, &[u8])` | Copies in or out, bounds checked once |
| `fill(offset, len, byte)` | Sets a range |
| `range(offset, len)` | A bounds-checked sub-view over the same storage |
| `read_u8`, `write_u8`, `read_i32`, `write_i32`, `read_f32`, `write_f32`, `read_f64`, `write_f64` | Scalars in host byte order |
| `read_f32x3`, `write_f32x3(offset, Vector3)` | Three consecutive f32, the semantics of `vector:writef32x3` |
| `read_packed::<T>(offset)`, `write_packed::<T>(offset, &value)` | A `BufferPack` layout through a copy |
| `to_vec()` | The whole buffer |

Bounds failures use the buffer library's own wording, `buffer access out of bounds`, so a host
method and `buffer.writef32` fail identically.

The zero-copy forms are `unsafe fn bytes_unchecked(&self) -> &[u8]` and
`unsafe fn bytes_mut_unchecked(&mut self) -> &mut [u8]`, for trusted code that proves nothing
writes the buffer while the slice lives: no call back into Lua, no write through this or another
view of the same storage. Those are the rules `lua_tobuffer` imposes on C. The built-in network
and rendering extensions use them to hand a slice to a pure Rust crate for the duration of one
call.

```rust
let swap = runtime.bind_function("dreamweave.tests.swap", |a: BufferView, b: BufferView| {
    let first = a.read_u8(0)?;
    b.write_u8(0, a.read_u8(1)?)?;
    a.write_u8(1, first)?;
    b.write_packed(0, &0x0201u16)?;
    a.read_packed::<u16>(0)
})?;
```

Called as `swap(b, b)` with one buffer, both views alias the same bytes and the call still reads
and writes through copies.

## Exact integers and bit patterns

The plain Rust integer conversions keep OpenMW's behaviour: a Lua number rounds half away from
zero, so `640.4` reads as `640`. That is compatibility, not a contract for a structural value.

{{ api_signature(value="struct Exact<T>(pub T)") }}

`Exact<T>` reads an integer that never rounds: a Luau integer in range, or a number whose
fractional part is exactly zero and that is in range, for `T` any of `i8` to `i64`, `isize`,
`u8` to `u64`, `usize`. A fraction, NaN, an infinity, or an out-of-range value is an error
(`number 3.5 is not an exact i64`, `integer <value> is out of range for <type>`). It is input
only and has no push form. Use it for indices, offsets, counts, sizes, and ids.

For results the vocabulary is:

| Type | Pushes | Luau sees |
|---|---|---|
| A plain Rust integer (`i64`, `u32`, ...) | A number | `number`, exact while it fits a double |
| `Integer(pub i64)` | A Luau integer | `integer` |
| `Bits64(pub u64)` | A Luau integer holding all 64 bits | `integer`, with no numeric meaning |

`Bits64` carries an opaque pattern (a hash, a peer id) through a Luau integer: reads accept an
integer only, a number never becomes one, and numerically the value may look negative to a script,
which is the deal for opaque ids. A numeric `u64` stays within what an integer or an exact number
holds, so nothing silently reinterprets.

{% callout(kind="note", title="Luau integers compare with == only") %}
In Luau 0.740, `<` and `<=` between two integers raise, and an integer never equals a number
(`42i ~= 42`). l3i pushes identities as integers and everything a script thresholds or counts as
plain numbers.
{% end %}

## Packed layouts and packed scalars

{{ api_signature(value="trait BufferPack: Sized { const SIZE: usize; fn read_from(bytes: &[u8]) -> Result<Self>; fn write_to(&self, bytes: &mut [u8]) -> Result<()>; }") }}

`BufferPack` is a fixed little-endian byte layout inside a buffer. A domain type states its size
and how to read and write itself, and `BufferView::read_packed` and `write_packed` own the safe
crossing: one bounds check, one copy, no per-field calls. The primitive integers, `f32`, `f64`,
`Vector3` (12 bytes), `raster::Color` (4 bytes), `raster::Color16` (8 bytes) and every
`Packed<T>` (8 bytes) implement it.

{{ api_signature(value="trait PackedScalar: Sized + 'static { const KIND: u8; const NAME: &'static str; fn pack(&self) -> (u64, u8); fn unpack(payload: u64, flags: u8) -> Result<Self>; }") }}

A `PackedScalar` is a semantic value that physically occupies one Luau integer: a 4-bit kind in
the top nibble, 4 flag bits the kind may use, and a 56-bit payload. The kind is checked on every
read, so untyped script code handing the wrong integer to a native operation fails with a type
error (`expected a packed Quaternion, got kind 3`) instead of decoding garbage. `Packed<T>(pub T)`
is the parameter and return type; `Packed::bits()` and `Packed::from_bits(i64)` convert by hand,
and `packed::encode(kind, flags, payload)` and `packed::decode(bits)` are the bit-level forms.

Kinds are a registry, not a convention, because a kind number is what a packed integer means
once it is written to a file or a socket:

| Kind | Owner | Fixed |
|---|---|---|
| 1 | `quat::Quaternion` | For good |
| 2 | `quat::AnimationKey` | For good |
| 3 | `raster::Color` | For good |
| 4 | `raster::ClipRect` | For good |
| 5 to 15 | The application's, one Rust type per number | Per plan or per runtime |
| 0 | Reserved, so a plain zero integer never passes | |

An application declares a kind per extension with `ExtensionDescriptor::packed::<T>()` or per
hand-assembled runtime with `Runtime::register_packed::<T>()`. A plan with two types on one number
does not finalize; a runtime refuses a second type on a registered number (`kind 5 is already
registered to Handle`) and a host type on one of l3i's numbers (`kind 3, which belongs to l3i
(Color)`); a `Packed<T>` crossing into or out of a VM where `T` is not the kind's registered
owner is a logic error rather than another type's bits. An encoding whose kind, flags or payload
overflow their fields is an error, never a truncated integer.

```rust
#[derive(Clone, Copy)]
struct Handle {
    id: u32,
    flags: u8,
}

impl PackedScalar for Handle {
    const KIND: u8 = 5;
    const NAME: &'static str = "Handle";
    fn pack(&self) -> (u64, u8) {
        (u64::from(self.id), self.flags)
    }
    fn unpack(payload: u64, flags: u8) -> Result<Self> {
        Ok(Handle { id: payload as u32, flags })
    }
}

runtime.register_packed::<Handle>()?;
let make = runtime.bind_function("dreamweave.tests.make", |id: i64| Packed(Handle { id: id as u32, flags: 0xA }))?;
let read = runtime.bind_function("dreamweave.tests.read", |handle: Packed<Handle>| i64::from(handle.0.id) * 2)?;
```

A script then sees `make(21)` as an integer, `read(make(21))` as `42`, and `read(21i)` as a type
error naming `Handle`.

## Strict option tables

{{ api_signature(value="fn Options::read<R>(scope: &impl Scope, view: ValueView<'_>, context: &str, body: impl FnOnce(&mut Options<'_, '_>) -> Result<R>) -> Result<R>") }}

`Options` reads a camelCase option table strictly: `read` opens a frame, checks that the value is
a table with string keys, hands the reader to `body`, and then fails if any key was never asked
for. A misspelt option is an error, not a silently ignored default. `context` is the API name the
errors carry.

| Reader | Reads |
|---|---|
| `required::<T>(key)`, `optional::<T>(key)`, `or(key, default)` | By value through `T`'s `FromView`; `Table` and `Function` qualify |
| `required_str`, `optional_str`, `required_bytes`, `optional_bytes` | `&str` or `&[u8]` borrowed for a closure, no copy |
| `with_required(key, body)`, `with_optional(key, body)` | The value's slot handed to `body`, so `&str`, `&[u8]` and `BufferView` cost no copy; the body cannot walk a table |
| `required_table(key, body)`, `optional_table(key, body)` | A table walked in place: `body` gets the reader's frame and the `TableView`, nothing pinned |
| `required_table_expecting(key, expected, body)`, `optional_table_expecting(..)` | The same, with the non-table case worded in the option's own words (`"an array of strings"`) |
| `nested(key, body)` | A nested option table under `key`, read with its own strict reader |
| `has(key)`, `context()`, `frame()` | Whether a key exists (consuming nothing), the diagnostic path, and the frame to open nested frames from |

`FromOptions` is the trait form: `from_options(&mut Options)` builds the type and
`Extract::read(scope, view, context)` calls it through `Options::read`.

```rust
impl FromOptions for Extract {
    fn from_options(o: &mut Options<'_, '_>) -> Result<Self> {
        Ok(Extract {
            path: o.required("path")?,
            force: o.or("force", false)?,
            limit: o.optional("limit")?,
            nested: o.nested("nested", |inner| inner.required::<i64>("depth"))?,
        })
    }
}
```

The errors name what the script author wrote:

| Input | Error |
|---|---|
| `{ }` | `archive:extract: missing required option 'path'` |
| `{ path = 'a', frce = true }` | `archive:extract: unknown option 'frce'; known options are force, limit, nested, path` |
| `{ path = 5 }` | `archive:extract.path: expected string, got number` |
| `{ path = 'a', nested = { } }` | `archive:extract.nested: missing required option 'depth'` |
| `{ [1] = 'a' }` | `archive:extract: option keys must be strings, got number` |
| `nil` | `archive:extract: missing options table` |
| `7` | `archive:extract: options must be a table, got number` |

Inside a table option's body, an error comes back prefixed with the reader's context and key
(`scan.dirs: ...`) unless it already starts with that path, so a `field_type_error` spelled with
the full path reads flat and a nested `Options::read` under the field's context keeps one segment
per level. An error that names a stack slot (`Lua stack index N: ...`) loses the slot: a field
error names the field.

## Walking tables

{{ api_signature(value="fn for_each_array(&self, frame: &Frame<'_>, visitor: impl FnMut(&Frame<'_>, i64, ValueView<'_>) -> Result<()>) -> Result<()>") }}

{{ api_signature(value="fn for_each(&self, frame: &Frame<'_>, visitor: impl FnMut(&Frame<'_>, ValueView<'_>, ValueView<'_>) -> Result<()>) -> Result<()>") }}

`TableView::for_each_array` visits `t[1]` to `t[rawlen]` in order with one element on the stack
at a time and no frame per element; `for_each` visits every entry through `lua_rawiter` with the
key and value views. The visitor may push temporaries; they are dropped with the element. A walk
over twenty thousand entries never nears Luau's stack limit.

```rust
let names = o.required_table("dirs", |frame, dirs| {
    let mut names = String::new();
    dirs.for_each_array(frame, |_, index, value| {
        let text = value
            .read::<&str>()
            .map_err(|_| value.field_type_error(&format!("dirs[{index}]"), "a string"))?;
        names.push_str(text);
        Ok(())
    })?;
    Ok(names)
})?;
```

With `{ dirs = { 'a', 7 } }` this fails with `scan.dirs: dirs[2]: expected a string, got number`.

## Type errors by hand

A conversion written by hand raises through `ValueView`:

| Constructor | Message |
|---|---|
| `type_error(Type::Buffer)` | `Lua stack index N: expected buffer, got string` |
| `type_error_expecting("an entry handle or an archive path")` | `Lua stack index N: expected an entry handle or an archive path, got boolean` |
| `field_type_error("dirs[2]", "a string")` | `dirs[2]: expected a string, got number` |
| `field_type_error_of("rows[1]", Type::Table)` | `rows[1]: expected table, got number` |

`Frame::check(n)` ensures `n` free slots (`lua_checkstack`) before a bulk push into one frame
without a per-element frame; inside a bound function, `call.stack().check(n)` does the same.

## Sequence and stream views

{{ api_signature(value="trait SequenceSource: 'static { const NAME: &'static str; type Item: SequenceItem; fn len(&self) -> usize; fn get(&self, index: usize) -> Option<Self::Item>; }") }}

{{ api_signature(value="trait StreamSource: 'static { const NAME: &'static str; type Item: SequenceItem; type Cursor: 'static; fn open(&self) -> Self::Cursor; fn next(cursor: &Self::Cursor) -> Option<Self::Item>; }") }}

A `Sequence<S>` shows a Rust collection to scripts as `#items`, `items[i]` (1-based, `nil` past
the end, and `nil` for a fractional or out-of-range key rather than a rounded neighbour),
`for i, item in items`, and `items:toTable()`, without materialising it: only the element a script
touches is pushed. A `Stream<S>` is the cursor-backed form for results that cannot be indexed
(directory walks, filtered queries): `for _, item in stream` opens a private cursor per loop, so
nested loops over one stream stay independent, and nothing else.

Both are ordinary userdata types. An extension declares them with `d.sequence::<S>(key)` or
`d.stream::<S>(key)`, which bind `toTable`, `__len`, `__iter` and the integer `__index`, and names
the element type with `item_type("dream_vfs_Entry")` so the definitions declare the length, the
indexer and the iterator with it. A hand-assembled runtime configures a registered type with
`sequence::configure_sequence::<S>(ty)` or `configure_stream::<S>(ty)` inside its metatable
builder. `Sequence::push(scope, source)` and `Stream::push(scope, source)` push a view; a module
function returns one through `Value::store`:

```rust
d.sequence::<Numbers>("dream.views.Numbers").item_type("integer");
d.module("@dream/views")
    .function("numbers", |call: &Call, count: Exact<i64>| {
        Sequence::push(call, Numbers((1..=count.0).map(|n| n * 10).collect())).map(Value::store)?
    })
    .signature("(count: number) -> dream_views_Numbers");
```

Elements are pushed by value through `SequenceItem`: every `Push` scalar, `String`, `Vec<u8>`,
`Integer`, `Bits64`, `Vector3`, `Value`, `Table`, `Function`, `Packed<T>`, `Option<T>`, and
`Owned<T>` for any registered `T`, which moves the row into a fresh userdata without needing
`Clone`. `IterStep(control, item)` is what an iterator step returns: the next control value and
the element.

Indexing a view reads the receiver and the key straight from Luau's value layout: a tagged
receiver is one tag-to-type compare, the key one tag test.

## What they cost

Per call, in retired instructions on the pinned Luau 0.740, from `cargo bench --bench
instructions` (see [Compatibility and performance](@/docs/performance.md)):

| Path | Instructions |
|---|---|
| Bound `(Packed<Color>) -> f64` | 364, of which eleven are the kind registry check (one owner load and a type compare) on top of the bit decode |
| Planned method `() -> f64` | 431 |
| Planned sequence `[i]` | 547 |
| Planned sequence `#` | 563 |

The rest of a view's cost is Luau's own `__index` dispatch, since the VM has no integer-key
direct path for userdata.
