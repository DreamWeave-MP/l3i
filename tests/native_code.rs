//! Native code generation (`jit` feature): Luau's code generator with the binder's hooks, the
//! `writef32x3` lowering, and a userdata field lowering written in Rust.

use std::cell::Cell;
use std::sync::atomic::{AtomicU32, Ordering};

use l3i::Runtime;
use l3i::ffi::{LUA_TNUMBER, LUA_TUSERDATA};
use l3i::native_code::hooks::{AccessSite, NamecallSite, NativeCodeHooks, NativeContext};
use l3i::native_code::ir::{IrBuilder, IrCmd, bytecode_type};
use l3i::native_code::vector_buffer::VectorBufferWriter;
use l3i::native_code::{NativeCodeGen, NativeCodeMode, NativeCodeOptions, NativeCodeStatus, module_id};
use l3i::runtime::{CallContext, MemoryCategory};
use l3i::sandbox::{InstanceSpec, SandboxOptions};
use l3i::source::CompileOptions;
use l3i::userdata::{Userdata, tagged};

static WRITER_LOWERINGS: AtomicU32 = AtomicU32::new(0);
static FIELD_LOWERINGS: AtomicU32 = AtomicU32::new(0);

/// Counts how often the default writer lowering fires, then delegates to it.
struct CountingWriter;

impl NativeCodeHooks for CountingWriter {
    fn vector_namecall_type(&self, member: &str) -> u8 {
        VectorBufferWriter.vector_namecall_type(member)
    }
    fn vector_namecall(
        &self,
        context: &NativeContext<'_>,
        build: &mut IrBuilder<'_>,
        member: &str,
        site: NamecallSite,
    ) -> bool {
        let lowered = VectorBufferWriter.vector_namecall(context, build, member, site);
        if lowered {
            WRITER_LOWERINGS.fetch_add(1, Ordering::Relaxed);
        }
        lowered
    }
}

const POINT_TAG: u8 = 12;

#[repr(C)]
struct Point {
    x: f32,
    y: f32,
}

unsafe impl Userdata for Point {
    const NAME: &'static str = "dreamweave.tests.Point";
}

/// Lowers `p.x` / `p.y` on a `Point` (userdata type index 0) to a tag check and an f32 load,
/// exactly the shape OpenMW uses for its vector types, but authored in Rust.
struct PointFields;

impl NativeCodeHooks for PointFields {
    fn userdata_access_type(&self, _: &NativeContext<'_>, userdata_type: u8, member: &str) -> u8 {
        if userdata_type == bytecode_type::TAGGED_USERDATA_BASE && matches!(member, "x" | "y") {
            bytecode_type::NUMBER
        } else {
            bytecode_type::ANY
        }
    }

    fn userdata_access(
        &self,
        context: &NativeContext<'_>,
        build: &mut IrBuilder<'_>,
        userdata_type: u8,
        member: &str,
        site: AccessSite,
    ) -> bool {
        if userdata_type != bytecode_type::TAGGED_USERDATA_BASE {
            return false;
        }
        // The tag comes from the VM being compiled for, never from a constant.
        let Some(point_tag) = context.tag_of::<Point>() else { return false };
        let offset = match member {
            "x" => 0,
            "y" => 4,
            _ => return false,
        };
        let source = build.vm_reg(site.source_reg);
        let userdata = build.inst(IrCmd::LOAD_POINTER, &[source]);
        let tag = build.const_int(i32::from(point_tag));
        let exit = build.vm_exit(site.pcpos);
        build.inst(IrCmd::CHECK_USERDATA_TAG, &[userdata, tag, exit]);
        let at = build.const_int(offset);
        let userdata_tag = build.const_tag(LUA_TUSERDATA as u8);
        let value = build.inst(IrCmd::BUFFER_READF32, &[userdata, at, userdata_tag]);
        let number = build.inst(IrCmd::FLOAT_TO_NUM, &[value]);
        let result = build.vm_reg(site.result_reg);
        build.inst(IrCmd::STORE_DOUBLE, &[result, number]);
        let number_tag = build.const_tag(LUA_TNUMBER as u8);
        build.inst(IrCmd::STORE_TAG, &[result, number_tag]);
        FIELD_LOWERINGS.fetch_add(1, Ordering::Relaxed);
        true
    }
}

fn runtime(mode: NativeCodeMode) -> Runtime {
    let options = NativeCodeOptions { hooks: vec![Box::new(CountingWriter)], ..NativeCodeOptions::default() }
        .mode(mode)
        .hooks(PointFields)
        .userdata_types(["Point"]);
    let options = NativeCodeOptions { record_counters: true, ..options };
    let runtime = Runtime::builder().native_code(options).build().unwrap();
    runtime.install_vector_buffer_writer().unwrap();
    runtime
}

fn compile_options() -> CompileOptions {
    CompileOptions { userdata_types: vec![c"Point".to_owned()], type_info_level: 1, ..CompileOptions::default() }
}

