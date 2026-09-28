//! The extension-layer primitives: zero-copy bytes, strict options, sequence and stream views,
//! packed scalars.

use std::cell::Cell;

use l3i::bind::Call;
use l3i::convert::{BufferView, BytesView, Integer};
use l3i::extension::{Extension, ExtensionDescriptor, InstallContext, RuntimePlan, RuntimePolicy};
use l3i::options::{FromOptions, Options};
use l3i::packed::{BufferPack, Packed, PackedScalar};
use l3i::sequence::{Sequence, SequenceSource, Stream, StreamSource};
use l3i::{Result, Runtime};

// ---- bytes -----------------------------------------------------------------------------------

#[test]
fn bytes_view_reads_strings_and_buffers_without_copying() {
    let runtime = Runtime::new().unwrap();
    let total = runtime
        .bind_function("dreamweave.tests.total", |bytes: BytesView| {
            bytes.with_bytes(|b| b.iter().map(|&x| i64::from(x)).sum::<i64>())
        })
        .unwrap();
    runtime.set_global("total", &total).unwrap();
    runtime
        .exec(
            "assert(total('\\1\\2\\3') == 6) local b = buffer.create(4) buffer.writeu8(b, 0, 10) buffer.writeu8(b, 3, 5) \
             assert(total(b) == 15) local ok = pcall(total, 42) assert(not ok)",
        )
        .unwrap();
    // Mutable scoped access and ranges.
    let fill = runtime
        .bind_function("dreamweave.tests.fill", |mut buffer: BufferView, value: i64| {
            buffer.with_bytes_mut(|bytes| bytes.fill(value as u8));
            buffer.range(1, 2).unwrap().with_bytes(|b| i64::from(b[0]) + i64::from(b[1]))
        })
        .unwrap();
    runtime.set_global("fill", &fill).unwrap();
    runtime.exec("local b = buffer.create(4) assert(fill(b, 7) == 14) assert(buffer.readu8(b, 3) == 7)").unwrap();
    let error =
        runtime.bind_function("dreamweave.tests.bad", |buffer: BufferView| buffer.range(3, 2).map(|_| 0i64)).unwrap();
    runtime.set_global("bad", &error).unwrap();
    let message = runtime.exec("bad(buffer.create(4))").unwrap_err().to_string();
    assert!(message.contains("buffer access out of bounds"), "{message}");
}

// ---- options ---------------------------------------------------------------------------------

#[derive(Debug, PartialEq)]
struct Extract {
    path: String,
    force: bool,
    limit: Option<i64>,
    nested: Option<i64>,
}

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

#[test]
fn options_are_strict_and_precise() {
    let runtime = Runtime::new().unwrap();
    let parse = runtime
        .bind_function("dreamweave.tests.parse", |call: &Call, view: l3i::stack::ValueView| {
            let extract = Extract::read(call, view, "archive:extract")?;
            Ok::<String, l3i::Error>(format!("{extract:?}"))
        })
        .unwrap();
    runtime.set_global("parse", &parse).unwrap();
    let cases = [
        ("{ path = 'a' }", "Ok: Extract { path: \"a\", force: false, limit: None, nested: None }"),
        (
            "{ path = 'a', force = true, limit = 3i, nested = { depth = 2i } }",
            "Ok: Extract { path: \"a\", force: true, limit: Some(3), nested: Some(2) }",
        ),
        ("{ }", "archive:extract: missing required option 'path'"),
        (
            "{ path = 'a', frce = true }",
            "archive:extract: unknown option 'frce'; known options are force, limit, nested, path",
        ),
        (
            "{ path = 'a', x = 1, y = 2 }",
            "archive:extract: unknown options 'x', 'y'; known options are force, limit, nested, path",
        ),
        ("{ path = 5 }", "archive:extract.path: "),
        (
            "{ path = 'a', nested = { depth = 1i, dept = 2i } }",
            "archive:extract.nested: unknown option 'dept'; known options are depth",
        ),
        ("{ path = 'a', nested = { } }", "archive:extract.nested: missing required option 'depth'"),
        ("{ [1] = 'a' }", "archive:extract: option keys must be strings, got number"),
        ("nil", "archive:extract: missing options table"),
        ("7", "archive:extract: options must be a table, got number"),
    ];
    for (source, expected) in cases {
        let script = format!("local ok, result = pcall(parse, {source}) return ok and ('Ok: ' .. result) or result");
        let function = runtime.load_function(&format!("return function() {script} end")).unwrap();
        let text: String = function.invoke(&runtime.stack(), ()).unwrap();
        assert!(text.contains(expected), "{source}: {text}");
    }
}

// ---- sequences and streams -------------------------------------------------------------------

struct Numbers(Vec<i64>);

impl SequenceSource for Numbers {
    const NAME: &'static str = "dreamweave.tests.Numbers";
    type Item = i64;
    fn len(&self) -> usize {
        self.0.len()
    }
    fn get(&self, index: usize) -> Option<i64> {
        self.0.get(index).copied()
    }
}

