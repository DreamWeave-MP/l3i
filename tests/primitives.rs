//! The extension-layer primitives: zero-copy bytes, strict options, sequence and stream views,
//! packed scalars.

use std::cell::Cell;

use l3i::bind::Call;
use l3i::convert::{Bits64, BufferView, BytesView, Exact, Integer, Push};
use l3i::extension::{Extension, ExtensionDescriptor, RuntimePlan, RuntimePolicy};
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
            bytes.to_vec().iter().map(|&x| i64::from(x)).sum::<i64>()
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
        .bind_function("dreamweave.tests.fill", |buffer: BufferView, value: i64| {
            buffer.fill(0, buffer.len(), value as u8)?;
            let range = buffer.range(1, 2)?;
            Ok::<i64, l3i::Error>(i64::from(range.read_u8(0)?) + i64::from(range.read_u8(1)?))
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
        d.module("@dream/views")
            .function("numbers", |call: &Call, count: i64| {
                Sequence::push(call, Numbers((1..=count).map(|n| n * 10).collect())).map(l3i::value::Value::store)?
            })
            .untyped()
            .function("countdown", |call: &Call, from: i64| {
                Stream::push(call, Countdown(from)).map(l3i::value::Value::store)?
            })
            .untyped();
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
                 assert(s[1.5] == nil and s[2.0] == 20 and s[0] == nil and s[2^70] == nil, 'exact index') \
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

#[test]
fn exact_integers_never_round_and_bit_patterns_keep_all_sixty_four_bits() {
    let runtime = Runtime::new().unwrap();
    let exact = runtime.bind_function("dreamweave.tests.exact", |v: Exact<i64>| v.0 * 2).unwrap();
    let unsigned = runtime.bind_function("dreamweave.tests.unsigned", |v: Exact<u64>| v.0 == 1u64 << 63).unwrap();
    let legacy = runtime.bind_function("dreamweave.tests.legacy", |v: i64| v).unwrap();
    let bits = runtime.bind_function("dreamweave.tests.bits", |b: Bits64| Bits64(b.0 ^ 1)).unwrap();
    for (name, function) in [("exact", &exact), ("unsigned", &unsigned), ("legacy", &legacy), ("bits", &bits)] {
        runtime.set_global(name, function).unwrap();
    }
    runtime
        .exec(
            "assert(exact(3) == 6 and exact(3i) == 6 and exact(-2^53) == -2^54, 'integral numbers and integers') \
             assert(not pcall(exact, 3.5), 'fraction') assert(not pcall(exact, 1/0), 'infinity') \
             assert(not pcall(exact, 0/0), 'nan') assert(not pcall(exact, 2^63), 'range') assert(not pcall(exact, 'x')) \
             assert(unsigned(2^63), 'unsigned range reaches past i64') assert(not pcall(unsigned, -1)) \
             assert(legacy(3.5) == 4, 'the compatibility conversion still rounds') \
             assert(bits(-1i) == -2i, 'all sixty-four bits, as a pattern') assert(not pcall(bits, 5), 'a number is not a pattern')",
        )
        .unwrap();
    // The full pattern round-trips through the stack.
    let all = runtime.stack().with_frame(|frame| Bits64(u64::MAX).push_into(frame)?.read::<Bits64>()).unwrap();
    assert_eq!(all, Bits64(u64::MAX));
}

#[test]
fn one_buffer_in_two_parameters_is_two_views_that_may_read_and_write_each_other() {
    // A script can hand the same buffer to both parameters; the safe API copies, so writing
    // through one view while reading the other is ordinary, never an aliased slice.
    let runtime = Runtime::new().unwrap();
    let swap = runtime
        .bind_function("dreamweave.tests.swap", |a: BufferView, b: BufferView| {
            let first = a.read_u8(0)?;
            b.write_u8(0, a.read_u8(1)?)?;
            a.write_u8(1, first)?;
            b.write_packed(0, &0x0201u16)?;
            a.read_packed::<u16>(0)
        })
        .unwrap();
    runtime.set_global("swap", &swap).unwrap();
    runtime
        .exec(
            "local b = buffer.create(2) buffer.writeu8(b, 0, 9) buffer.writeu8(b, 1, 4) \
             assert(swap(b, b) == 513) assert(buffer.readu8(b, 0) == 1 and buffer.readu8(b, 1) == 2)",
        )
        .unwrap();
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
    // Nothing crosses until the kind is registered on this VM.
    let unregistered =
        runtime.bind_function("dreamweave.tests.unregistered", |h: Packed<Handle>| i64::from(h.0.id)).unwrap();
    runtime.set_global("unregistered", &unregistered).unwrap();
    let error = runtime.exec("unregistered(5i)").unwrap_err().to_string();
    assert!(error.contains("Handle (kind 5) is not registered"), "{error}");
    let error = runtime
        .stack()
        .with_frame(|frame| Packed(Handle { id: 1, flags: 0 }).push_into(frame).map(|_| ()))
        .unwrap_err()
        .to_string();
    assert!(error.contains("not registered"), "{error}");
    runtime.register_packed::<Handle>().unwrap();
    runtime.register_packed::<Handle>().unwrap();
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
    let bits = Packed(Handle { id: 7, flags: 3 }).bits().unwrap();
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

    // One number, one owner per VM; l3i's own numbers are never a host's.
    #[derive(Debug)]
    struct Rival;
    impl PackedScalar for Rival {
        const KIND: u8 = 5;
        const NAME: &'static str = "Rival";
        fn pack(&self) -> (u64, u8) {
            (0, 0)
        }
        fn unpack(_: u64, _: u8) -> Result<Self> {
            Ok(Rival)
        }
    }
    let error = runtime.register_packed::<Rival>().unwrap_err().to_string();
    assert!(error.contains("kind 5 is already registered to Handle"), "{error}");
    let rival = runtime.bind_function("dreamweave.tests.rival", |_: Packed<Rival>| 0i64).unwrap();
    runtime.set_global("rival", &rival).unwrap();
    let error = runtime.exec("rival(make(1))").unwrap_err().to_string();
    assert!(error.contains("registered to Handle in this runtime, not Rival"), "{error}");
    #[derive(Debug)]
    struct Squatter;
    impl PackedScalar for Squatter {
        const KIND: u8 = 3;
        const NAME: &'static str = "Squatter";
        fn pack(&self) -> (u64, u8) {
            (0, 0)
        }
        fn unpack(_: u64, _: u8) -> Result<Self> {
            Ok(Squatter)
        }
    }
    let error = runtime.register_packed::<Squatter>().unwrap_err().to_string();
    assert!(error.contains("kind 3, which belongs to l3i (Color)"), "{error}");
    let mut bytes = [0u8; 8];
    Packed(Handle { id: 1, flags: 0 }).write_to(&mut bytes).unwrap();
    assert_eq!(Packed::<Handle>::read_from(&bytes).unwrap().0.id, 1);
}

#[test]
fn options_lend_borrowed_strings_and_bytes_and_read_tables_and_eval_reads_chunk_results() {
    use l3i::stack::ValueView;
    let runtime = Runtime::new().unwrap();
    let opts = runtime
        .bind_function("dreamweave.tests.opts", |call: &Call, options: ValueView| {
            Options::read(call, options, "opts", |o| {
                let name_len = o.required_str("name", |s| Ok(s.len()))?;
                let raw = o.optional_bytes("raw", |b| Ok(b.to_vec()))?.unwrap_or_default();
                let extra: Option<l3i::value::Table> = o.optional("extra")?;
                let owner = o.optional_str("owner", |s| Ok(s.to_owned()))?;
                Ok(name_len as i64 + raw.len() as i64 + i64::from(extra.is_some()) + i64::from(owner.is_some()))
            })
        })
        .unwrap();
    runtime.set_global("opts", &opts).unwrap();
    assert_eq!(runtime.eval::<f64>("return opts({ name = 'abc', raw = '\\0\\1', extra = {} })").unwrap(), 6.0);
    let error = runtime.eval::<f64>("return opts({ name = 7 })").unwrap_err().to_string();
    assert!(error.contains("opts.name"), "{error}");
    let error = runtime.eval::<f64>("return opts({ name = 'x', extra = 5 })").unwrap_err().to_string();
    assert!(error.contains("opts.extra") && error.contains("table"), "{error}");
    let (a, b): (f64, String) = runtime.eval("return 1 + 1, 'two'").unwrap();
    assert_eq!((a, b.as_str()), (2.0, "two"));
    runtime.eval::<()>("local _ = 1").unwrap();
}

#[test]
fn table_options_walk_in_place_and_type_errors_carry_a_path_or_an_expectation() {
    use l3i::stack::{Type, ValueView};
    let runtime = Runtime::new().unwrap();
    // `dirs` is walked through the reader's frame with one element on the stack at a time; a
    // wrong element names its path; a wrong field names the field.
    let read = runtime
        .bind_function("dreamweave.tests.dirs", |call: &Call, options: ValueView| {
            Options::read(call, options, "scan", |o| {
                let mut total = 0usize;
                let names = o.required_table("dirs", |frame, dirs| {
                    let mut names = String::new();
                    dirs.for_each_array(frame, |_, index, value| {
                        let text = value
                            .read::<&str>()
                            .map_err(|_| value.field_type_error(&format!("dirs[{index}]"), "a string"))?;
                        names.push_str(text);
                        total += 1;
                        Ok(())
                    })?;
                    Ok(names)
                })?;
                let extra = o.optional_table("extra", |frame, table| table.len(frame))?;
                Ok(format!("{names}:{total}:{}", extra.unwrap_or(0)))
            })
        })
        .unwrap();
    runtime.set_global("dirs", &read).unwrap();
    assert_eq!(
        runtime.eval::<String>("return dirs({ dirs = { 'a', 'b', 'c' }, extra = { 1, 2 } })").unwrap(),
        "abc:3:2"
    );
    assert_eq!(runtime.eval::<String>("return dirs({ dirs = {} })").unwrap(), ":0:0");
    let error = runtime.eval::<String>("return dirs({ dirs = { 'a', 7 } })").unwrap_err().to_string();
    assert!(error.contains("scan.dirs: dirs[2]: expected a string, got number"), "{error}");
    let error = runtime.eval::<String>("return dirs({ dirs = 'nope' })").unwrap_err().to_string();
    assert!(error.contains("scan.dirs: Lua stack index") && error.contains("expected table, got string"), "{error}");
    // A union expectation in words, and a stack check that takes a count.
    let union = runtime
        .bind_function("dreamweave.tests.union", |call: &Call, value: ValueView| -> l3i::Result<f64> {
            if value.type_of() == Type::Number {
                return Ok(1.0);
            }
            if value.type_of() == Type::String {
                call.stack().check(3usize)?;
                return Ok(2.0);
            }
            Err(value.type_error_expecting("an entry handle or an archive path"))
        })
        .unwrap();
    runtime.set_global("union", &union).unwrap();
    assert_eq!(runtime.eval::<f64>("return union(1) + union('x')").unwrap(), 3.0);
    let error = runtime.eval::<f64>("return union(true)").unwrap_err().to_string();
    assert!(error.contains("expected an entry handle or an archive path, got boolean"), "{error}");
    // Walking a large array keeps the stack flat: far more elements than Luau's C stack limit.
    let big = runtime
        .bind_function("dreamweave.tests.big", |call: &Call, options: ValueView| {
            Options::read(call, options, "big", |o| {
                o.required_table("items", |frame, items| {
                    let mut sum = 0i64;
                    items.for_each_array(frame, |frame, _, value| {
                        // The visitor may push temporaries; they go with the element.
                        1i64.push_into(frame)?;
                        sum += value.read::<i64>()?;
                        Ok(())
                    })?;
                    Ok(sum)
                })
            })
        })
        .unwrap();
    runtime.set_global("big", &big).unwrap();
    assert_eq!(runtime.eval::<f64>("local t = table.create(20000, 1) return big({ items = t })").unwrap(), 20000.0);
}
