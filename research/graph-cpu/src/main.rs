//! The JSL host: L3i's module machinery around the research library, plus the benchmarks.
mod bench;

use l3i::source::LoadScope;
use l3i::{
    Result, Runtime,
    bind::{Call, VarArgs},
    convert::{BufferView, Exact, NewBuffer},
    module::{LuauModule, ModuleBuilder},
    stack::Scope,
    userdata::{Owned, Userdata},
};
use l3i_graph_cpu_research::{Budget, Domain, Expr, Numeric, Program, Shape, Tensor, View};

struct TensorValue(Tensor);
struct Expression(Expr);
struct CompiledProgram(Program);
// SAFETY: these payloads own native immutable storage and never access Lua in destructors.
unsafe impl Userdata for TensorValue {
    const NAME: &'static str = "dream.research.Tensor";
}
// SAFETY: as above.
unsafe impl Userdata for Expression {
    const NAME: &'static str = "dream.research.Expression";
}
// SAFETY: as above.
unsafe impl Userdata for CompiledProgram {
    const NAME: &'static str = "dream.research.Program";
}

unsafe extern "C" {
    fn lua_gc(state: *mut std::ffi::c_void, what: std::ffi::c_int, data: std::ffi::c_int) -> std::ffi::c_int;
}
/// `LUA_GCCOLLECT`. L3i has this collection inside `Call`, private and behind its `tcp` feature;
/// production would make that public instead of declaring the symbol.
const LUA_GCCOLLECT: std::ffi::c_int = 2;
fn collect(call: &Call<'_>) {
    // SAFETY: the state is live for the call, a full collection is permitted from a C function,
    // and no userdata destructor here calls the Lua API.
    unsafe { lua_gc(Scope::state(call).cast(), LUA_GCCOLLECT, 0) };
}
/// Native tensors look small to Luau's collector, so results can be garbage that holds the budget
/// without ever triggering a cycle. When the budget refuses `body`, collect once and retry.
fn collecting<R>(call: &Call<'_>, budget: &Budget, mut body: impl FnMut() -> Result<R>) -> Result<R> {
    let denials = budget.denials();
    let first = body();
    if first.is_err() && budget.denials() != denials {
        collect(call);
        return body();
    }
    first
}

/// Whether two byte ranges share a byte. An empty range shares none. A range's end never overflows,
/// because a slice cannot extend past the end of the address space.
fn overlaps(a: &[u8], b: &[u8]) -> bool {
    let (a, b) = (a.as_ptr_range(), b.as_ptr_range());
    a.start != a.end && b.start != b.end && a.start < b.end && b.start < a.end
}