struct Countdown(i64);

impl StreamSource for Countdown {
    const NAME: &'static str = "dreamweave.tests.Countdown";
    type Item = i64;
    type Cursor = Cell<i64>;
    fn open(&self) -> Cell<i64> {
        Cell::new(self.0)
    }
    fn next(cursor: &Cell<i64>) -> Option<i64> {
        let value = cursor.get();
        if value <= 0 {
            return None;
        }
        cursor.set(value - 1);
        Some(value)
    }
}

struct Views;

impl Extension for Views {
    fn id(&self) -> &'static str {
        "dream.views"
    }

    fn describe(&self, d: &mut ExtensionDescriptor) -> Result<()> {
        d.sequence::<Numbers>("dreamweave.tests.Numbers").tag(l3i::extension::TagPolicy::Preferred);
        d.stream::<Countdown>("dreamweave.tests.Countdown").tag(l3i::extension::TagPolicy::Never);
        d.module("@dream/views");
        Ok(())
    }

    fn install(&self, cx: &mut InstallContext<'_>) -> Result<()> {
        cx.sequence::<Numbers>("dreamweave.tests.Numbers")?;
        cx.stream::<Countdown>("dreamweave.tests.Countdown")?;
        let mut module = cx.module("@dream/views")?;
        module
            .function("numbers", |call: &Call, count: i64| {
                Sequence::push(call, Numbers((1..=count).map(|n| n * 10).collect())).map(l3i::value::Value::store)?
            })?
            .function("countdown", |call: &Call, from: i64| {
                Stream::push(call, Countdown(from)).map(l3i::value::Value::store)?
            })?;
        module.finish()?;
        Ok(())
    }
}

#[test]
fn sequences_index_iterate_measure_and_materialise() {
    let plan = RuntimePlan::builder()
        .policy(RuntimePolicy::new().compat_global("@dream/views", "views"))
        .extension(Views)
        .finalize()
        .unwrap();
    for _ in 0..2 {
        let runtime = Runtime::from_plan(&plan).unwrap();
        runtime
            .exec(
                "local s = views.numbers(4) assert(#s == 4, 'len') assert(s[1] == 10 and s[4] == 40, 'index') \
                 assert(s[5] == nil and s[0] == nil, 'past the end') \
                 local sum = 0 for i, v in s do sum = sum + v assert(s[i] == v) end assert(sum == 100, 'iterate') \
                 local t = s:toTable() assert(#t == 4 and t[2] == 20, 'toTable') assert(s.nothing == nil, 'method miss') \
                 local seen = {} for _, v in views.countdown(3) do table.insert(seen, v) end \
                 assert(#seen == 3 and seen[1] == 3 and seen[3] == 1, 'stream') \
                 local a, b = 0, 0 for _, v in views.countdown(2) do for _, w in views.countdown(2) do a = a + 1 end b = b + 1 end \
                 assert(a == 4 and b == 2, 'nested streams keep private cursors')",
            )
            .unwrap();
    }
}

// ---- packed scalars --------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq)]
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

#[test]
fn packed_scalars_cross_as_integers_with_a_kind_check() {
    let runtime = Runtime::new().unwrap();
    let make =
        runtime.bind_function("dreamweave.tests.make", |id: i64| Packed(Handle { id: id as u32, flags: 0xA })).unwrap();
    let read =
        runtime.bind_function("dreamweave.tests.read", |handle: Packed<Handle>| i64::from(handle.0.id) * 2).unwrap();
    runtime.set_global("make", &make).unwrap();
    runtime.set_global("read", &read).unwrap();
    runtime
        .exec(
            "local h = make(21) assert(type(h) == 'number' or type(h) == 'integer', type(h)) assert(read(h) == 42) \
             local ok, err = pcall(read, 21i) assert(not ok and string.find(err, 'Handle'), err) \
             local ok2 = pcall(read, 1.5) assert(not ok2)",
        )
        .unwrap();
    let bits = Packed(Handle { id: 7, flags: 3 }).bits();
    assert_eq!(Packed::<Handle>::from_bits(bits).unwrap().0, Handle { id: 7, flags: 3 });
    // Through a buffer.
    let store = runtime
        .bind_function("dreamweave.tests.store", |buffer: BufferView, handle: Packed<Handle>| {
            buffer.write_packed(0, &handle)
        })
        .unwrap();
    let load = runtime
        .bind_function("dreamweave.tests.load", |buffer: BufferView| {
            buffer.read_packed::<Packed<Handle>>(0).map(|h| Integer(i64::from(h.0.id)))
        })
        .unwrap();
    runtime.set_global("store", &store).unwrap();
    runtime.set_global("load", &load).unwrap();
    runtime.exec("local b = buffer.create(8) store(b, make(99)) assert(load(b) == 99i)").unwrap();
    let mut bytes = [0u8; 8];
    Packed(Handle { id: 1, flags: 0 }).write_to(&mut bytes).unwrap();
    assert_eq!(Packed::<Handle>::read_from(&bytes).unwrap().0.id, 1);
}