#[test]
fn writef32x3_is_lowered_to_native_stores_and_stays_exact() {
    let runtime = runtime(NativeCodeMode::Eager);
    let generator = runtime.native_code().expect("built with native code");
    assert!(generator.is_available(), "this platform has no Luau code generator");
    let sandbox = runtime
        .sandbox(|_| {}, SandboxOptions { compile_options: compile_options(), ..SandboxOptions::default() })
        .unwrap();
    let before = WRITER_LOWERINGS.load(Ordering::Relaxed);
    let template = sandbox
        .load_template(
            &runtime,
            "writer.lua",
            "local a, b = buffer.create(16), buffer.create(16)\n\
             local v = vector.create(1.5, -2.25, 1e10)\n\
             for i = 1, 3 do v:writef32x3(a, 4) end\n\
             buffer.writef32(b, 4, 1.5) buffer.writef32(b, 8, -2.25) buffer.writef32(b, 12, 1e10)\n\
             assert(buffer.tostring(a) == buffer.tostring(b), 'native stores match the library')\n\
             local ok, err = pcall(function() v:writef32x3(a, 5) end)\n\
             assert(not ok and err:find('buffer access out of bounds'), 'guard exits to the interpreter shim')\n\
             return 1",
        )
        .unwrap();
    let native = template.native_code().expect("compiled");
    assert_eq!(native.status, NativeCodeStatus::Success, "{native:?}");
    assert!(native.stats.functions_compiled >= 1 && native.stats.native_code_size_bytes > 0, "{native:?}");
    assert!(WRITER_LOWERINGS.load(Ordering::Relaxed) > before, "the Rust lowering hook ran");
    let loader = runtime.load_function("return function(name) error('module ' .. name .. ' not found') end").unwrap();
    let instance = sandbox
        .new_instance(&runtime, &InstanceSpec { name: "w", packages: &[], hidden_data: None, loader: &loader })
        .unwrap();
    let results =
        sandbox.run(&runtime, &template, &instance, CallContext { id: 1, category: MemoryCategory(0) }).unwrap();
    assert_eq!(results.len(), 1);
    let stats = generator.execution_stats(&runtime.stack());
    assert!(stats.regular_blocks_executed > 0, "{stats:?}");
    assert!(stats.vm_exits_taken > 0, "the out-of-bounds call took a VM exit: {stats:?}");
}

#[test]
fn userdata_field_access_can_be_lowered_from_rust() {
    let runtime = runtime(NativeCodeMode::Eager);
    tagged::register::<Point>(&runtime, POINT_TAG, |ty| {
        ty.property("x", |p: &Point| p.x)?;
        ty.property("y", |p: &Point| p.y)
    })
    .unwrap();
    {
        let stack = runtime.stack();
        let frame = stack.frame();
        tagged::push(&frame, Point { x: 3.0, y: 4.0 }).unwrap();
        frame.set_global("point").unwrap();
    }
    let sandbox = runtime
        .sandbox(|_| {}, SandboxOptions { compile_options: compile_options(), ..SandboxOptions::default() })
        .unwrap();
    let before = FIELD_LOWERINGS.load(Ordering::Relaxed);
    let template = sandbox
        .load_template(
            &runtime,
            "point.lua",
            "local function len(p: Point): number return math.sqrt(p.x * p.x + p.y * p.y) end\n\
             return len",
        )
        .unwrap();
    assert_eq!(template.native_code().unwrap().status, NativeCodeStatus::Success);
    assert!(FIELD_LOWERINGS.load(Ordering::Relaxed) > before, "the annotated parameter reached the Rust hook");
    let len = sandbox.instantiate(&runtime, &template, None).unwrap();
    let len: l3i::value::Function = len.invoke(&runtime.stack(), ()).unwrap();
    let point = runtime.global("point").unwrap();
    assert_eq!(len.invoke::<f64, _>(&runtime.stack(), (&point,)).unwrap(), 5.0);
    // A wrong tag at the same site exits to the interpreter, which reports the ordinary error.
    let error = len.invoke::<f64, _>(&runtime.stack(), (7,)).unwrap_err().to_string();
    assert!(error.contains("attempt to index number"), "{error}");
}

#[test]
fn annotated_mode_compiles_only_marked_modules_and_ids_are_stable() {
    let runtime = runtime(NativeCodeMode::Annotated);
    let sandbox = runtime.sandbox(|_| {}, SandboxOptions::default()).unwrap();
    let plain = sandbox.load_template(&runtime, "plain.lua", "return 1").unwrap();
    assert_eq!(plain.native_code().unwrap().status, NativeCodeStatus::NotNativeModule);
    // A trivial chunk is not profitable to compile; give the module a loop.
    let source = "--!native\nlocal s = 0 for i = 1, 100 do s += i end return s";
    let marked = sandbox.load_template(&runtime, "marked.lua", source).unwrap();
    assert_eq!(marked.native_code().unwrap().status, NativeCodeStatus::Success);
    let again = sandbox.load_template(&runtime, "marked.lua", source).unwrap();
    assert_eq!(again.native_code().unwrap().module_id, marked.native_code().unwrap().module_id);
    assert_eq!(module_id(b""), module_id(b""));
    assert_ne!(module_id(b"a"), module_id(b"b"));
    let off = Runtime::builder().native_code(NativeCodeOptions::default().mode(NativeCodeMode::Off)).build().unwrap();
    let sandbox = off.sandbox(|_| {}, SandboxOptions::default()).unwrap();
    let template = sandbox.load_template(&off, "x.lua", "--!native\nreturn 1").unwrap();
    assert_eq!(template.native_code().unwrap().status, NativeCodeStatus::Skipped);
    let _ = Cell::new(0);
}