/// `program:runInto(out, a, b, ...)`: Luau buffers, borrowed for this call and never retained.
fn run_into(
    program: &Program,
    out: &mut BufferView<'_>,
    inputs: &[BufferView<'_>],
    budget: &std::sync::Arc<Budget>,
) -> Result<()> {
    if inputs.len() != program.signature().len() {
        return Err(l3i::Error::runtime("input slot set mismatch"));
    }
    // SAFETY: no Lua runs and no other view writes between here and the end of `run_into` below.
    // The exclusive slice is only made after every input's byte range is shown disjoint from it,
    // so this holds for any `BufferView`, including sub-ranges of one buffer. Inputs only read, so
    // they may overlap each other.
    let (input_bytes, out_bytes) = unsafe {
        let target = out.bytes_unchecked();
        if inputs.iter().any(|input| overlaps(input.bytes_unchecked(), target)) {
            return Err(l3i::Error::runtime("output buffer aliases an input"));
        }
        let reads: Vec<&[u8]> = inputs.iter().map(|input| input.bytes_unchecked()).collect();
        (reads, out.bytes_mut_unchecked())
    };
    let views = input_bytes
        .iter()
        .zip(program.signature())
        .map(|(bytes, shape)| View::contiguous(bytes, *shape))
        .collect::<Result<Vec<View<'_>>>>()?;
    program.run_into(&views, out_bytes, budget)
}

struct Research;
impl LuauModule for Research {
    const NAME: &'static str = "dream.research.compute";
    fn register(_runtime: &Runtime, module: &mut ModuleBuilder<'_>) -> Result<()> {
        let service_budget = Budget::new(64 * 1024 * 1024);
        let service_domain = Domain::new(4096, 512);
        module.userdata::<TensorValue>(None, |ty| {
            ty.method("read", |t: &TensorValue| t.0.readback().map(NewBuffer))?;
            ty.method("shares", |a: &TensorValue, b: &TensorValue| a.0.shares_storage(&b.0))?;
            ty.method(
                "view",
                |t: &TensorValue,
                 rows: Exact<usize>,
                 cols: Exact<usize>,
                 offset: Exact<usize>,
                 row_stride: Exact<usize>,
                 col_stride: Exact<usize>| {
                    t.0.view(Shape::Matrix(rows.0, cols.0), offset.0, [row_stride.0, col_stride.0])
                        .map(|v| Owned(TensorValue(v)))
                },
            )
        })?;
        module.userdata::<Expression>(None, |ty| {
            ty.method("add", |a: &Expression, b: &Expression| a.0.add(&b.0).map(|e| Owned(Expression(e))))?;
            ty.method("mul", |a: &Expression, b: &Expression| a.0.mul(&b.0).map(|e| Owned(Expression(e))))?;
            ty.method("relu", |a: &Expression| a.0.relu().map(|e| Owned(Expression(e))))?;
            ty.method("softmax", |a: &Expression| a.0.softmax().map(|e| Owned(Expression(e))))?;
            ty.method("sumRows", |a: &Expression| a.0.sum_rows().map(|e| Owned(Expression(e))))?;
            ty.method("matmul", |a: &Expression, b: &Expression| a.0.matmul(&b.0).map(|e| Owned(Expression(e))))?;
            ty.method("compile", |a: &Expression| a.0.compile().map(|e| Owned(CompiledProgram(e))))?;
            ty.method("compileFast", |a: &Expression| {
                a.0.compile_with(Numeric::Fast).map(|e| Owned(CompiledProgram(e)))
            })
        })?;
        module.userdata::<CompiledProgram>(None, |ty| {
            let budget = service_budget.clone();
            ty.method("run", move |p: &CompiledProgram, call: &Call<'_>, tensors: VarArgs<&TensorValue>| {
                let tensors: Vec<&Tensor> = tensors.values.iter().map(|t| &t.0).collect();
                collecting(call, &budget, || p.0.run(&tensors, &budget)).map(|t| Owned(TensorValue(t)))
            })?;
            let budget = service_budget.clone();
            ty.method(
                "runInto",
                move |p: &CompiledProgram,
                      call: &Call<'_>,
                      mut out: BufferView<'_>,
                      inputs: VarArgs<BufferView<'_>>| {
                    collecting(call, &budget, || run_into(&p.0, &mut out, &inputs.values, &budget))
                },
            )?;
            let budget = service_budget.clone();
            ty.method("runReference", move |p: &CompiledProgram, call: &Call<'_>, tensors: VarArgs<&TensorValue>| {
                let views: Vec<View<'_>> = tensors.values.iter().map(|t| t.0.borrow()).collect();
                collecting(call, &budget, || p.0.run_reference(&views, &budget)).map(|t| Owned(TensorValue(t)))
            })?;
            ty.method("native_source", |p: &CompiledProgram| p.0.native_source())?;
            ty.method("shader_source", |p: &CompiledProgram, mode: String| p.0.shader_source(&mode))?;
            ty.method("native", |p: &CompiledProgram, call: &Call<'_>| -> Result<l3i::value::Function> {
                // Without the generator the same source runs interpreted: slower, identical results.
                #[cfg(feature = "luau-jit")]
                if !l3i::native_code::NativeCodeGen::for_scope(call)
                    .is_some_and(l3i::native_code::NativeCodeGen::is_available)
                {
                    return Err(l3i::Error::runtime("native backend unavailable"));
                }
                let chunk = call.load_source(
                    "=tensor-native",
                    &p.0.native_source()?,
                    &l3i::source::CompileOptions { type_info_level: 1, ..Default::default() },
                )?;
                chunk.invoke::<l3i::value::Function, _>(call, ())
            })?;
            ty.method("ir", |p: &CompiledProgram| p.0.dump())
        })?;
        let domain = service_domain.clone();
        module.function("slot", move |slot: Exact<usize>, rows: Exact<usize>, cols: Exact<usize>| {
            Expr::slot(slot.0, Shape::Matrix(rows.0, cols.0), domain.clone()).map(|e| Owned(Expression(e)))
        })?;
        let budget = service_budget.clone();
        module.function("f32", move |call: &Call<'_>, bytes: BufferView, r: Exact<usize>, c: Exact<usize>| {
            collecting(call, &budget, || Tensor::import_view(&bytes, Shape::Matrix(r.0, c.0), &budget))
                .map(|t| Owned(TensorValue(t)))
        })?;
        module.function("collect", |call: &Call<'_>| collect(call))?;
        #[cfg(feature = "luau-jit")]
        module.function("setNative", |call: &Call<'_>, enabled: bool| {
            if let Some(generator) = l3i::native_code::NativeCodeGen::for_scope(call) {
                generator.set_native_execution_enabled(call, enabled);
            }
        })?;
        #[cfg(not(feature = "luau-jit"))]
        module.function("setNative", |_enabled: bool| ())?;
        module.function("constant", move |v: f32| {
            Expr::constant(v, service_domain.clone()).map(|e| Owned(Expression(e)))
        })?;
        let budget = service_budget.clone();
        module.function("used", move || budget.used())?;
        let budget = service_budget.clone();
        module.function("peak", move || budget.peak())?;
        module.function("allocations", move || service_budget.allocations())?;
        module.function("noop", || ())?;

        let start = std::time::Instant::now();
        module.function("clock", move || start.elapsed().as_secs_f64())?;
        Ok(())
    }
}

