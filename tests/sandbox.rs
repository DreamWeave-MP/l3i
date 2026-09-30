//! Sandboxes, instances, and templates (luastate.cpp `runInNewSandbox` and friends).

use std::cell::RefCell;
use std::rc::Rc;

use l3i::Runtime;
use l3i::runtime::{CallContext, MemoryCategory};
use l3i::sandbox::{InstanceSpec, Sandbox, SandboxOptions};
use l3i::value::Value;

type Log = Rc<RefCell<Vec<String>>>;

fn sandbox_with_log(runtime: &Runtime, options: SandboxOptions) -> (Sandbox, Log) {
    let log: Log = Rc::new(RefCell::new(Vec::new()));
    let sink = log.clone();
    let sandbox = runtime.sandbox(move |line| sink.borrow_mut().push(line.to_owned()), options).unwrap();
    (sandbox, log)
}

fn context() -> CallContext {
    CallContext { id: 1, category: MemoryCategory(0) }
}

#[test]
fn the_base_env_hides_escape_hatches_and_freezes_library_tables() {
    let runtime = Runtime::new().unwrap();
    let (sandbox, _log) = sandbox_with_log(&runtime, SandboxOptions::default());
    let probe = runtime
        .load_function(
            "return function(env) return env.getfenv == nil and env.setfenv == nil and env.newproxy == nil \
             and env.print == nil and env.require == nil and env._G == nil and type(env.math) == 'table' \
             and env.math.sqrt(16) == 4 and pcall(function() env.math.sqrt = nil end) == false \
             and pcall(function() env.x = 1 end) == false and rawequal(env.getmetatable, getmetatable) == false end",
        )
        .unwrap();
    assert!(probe.invoke::<bool, _>(&runtime.stack(), (sandbox.base_env(),)).unwrap());
    assert!(sandbox.common_packages().contains_key("math"));
    assert!(sandbox.common_packages().contains_key("string"));
    assert!(!sandbox.common_packages().contains_key("print"));
}

#[test]
fn instances_get_their_own_globals_print_and_require() {
    let runtime = Runtime::new().unwrap();
    let (mut sandbox, log) = sandbox_with_log(&runtime, SandboxOptions::default());
    let shared = runtime.load_function("return function() return { made = 0 } end").unwrap();
    // A function package is a factory called with the hidden data.
    let factory =
        runtime.load_function("return function(hidden) return { hidden = hidden, tag = 'factory' } end").unwrap();
    sandbox.add_common_package(&runtime, "shared", shared.into_value()).unwrap();
    sandbox.add_common_package(&runtime, "made", factory.into_value()).unwrap();
    let hidden = runtime.load_function("return function() return { id = 42 } end").unwrap();
    let hidden: Value = hidden.invoke::<Value, _>(&runtime.stack(), ()).unwrap();
    let loader = runtime
        .load_function(
            "return function(name, env) return function(n) if n == 'answer' then return 42 end error('module ' .. n .. ' not found') end end",
        )
        .unwrap();
    let spec = InstanceSpec { name: "scriptA", packages: &[], hidden_data: Some(&hidden), loader: &loader };
    let a = sandbox.new_instance(&runtime, &spec).unwrap();
    let b = sandbox.new_instance(&runtime, &InstanceSpec { name: "scriptB", ..spec }).unwrap();

    let template = sandbox
        .load_template(
            &runtime,
            "script.lua",
            "counter = (counter or 0) + 1\n\
             print('hello', counter, _G == getfenv and 'same' or 'x')\n\
             local made = require('made')\n\
             assert(made.hidden.id == 42 and made.tag == 'factory')\n\
             assert(require('answer') == 42)\n\
             assert(require('shared').made == 0)\n\
             assert(pcall(require, 'missing') == false)\n\
             assert(math.sqrt(16) == 4)\n\
             assert(pcall(function() math.sqrt = nil end) == false)\n\
             return counter, _G",
        )
        .unwrap();
    assert_eq!(template.chunk_name(), "script.lua");
    let first = sandbox.run(&runtime, &template, &a, context()).unwrap();
    let second = sandbox.run(&runtime, &template, &a, context()).unwrap();
    let other = sandbox.run(&runtime, &template, &b, context()).unwrap();
    let count = |values: &Vec<Value>| values[0].with_value(&runtime.stack(), |_, view| view.read::<i32>()).unwrap();
    assert_eq!((count(&first), count(&second), count(&other)), (1, 2, 1), "instances keep separate globals");
    assert_eq!(log.borrow().as_slice(), ["scriptA:\thello\t1\tx", "scriptA:\thello\t2\tx", "scriptB:\thello\t1\tx"]);
    // The real globals are untouched by sandboxed writes.
    assert!(runtime.global("counter").unwrap().is_nil());
    // The instance's `made` package was produced once per instance and kept in `loaded`.
    assert!(a.loaded.get::<Value>(&runtime.stack(), "made").unwrap().is_table());
    assert!(a.env.get::<Value>(&runtime.stack(), "print").unwrap().is_function());
}

