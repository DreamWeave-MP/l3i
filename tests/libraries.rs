//! Selective library opening, Luau's own sandbox, registered C libraries, table utilities,
//! comparison and concatenation through metamethods, the string builder, environments for
//! loaded chunks, compile-time library members, and the inliner toggle.

use std::ffi::c_int;
use std::rc::Rc;

use l3i::Runtime;
use l3i::ffi;
use l3i::libraries::{Library, StringBuilder};
use l3i::source::{CompileConstant, CompileOptions, LibraryMembers};
use l3i::thread::Resume;
use l3i::value::{Function, Table, Value};

#[test]
fn libraries_open_one_at_a_time() {
    let runtime = Runtime::builder().standard_libraries(false).build().unwrap();
    runtime.open_library(Library::Base).unwrap();
    runtime.open_library(Library::Math).unwrap();
    runtime.exec("assert(math.sqrt(16) == 4) assert(string == nil) assert(type(pcall) == 'function')").unwrap();
    runtime.open_library(Library::String).unwrap();
    runtime.exec("assert(string.rep('a', 3) == 'aaa')").unwrap();
    assert_eq!(Library::STANDARD.len(), 12);
    assert_eq!(Library::Vector.global_name(), "vector");
}

#[test]
fn luau_sandbox_freezes_globals_and_threads_get_writable_proxies() {
    let runtime = Runtime::new().unwrap();
    runtime.exec("shared = 1").unwrap();
    runtime.sandbox_luau();
    let error = runtime.exec("shared = 2").unwrap_err().to_string();
    assert!(error.contains("readonly") || error.contains("read-only"), "{error}");
    assert!(runtime.exec("math.sqrt = nil").is_err());
    let thread = runtime.new_thread().unwrap();
    thread.sandbox(&runtime.stack()).unwrap();
    let chunk: Function = thread
        .with_stack(&runtime.stack(), |stack| {
            stack.with_frame(|frame| {
                let chunk = runtime.load(frame, "=t", "shared = 5 return shared", &CompileOptions::default())?;
                Function::from_value(Value::store(chunk)?)
            })
        })
        .unwrap();
    let Resume::Finished(values) = thread.start(&runtime.stack(), &chunk, ()).unwrap() else { panic!() };
    assert_eq!(values[0].with_value(&runtime.stack(), |_, v| v.read::<i32>()).unwrap(), 5);
    runtime.exec("assert(shared == 1)").unwrap();
}

unsafe extern "C-unwind" fn twice(state: *mut ffi::lua_State) -> c_int {
    unsafe {
        ffi::lua_pushnumber(state, ffi::lua_tonumber(state, 1) * 2.0);
        1
    }
}

unsafe extern "C-unwind" fn thrice(state: *mut ffi::lua_State) -> c_int {
    unsafe {
        ffi::lua_pushnumber(state, ffi::lua_tonumber(state, 1) * 3.0);
        1
    }
}

#[test]
fn c_libraries_register_and_dotted_tables_are_found_or_created() {
    let runtime = Runtime::new().unwrap();
    let table = runtime.register_library("mathx", &[("twice", twice), ("thrice", thrice)]).unwrap();
    assert!(table.get::<Value>(&runtime.stack(), "twice").unwrap().is_function());
    runtime.exec("assert(mathx.twice(4) == 8 and mathx.thrice(2) == 6)").unwrap();
    // Registering again reuses the table; a non-table name is a conflict.
    runtime.register_library("mathx", &[]).unwrap();
    runtime.exec("conflict = 1").unwrap();
    let error = runtime.register_library("conflict", &[("twice", twice)]).unwrap_err().to_string();
    assert!(error.contains("name conflict"), "{error}");
    assert!(runtime.register_library("", &[]).is_err());
    let nested = runtime.find_table("a.b.c").unwrap();
    nested.set(&runtime.stack(), "leaf", &7i32).unwrap();
    runtime.exec("assert(a.b.c.leaf == 7)").unwrap();
    let again = runtime.find_table("a.b").unwrap();
    assert!(again.get::<Value>(&runtime.stack(), "c").unwrap().is_table());
    let error = runtime.find_table("conflict.x").unwrap_err().to_string();
    assert!(error.contains("not a table"), "{error}");
}