fn runtime() -> Result<Runtime> {
    let policy = l3i::extension::RuntimePolicy::new().compat_global("@dream/data", "data");
    #[cfg(feature = "luau-jit")]
    let policy =
        policy.native_code(l3i::extension::NativeCodePolicy { record_counters: cfg!(test), ..Default::default() });
    let plan = l3i::extension::RuntimePlan::builder().policy(policy).extension(l3i::data::DataExtension).finalize()?;
    let runtime = Runtime::from_plan(&plan)?;
    let module = runtime.register_module::<Research>()?;
    runtime.set_global("compute", &module)?;
    let kernels = runtime.stack().load_source(
        "=luau-kernels",
        include_str!("../luau-kernels.jsl"),
        &l3i::source::CompileOptions { type_info_level: 1, ..Default::default() },
    )?;
    let kernels: l3i::value::Value = kernels.invoke(&runtime.stack(), ())?;
    runtime.set_global("kernels", &kernels)?;
    runtime.exec("shared = { state = {} }")?;
    // Luau's builtin fast paths and JIT guards require a safe environment. Without this, every
    // `buffer.readf32` is a table lookup and a full call, with or without native code.
    runtime.sandbox_globals()?;
    Ok(runtime)
}

fn run() -> Result<()> {
    let runtime = runtime()?;
    runtime.exec(include_str!("../example.jsl"))?;
    runtime.exec(include_str!("../slots.jsl"))?;
    #[cfg(all(test, feature = "luau-jit"))]
    assert!(runtime.native_code().unwrap().execution_stats(&runtime.stack()).regular_blocks_executed > 0);
    runtime.collect_garbage();
    // A program survives collection of everything but itself, and runs on new inputs.
    runtime.exec(
        "local t = compute.f32(shared.state.retained_input, 2, 3)
         local b = shared.state.retained:run(t):read()
         assert(buffer.readf32(b, 0) == 3 and buffer.readf32(b, 20) == 8)
         shared.state.retained, shared.state.retained_input, t = nil, nil, nil",
    )?;
    runtime.collect_garbage();
    runtime.exec("assert(compute.used()==0)")?;

    let arguments: Vec<String> = std::env::args().collect();
    if arguments.iter().any(|arg| arg == "--bench-rows") {
        bench::reductions();
        runtime.exec(include_str!("../bench-rows.jsl"))?;
    }
    if arguments.iter().any(|arg| arg == "--bench") {
        bench::native();
        runtime.exec(include_str!("../bench.jsl"))?;
        runtime.collect_garbage();
        runtime.exec("assert(compute.used()==0)")?;
    }
    #[cfg(feature = "luau-jit")]
    if let Some(path) = arguments.iter().skip_while(|arg| *arg != "--emit-luau-assembly").nth(1) {
        let chunk = runtime.stack().load_source(
            "=luau-kernels",
            include_str!("../luau-kernels.jsl"),
            &l3i::source::CompileOptions { type_info_level: 1, ..Default::default() },
        )?;
        let generator = runtime.native_code().ok_or_else(|| l3i::Error::runtime("native backend unavailable"))?;
        let text = runtime.stack().with_frame(|frame| {
            let function = frame.push(&chunk)?;
            let options = l3i::native_code::AssemblyOptions { include_ir: true, ..Default::default() };
            generator.assembly(frame, function.index(), options)
        })?;
        std::fs::write(path, text).map_err(|e| l3i::Error::runtime(e.to_string()))?;
    }
    if let Some(path) = arguments.iter().skip_while(|arg| *arg != "--emit-shader").nth(1) {
        let source: String = runtime.eval("return shared.state.shader_artifact")?;
        std::fs::write(path, source).map_err(|e| l3i::Error::runtime(e.to_string()))?;
    }
    Ok(())
}