#[test]
fn a_bound_function_reaches_the_generator_and_loads_native_modules_on_its_own_thread() {
    use l3i::bind::Call;
    use l3i::source::LoadScope;
    let eager = runtime(NativeCodeMode::Eager);
    let generator = eager.native_code().expect("built with native code");
    assert!(generator.is_available(), "this platform has no Luau code generator");
    let plain = Runtime::new().unwrap();
    assert!(NativeCodeGen::for_scope(&plain.stack()).is_none(), "no generator without native code");
    // A `require` written in Rust: it compiles the module natively because the load applies the
    // runtime's policy, and it can ask the generator itself from the call.
    let require = eager
        .bind_function("dreamweave.test.require", |call: &Call, source: &str| -> l3i::Result<f64> {
            let generator = NativeCodeGen::for_scope(call).expect("the runtime's generator, from a call");
            assert_eq!(generator.mode(), NativeCodeMode::Eager);
            let before = generator.execution_stats(call).regular_blocks_executed;
            let module = call.load_source("@module.luau", source, &CompileOptions::default())?;
            let sum = module.invoke::<f64, _>(call, ())?;
            let after = generator.execution_stats(call).regular_blocks_executed;
            assert!(after > before, "the module ran natively: {before} -> {after}");
            Ok(sum)
        })
        .unwrap();
    eager.set_global("require_source", &require).unwrap();
    let sum: f64 = eager.eval("return require_source('local s = 0 for i = 1, 100 do s += i end return s')").unwrap();
    assert_eq!(sum, 5050.0);
    // `Annotated` compiles only marked chunks: a plain one loads and runs interpreted.
    let annotated = runtime(NativeCodeMode::Annotated);
    let generator = annotated.native_code().unwrap();
    let plain_source = "local s = 0 for i = 1, 100 do s += i end return s";
    let marked_source = "--!native\nlocal s = 0 for i = 1, 100 do s += i end return s";
    let counted = |source: &str| -> u64 {
        let stack = annotated.stack();
        let before = generator.execution_stats(&stack).regular_blocks_executed;
        let chunk = stack.load_source("=chunk", source, &CompileOptions::default()).unwrap();
        assert_eq!(chunk.invoke::<f64, _>(&stack, ()).unwrap(), 5050.0);
        generator.execution_stats(&stack).regular_blocks_executed - before
    };
    assert_eq!(counted(plain_source), 0, "an unmarked chunk stays interpreted");
    assert!(counted(marked_source) > 0, "a --!native chunk runs natively");
}

#[test]
fn assembly_dumps_and_the_perf_log_describe_compiled_code() {
    use l3i::native_code::{AssemblyOptions, AssemblyTarget};
    use std::sync::{Arc, Mutex};
    let seen = Arc::new(Mutex::new(Vec::new()));
    let sink = seen.clone();
    l3i::native_code::set_perf_log(move |entry| sink.lock().unwrap().push(entry));
    let runtime = runtime(NativeCodeMode::Eager);
    let generator = runtime.native_code().unwrap();
    let function =
        runtime.load_function("return function(n) local s = 0 for i = 1, n do s += i end return s end").unwrap();
    let text = runtime
        .stack()
        .with_frame(|frame| {
            let view = function.push_to(frame)?;
            generator.assembly(frame, view.index(), AssemblyOptions { include_ir: true, ..AssemblyOptions::default() })
        })
        .unwrap();
    assert!(text.contains("bb_"), "IR blocks are printed: {}", &text[..text.len().min(200)]);
    let cross = runtime
        .stack()
        .with_frame(|frame| {
            let view = function.push_to(frame)?;
            generator.assembly(
                frame,
                view.index(),
                AssemblyOptions { target: AssemblyTarget::A64, ..AssemblyOptions::default() },
            )
        })
        .unwrap();
    assert!(!cross.is_empty(), "cross-target assembly is generated without installing it");
    // Compiling for real reports the functions to the perf log.
    let sandbox = runtime.sandbox(|_| {}, SandboxOptions::default()).unwrap();
    sandbox.load_template(&runtime, "perf.lua", "local s = 0 for i = 1, 100 do s += i end return s").unwrap();
    assert!(!seen.lock().unwrap().is_empty(), "perf log entries");
    assert!(seen.lock().unwrap().iter().all(|entry| entry.size > 0));
    l3i::native_code::clear_perf_log();
}