#[test]
fn templates_reject_binary_chunks_and_report_syntax_errors() {
    let runtime = Runtime::new().unwrap();
    let (sandbox, _log) = sandbox_with_log(&runtime, SandboxOptions::default());
    let error = sandbox.load_template(&runtime, "bin.luac", "\x1bLua garbage").unwrap_err().to_string();
    assert!(error.contains("Binary Lua/Luau chunks are not supported: bin.luac"), "{error}");
    let error = sandbox.load_template(&runtime, "bad.lua", "local = 1").unwrap_err().to_string();
    assert_eq!(error, "[string \"bad.lua\"]:1: Expected identifier when parsing variable name, got '='");
    assert_eq!(runtime.stack().top(), 0);
}

#[test]
fn templates_resolve_their_imports_against_the_base_env() {
    let runtime = Runtime::new().unwrap();
    runtime.exec("lib = { answer = 42 }").unwrap();
    let (sandbox, log) = sandbox_with_log(&runtime, SandboxOptions::default());
    let loader = runtime.load_function("return function(name) error('module ' .. name .. ' not found') end").unwrap();
    let instance = sandbox
        .new_instance(&runtime, &InstanceSpec { name: "a", packages: &[], hidden_data: None, loader: &loader })
        .unwrap();
    // A chunk of its own assigning `lib` would compile the read as a global lookup; a shadow
    // written by another chunk is what the safe environments trade for the fast import path.
    let shadow = sandbox.load_template(&runtime, "shadow.lua", "lib = { answer = 1 }").unwrap();
    sandbox.run(&runtime, &shadow, &instance, context()).unwrap();
    let reader = sandbox.load_template(&runtime, "reader.lua", "return lib.answer, rawget(_G, 'lib').answer").unwrap();
    let results = sandbox.run(&runtime, &reader, &instance, context()).unwrap();
    let read = |value: &Value| value.with_value(&runtime.stack(), |_, view| view.read::<f64>()).unwrap();
    assert_eq!((read(&results[0]), read(&results[1])), (42.0, 1.0));
    // Names the base env leaves out, `print` and `require`, still resolve per instance.
    let printer = sandbox.load_template(&runtime, "printer.lua", "print('hi') return type(require)").unwrap();
    let results = sandbox.run(&runtime, &printer, &instance, context()).unwrap();
    assert_eq!(results[0].with_value(&runtime.stack(), |_, view| view.read::<String>()).unwrap(), "function");
    assert_eq!(log.borrow().as_slice(), ["a:\thi"]);
}

#[test]
fn instantiating_without_an_environment_runs_in_the_real_globals() {
    let runtime = Runtime::new().unwrap();
    let (sandbox, _log) = sandbox_with_log(&runtime, SandboxOptions::default());
    let template = sandbox.load_template(&runtime, "lib.lua", "top_level_marker = 7 return 1").unwrap();
    let function = sandbox.instantiate(&runtime, &template, None).unwrap();
    assert_eq!(function.invoke::<i32, _>(&runtime.stack(), ()).unwrap(), 1);
    assert_eq!(
        runtime.global("top_level_marker").unwrap().with_value(&runtime.stack(), |_, v| v.read::<i32>()).unwrap(),
        7
    );
}