fn main() -> Result<()> {
    run()
}

#[cfg(test)]
mod tests {
    /// Sub-ranges with different starts still count: start-address equality would miss these.
    #[test]
    fn overlap_is_a_range_check() {
        let bytes = [0u8; 16];
        assert!(super::overlaps(&bytes[0..12], &bytes[4..8]), "contained, different start");
        assert!(super::overlaps(&bytes[0..8], &bytes[4..12]), "partial");
        assert!(super::overlaps(&bytes[4..12], &bytes[0..8]), "partial, other order");
        assert!(super::overlaps(&bytes, &bytes), "identical");
        assert!(!super::overlaps(&bytes[0..8], &bytes[8..16]), "adjacent");
        assert!(!super::overlaps(&bytes[8..16], &bytes[0..8]), "adjacent, other order");
        assert!(!super::overlaps(&bytes[4..4], &bytes), "empty");
        assert!(!super::overlaps(&bytes, &bytes[4..4]), "empty, other side");
    }

    #[test]
    fn jsl_programs_and_teardown() {
        super::run().unwrap();
        super::run().unwrap();
        // Without the feature, everything above ran with no executable memory mapped at runtime.
        #[cfg(all(not(feature = "luau-jit"), target_os = "linux"))]
        {
            let maps = std::fs::read_to_string("/proc/self/maps").unwrap();
            let anonymous_executable = maps.lines().filter(|line| {
                let mut fields = line.split_whitespace();
                let permissions = fields.nth(1).unwrap_or("");
                permissions.contains('x') && fields.nth(3).is_none()
            });
            assert_eq!(anonymous_executable.count(), 0, "no-JIT must map no anonymous executable memory:\n{maps}");
        }
    }
}
