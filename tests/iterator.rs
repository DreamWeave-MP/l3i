//! Ported from `testluaubinding.cpp` (stateless, keyed, and cursor iterators).

use std::cell::Cell;

use l3i::bind::Call;
use l3i::stack::ValueView;
use l3i::userdata::iterator::Cursor;
use l3i::userdata::{Userdata, tagged};
use l3i::{Result, Runtime};

struct Sequence;

unsafe impl Userdata for Sequence {
    const NAME: &'static str = "dreamweave.tests.Sequence";
}

struct Keyed;

unsafe impl Userdata for Keyed {
    const NAME: &'static str = "dreamweave.tests.Keyed";
}

struct Counted {
    limit: f64,
}

unsafe impl Userdata for Counted {
    const NAME: &'static str = "dreamweave.tests.Counted";
}

struct CountCursor {
    current: Cell<f64>,
    end: f64,
}

fn push_global<T: Userdata>(runtime: &Runtime, name: &str, value: T) {
    let stack = runtime.stack();
    let frame = stack.frame();
    tagged::push(&frame, value).unwrap();
    frame.set_global(name).unwrap();
}

#[test]
fn stateless_iterator_reuses_the_generator_and_preserves_state() {
    let runtime = Runtime::new().unwrap();
    tagged::register::<Sequence>(&runtime, 40, |ty| {
        ty.array_iterator(|_: &Sequence, index: f64| -> Option<(f64, f64)> {
            if index >= 3.0 { None } else { Some((index + 1.0, (index + 1.0) * 10.0)) }
        })
    })
    .unwrap();
    push_global(&runtime, "subject", Sequence);
    runtime
        .exec(
            "local values = {} for index, value in subject do values[#values + 1] = tostring(index) .. ':' .. tostring(value) end \
             assert(table.concat(values, ',') == '1:10,2:20,3:30', table.concat(values, ','))",
        )
        .unwrap();
    // Two loops over the same object share the generator and see the same sequence.
    runtime
        .exec(
            "local a = {} for i, v in subject do a[#a+1] = v end local b = {} for i, v in subject do b[#b+1] = v end \
             assert(#a == 3 and #b == 3 and a[1] == 10 and b[3] == 30)",
        )
        .unwrap();
}

#[test]
fn keyed_iterator_starts_with_nil_control() {
    let runtime = Runtime::new().unwrap();
    tagged::register::<Keyed>(&runtime, 41, |ty| {
        ty.keyed_iterator(|_: &Keyed, previous: ValueView| -> Result<Option<(String, f64)>> {
            if previous.is_nil() {
                return Ok(Some(("first".to_owned(), 10.0)));
            }
            if previous.read::<&str>()? == "first" {
                return Ok(Some(("second".to_owned(), 20.0)));
            }
            Ok(None)
        })
    })
    .unwrap();
    push_global(&runtime, "keyed", Keyed);
    runtime
        .exec(
            "local values = {} for key, value in keyed do values[#values + 1] = key .. ':' .. tostring(value) end \
             assert(table.concat(values, ',') == 'first:10,second:20', table.concat(values, ','))",
        )
        .unwrap();
}

#[test]
fn cursor_iterator_creates_independent_state_for_nested_loops() {
    let runtime = Runtime::new().unwrap();
    tagged::register::<Counted>(&runtime, 42, |ty| {
        ty.cursor_iterator(
            |call: &Call| -> Result<CountCursor> {
                let counted = l3i::userdata::check_receiver::<Counted>(call.arg(1))?;
                Ok(CountCursor { current: Cell::new(0.0), end: counted.limit })
            },
            |cursor: Cursor<CountCursor>, _control: ValueView| -> Option<(f64, f64)> {
                if cursor.current.get() >= cursor.end {
                    return None;
                }
                cursor.current.set(cursor.current.get() + 1.0);
                Some((cursor.current.get(), cursor.current.get() * 10.0))
            },
        )
    })
    .unwrap();
    push_global(&runtime, "three", Counted { limit: 3.0 });
    push_global(&runtime, "two", Counted { limit: 2.0 });
    runtime
        .exec(
            "local outer, nested = {}, {} \
             for outerIndex, outerValue in three do \
                 outer[#outer + 1] = tostring(outerIndex) .. ':' .. tostring(outerValue) \
                 local inner = {} \
                 for innerIndex, innerValue in two do inner[#inner + 1] = tostring(innerIndex) .. ':' .. tostring(innerValue) end \
                 nested[#nested + 1] = table.concat(inner, ',') \
             end \
             assert(table.concat(outer, ',') == '1:10,2:20,3:30', table.concat(outer, ',')) \
             assert(table.concat(nested, ';') == '1:10,2:20;1:10,2:20;1:10,2:20', table.concat(nested, ';'))",
        )
        .unwrap();
    // Each loop gets its own cursor; the shared next rejects foreign state.
    let error = runtime
        .exec("local n = 0 for i in three do n += 1 end assert(n == 3) local mt = getmetatable(three) assert(mt == false)")
        .err();
    assert!(error.is_none(), "{error:?}");
    runtime.collect_garbage();
}

#[test]
fn iter_conflicts_are_detected() {
    let runtime = Runtime::new().unwrap();
    let error = tagged::register::<Sequence>(&runtime, 40, |ty| {
        ty.array_iterator(|_: &Sequence, _: f64| -> Option<(f64, f64)> { None })?;
        ty.keyed_iterator(|_: &Sequence, _: ValueView| -> Option<(f64, f64)> { None })
    })
    .unwrap_err();
    assert_eq!(error, l3i::Error::logic("Metatable already has an __iter metamethod"));
}