#[test]
fn compat_iterators_honour_pairs_metamethods_on_views_and_userdata() {
    let runtime = Runtime::new().unwrap();
    let (_sandbox, _log) = sandbox_with_log(&runtime, SandboxOptions::default());
    let backing = runtime.load_function("return function() return { a = 1, b = 2 } end").unwrap();
    let backing = l3i::value::Table::from_value(backing.invoke::<Value, _>(&runtime.stack(), ()).unwrap()).unwrap();
    let view = l3i::readonly::make_read_only_view(&runtime, &backing).unwrap();
    let count = runtime
        .load_function("return function(t) local n = 0 for k, v in pairs(t) do n = n + v end return n end")
        .unwrap();
    assert_eq!(count.invoke::<i32, _>(&runtime.stack(), (&view,)).unwrap(), 3);
    let plain = runtime
        .load_function("return function() local n = 0 for i, v in ipairs({4, 5}) do n = n + v end return n end")
        .unwrap();
    assert_eq!(plain.invoke::<i32, _>(&runtime.stack(), ()).unwrap(), 9);
    let error = runtime
        .exec("for k in pairs(newproxy and newproxy(true) or coroutine.running()) do end")
        .unwrap_err()
        .to_string();
    assert!(error.contains("without iterator support") || error.contains("attempt to iterate"), "{error}");
}

#[test]
fn string_format_compat_applies_tostring_to_percent_s() {
    let runtime = Runtime::new().unwrap();
    let options = SandboxOptions { compat_string_format: true, ..SandboxOptions::default() };
    let (_sandbox, _log) = sandbox_with_log(&runtime, options);
    let format = runtime
        .load_function(
            "return function() return string.format('%s|%5s|%-3s|%%|%d|%s', {}, true, nil, 7, setmetatable({}, { __tostring = function() return 'custom' end })) end",
        )
        .unwrap();
    let text = format.invoke::<String, _>(&runtime.stack(), ()).unwrap();
    assert!(text.starts_with("table: 0x"), "{text}");
    assert!(text.ends_with("| true|nil|%|7|custom"), "{text}");
    let error = runtime.exec("string.format(42)").is_ok();
    assert!(error, "numbers are strings for string.format");
}

#[test]
fn neutered_randomseed_is_a_no_op() {
    let runtime = Runtime::new().unwrap();
    let options = SandboxOptions { neuter_randomseed: true, ..SandboxOptions::default() };
    let (_sandbox, _log) = sandbox_with_log(&runtime, options);
    let probe = runtime
        .load_function("return function() math.randomseed(1) local a = math.random(1, 1000000) math.randomseed(1) return a ~= math.random(1, 1000000) end")
        .unwrap();
    assert!(probe.invoke::<bool, _>(&runtime.stack(), ()).unwrap(), "reseeding does not reset the sequence");
}

#[test]
fn a_rust_require_loader_can_instantiate_templates_from_inside_the_call() {
    use l3i::bind::{Call, StackResults};
    use std::rc::Rc;
    let runtime = Rc::new(Runtime::new().unwrap());
    let (sandbox, _log) = sandbox_with_log(&runtime, SandboxOptions::default());
    let sandbox = Rc::new(sandbox);
    // `require('util')` compiles the module source on demand, on the calling scope, while the
    // host's root stack is suspended inside the script call.
    let loader = {
        let sandbox = Rc::clone(&sandbox);
        runtime
            .bind_function(
                "dreamweave.test.loader",
                move |call: &Call, name: &str, env: Value| -> l3i::Result<StackResults> {
                    if name != "util" {
                        return Err(l3i::Error::runtime(format!("module '{name}' not found")));
                    }
                    let env = l3i::value::Table::from_value(env)?;
                    let template = sandbox.load_template_in(
                        call,
                        "util.lua",
                        "return function(name) return { twice = function(x) return x * 2 end, name = name } end",
                    )?;
                    let factory = sandbox.instantiate_in(call, &template, Some(&env))?;
                    factory.invoke::<l3i::value::Function, _>(call, ())?.value().push_to_scope(call)?;
                    Ok(StackResults)
                },
            )
            .unwrap()
    };
    let instance = sandbox
        .new_instance(&runtime, &InstanceSpec { name: "consumer", packages: &[], hidden_data: None, loader: &loader })
        .unwrap();
    let script = sandbox
        .load_template(
            &runtime,
            "consumer.lua",
            "local util = require('util') assert(util.name == 'util') return util.twice(21)",
        )
        .unwrap();
    let results = sandbox.run(&runtime, &script, &instance, context()).unwrap();
    assert_eq!(results[0].with_value(&runtime.stack(), |_, view| view.read::<i32>()).unwrap(), 42);
    drop(script);
    drop(instance);
    drop(loader);
    drop(sandbox);
}