#[test]
fn tables_clone_and_clear_and_values_compare_and_concatenate_through_metamethods() {
    let runtime = Runtime::new().unwrap();
    runtime
        .exec("source = setmetatable({ 1, 2, x = 3 }, { __eq = function() return true end, __lt = function(a, b) return a.x < b.x end, __concat = function(a, b) return 'cat' end })\n\
               other = setmetatable({ x = 9 }, getmetatable(source))")
        .unwrap();
    let source = Table::from_value(runtime.global("source").unwrap()).unwrap();
    let other = Table::from_value(runtime.global("other").unwrap()).unwrap();
    let stack = runtime.stack();
    stack
        .with_frame(|frame| {
            let view = source.push_to(frame)?;
            let copy = view.clone_table(frame)?;
            assert_eq!(copy.raw_len(), 2);
            assert_eq!(copy.get_as::<i32>(frame, "x")?, 3);
            assert!(!copy.is_read_only());
            copy.clear(frame)?;
            assert_eq!(copy.raw_len(), 0);
            assert_eq!(view.raw_len(), 2, "the original is untouched");
            let other_view = other.push_to(frame)?;
            assert!(frame.equal(view.value(), other_view.value())?, "__eq");
            assert!(frame.less_than(view.value(), other_view.value())?, "__lt");
            assert!(!frame.less_than(other_view.value(), view.value())?);
            frame.push_value(view.value())?;
            frame.push_value(other_view.value())?;
            assert_eq!(frame.concat(2)?.read::<String>()?, "cat");
            frame.push_string("a");
            frame.push_number(1.0);
            frame.push_string("b");
            assert_eq!(frame.concat(3)?.read::<String>()?, "a1b");
            assert!(frame.concat(99).is_err());
            Ok(())
        })
        .unwrap();
    drop(stack);
    runtime.exec("frozen = table.freeze({})").unwrap();
    let frozen = Table::from_value(runtime.global("frozen").unwrap()).unwrap();
    let error = runtime.stack().with_frame(|frame| frozen.push_to(frame)?.clear(frame)).unwrap_err().to_string();
    assert!(error.contains("readonly") || error.contains("read-only"), "{error}");
}

#[test]
fn the_string_builder_assembles_text_and_values() {
    let runtime = Runtime::new().unwrap();
    let stack = runtime.stack();
    let text = stack
        .with_frame(|frame| {
            let number = frame.push_number(42.0);
            let mut builder = StringBuilder::new(frame);
            builder.push_str("answer=").push_value(number)?.push_bytes(b";").push_str(&"x".repeat(2000));
            let result = builder.finish();
            result.read::<String>()
        })
        .unwrap();
    assert!(text.starts_with("answer=42;"));
    assert_eq!(text.len(), "answer=42;".len() + 2000, "spilled past the inline buffer");
}

#[test]
fn chunks_load_into_a_given_environment() {
    let runtime = Runtime::new().unwrap();
    let env = Table::new(&runtime.stack(), 0, 2).unwrap();
    env.set(&runtime.stack(), "seed", &10i32).unwrap();
    let result: i32 = runtime
        .stack()
        .with_frame(|frame| {
            let chunk = runtime.load_with_env(
                frame,
                "=env",
                "answer = seed + 1 return answer",
                &CompileOptions::default(),
                &env,
            )?;
            chunk.as_function()?.invoke::<i32, ()>(frame, ())
        })
        .unwrap();
    assert_eq!(result, 11);
    assert_eq!(env.get::<i32>(&runtime.stack(), "answer").unwrap(), 11);
    assert!(runtime.global("answer").unwrap().is_nil(), "writes went to the environment");
}

struct Limits;

impl LibraryMembers for Limits {
    fn member_type(&self, library: &str, member: &str) -> Option<u8> {
        (library == "limits" && member == "max").then_some(2) // LBC_TYPE_NUMBER
    }
    fn member_constant(&self, library: &str, member: &str) -> Option<CompileConstant> {
        match (library, member) {
            ("limits", "max") => Some(CompileConstant::Number(42.0)),
            ("limits", "name") => Some(CompileConstant::String("l3i".to_owned())),
            _ => None,
        }
    }
}

#[test]
fn compile_time_library_members_fold_into_constants() {
    let runtime = Runtime::new().unwrap();
    let options = CompileOptions {
        known_libraries: vec![c"limits".to_owned()],
        library_members: Some(Rc::new(Limits)),
        ..CompileOptions::default()
    };
    // `limits` is not defined at run time: only a folded constant can make this return 42.
    let (max, name): (f64, String) = runtime
        .stack()
        .with_frame(|frame| {
            let chunk = runtime.load(frame, "=folded", "return limits.max, limits.name", &options)?;
            chunk.as_function()?.invoke::<(f64, String), ()>(frame, ())
        })
        .unwrap();
    assert_eq!((max, name.as_str()), (42.0, "l3i"));
    // Without the callbacks the same chunk indexes a nil global.
    let error = runtime
        .stack()
        .with_frame(|frame| {
            let chunk = runtime.load(frame, "=plain", "return limits.max", &CompileOptions::default())?;
            chunk.as_function()?.invoke::<f64, ()>(frame, ())
        })
        .unwrap_err()
        .to_string();
    assert!(error.contains("attempt to index nil"), "{error}");
}

#[test]
fn the_inliner_toggle_is_accepted() {
    let runtime = Runtime::new().unwrap();
    runtime.set_jit_inliner(true);
    runtime
        .exec(
            "local function f(x) return x + 1 end local s = 0 for i = 1, 1000 do s = s + f(i) end assert(s == 501500)",
        )
        .unwrap();
    runtime.set_jit_inliner(false);
    runtime.exec("assert(1 + 1 == 2)").unwrap();
}
