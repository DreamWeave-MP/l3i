use l3i::source::LoadScope;
use l3i::{
    Result, Runtime,
    convert::{BufferView, Exact, NewBuffer},
    module::{LuauModule, ModuleBuilder},
    userdata::{Owned, Userdata},
};
use l3i_graph_cpu_research::{Bindings, Budget, Domain, Executable, Expr, Shape, Tensor};
use std::cell::RefCell;
struct TensorValue(Tensor);
struct Expression(Expr);
struct Program(Executable);
struct Bound(RefCell<Bindings>);
// SAFETY: bindings retain native immutable owners only; no Lua access in destruction.
unsafe impl Userdata for Bound {
    const NAME: &'static str = "dream.research.Bindings";
}
// SAFETY: these payloads own native immutable storage and never access Lua in destructors.
unsafe impl Userdata for TensorValue {
    const NAME: &'static str = "dream.research.Tensor";
}
// SAFETY: the graph retains only immutable native nodes and owners, never Lua references.
unsafe impl Userdata for Expression {
    const NAME: &'static str = "dream.research.Expression";
}
// SAFETY: the executable owns immutable native instructions and storage, with no Lua references.
unsafe impl Userdata for Program {
    const NAME: &'static str = "dream.research.Program";
}
struct Research;
impl LuauModule for Research {
    const NAME: &'static str = "dream.research.compute";
    fn register(_runtime: &Runtime, module: &mut ModuleBuilder<'_>) -> Result<()> {
        let service_budget = Budget::new(64 * 1024 * 1024);
        let service_domain = Domain::new(1024, 32);
        module.userdata::<TensorValue>(None, |ty| {
            let domain = service_domain.clone();
            ty.method("expression", move |t: &TensorValue| {
                Expr::input_in(t.0.clone(), domain.clone()).map(|e| Owned(Expression(e)))
            })?;
            ty.method("read", |t: &TensorValue| t.0.readback().map(NewBuffer))?;
            ty.method("shares", |a: &TensorValue, b: &TensorValue| a.0.shares_storage(&b.0))?;
            ty.method(
                "view",
                |t: &TensorValue,
                 r: Exact<usize>,
                 c: Exact<usize>,
                 offset: Exact<usize>,
                 rs: Exact<usize>,
                 cs: Exact<usize>| {
                    t.0.view(Shape::Matrix(r.0, c.0), offset.0, [rs.0, cs.0]).map(|t| Owned(TensorValue(t)))
                },
            )
        })?;
        module.userdata::<Expression>(None, |ty| {
            ty.method("add", |a: &Expression, b: &Expression| a.0.add(&b.0).map(|e| Owned(Expression(e))))?;
            ty.method("mul", |a: &Expression, b: &Expression| a.0.mul(&b.0).map(|e| Owned(Expression(e))))?;
            ty.method("relu", |a: &Expression| a.0.relu().map(|e| Owned(Expression(e))))?;
            ty.method("matmul", |a: &Expression, b: &Expression| a.0.matmul(&b.0).map(|e| Owned(Expression(e))))?;
            ty.method("compile", |a: &Expression| a.0.compile().map(|e| Owned(Program(e))))
        })?;
        module.userdata::<Bound>(None, |ty| {
            ty.method("set", |b: &Bound, slot: Exact<usize>, t: &TensorValue| b.0.borrow_mut().set(slot.0, t.0.clone()))
        })?;
        module.function("bindings", || Owned(Bound(RefCell::new(Bindings::default()))))?;
        let domain = service_domain.clone();
        module.function("slot", move |slot: Exact<usize>, rows: Exact<usize>, cols: Exact<usize>| {
            Expr::slot_in(slot.0, Shape::Matrix(rows.0, cols.0), domain.clone()).map(|e| Owned(Expression(e)))
        })?;
        module.userdata::<Program>(None, |ty| {
            let budget = service_budget.clone();
            ty.method("run_bound", move |p: &Program, b: &Bound| {
                p.0.run_bound(&b.0.borrow(), &budget).map(|t| Owned(TensorValue(t)))
            })?;
            ty.method("native_source", |p: &Program| p.0.native_source())?;
            ty.method("shader_source", |p: &Program, mode: String| p.0.shader_source(&mode))?;
            ty.method("native", |p: &Program, call: &l3i::bind::Call<'_>| -> Result<l3i::value::Function> {
                let generator = l3i::native_code::NativeCodeGen::for_scope(call)
                    .ok_or_else(|| l3i::Error::runtime("native backend unavailable"))?;
                if !generator.is_available() {
                    return Err(l3i::Error::runtime("native backend unavailable"));
                }
                let chunk = call.load_source(
                    "=tensor-native",
                    &p.0.native_source()?,
                    &l3i::source::CompileOptions { type_info_level: 1, ..Default::default() },
                )?;
                chunk.invoke::<l3i::value::Function, _>(call, ())
            })?;

            let budget = service_budget.clone();
            ty.method("run", move |p: &Program| p.0.run(&budget).map(|t| Owned(TensorValue(t))))?;
            ty.method("ir", |p: &Program| p.0.dump())
        })?;
        let budget = service_budget.clone();
        module.function("f32", move |bytes: BufferView, r: Exact<usize>, c: Exact<usize>| {
            Tensor::import_view(&bytes, Shape::Matrix(r.0, c.0), &budget).map(|t| Owned(TensorValue(t)))
        })?;
        module.function("constant", move |v: f32| {
            Expr::constant_in(v, service_domain.clone()).map(|e| Owned(Expression(e)))
        })?;
        let budget = service_budget.clone();
        module.function("used", move || budget.used())?;
        module.function("allocations", move || service_budget.allocations())?;
        module.function("noop", || ())?;

        let start = std::time::Instant::now();
        module.function("clock", move || start.elapsed().as_secs_f64())?;
        Ok(())
    }
}
fn run() -> Result<()> {
    let plan = l3i::extension::RuntimePlan::builder()
        .policy(
            l3i::extension::RuntimePolicy::new()
                .compat_global("@dream/data", "data")
                .native_code(l3i::extension::NativeCodePolicy { record_counters: cfg!(test), ..Default::default() }),
        )
        .extension(l3i::data::DataExtension)
        .finalize()?;
    let runtime = Runtime::from_plan(&plan)?;
    let module = runtime.register_module::<Research>()?;
    runtime.set_global("compute", &module)?;
    runtime.exec(include_str!("../example.jsl"))?;
    runtime.exec(include_str!("../slots.jsl"))?;
    if cfg!(test) {
        assert!(runtime.native_code().unwrap().execution_stats(&runtime.stack()).regular_blocks_executed > 0);
    }
    runtime.collect_garbage();
    runtime.exec("local b = retained:run():read(); assert(buffer.readf32(b, 20) == 26); retained = nil")?;
    runtime.collect_garbage();
    runtime.exec("assert(compute.used()==0)")?;

    if std::env::args().any(|arg| arg == "--bench") {
        runtime.exec(include_str!("../bench.jsl"))?;
        runtime.collect_garbage();
        runtime.exec("assert(compute.used()==0)")?;
        native_benchmark();
        runtime.exec(include_str!("../native-bench.jsl"))?;
    }
    if let Some(path) = std::env::args().skip_while(|arg| arg != "--emit-shader").nth(1) {
        let source: String = runtime.eval("return shader_artifact")?;
        std::fs::write(path, source).map_err(|e| l3i::Error::runtime(e.to_string()))?;
    }
    Ok(())
}
fn main() -> Result<()> {
    run()
}
#[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation, clippy::cast_possible_wrap, clippy::float_cmp)] // Small exact integer fixtures and iteration count.
fn native_benchmark() {
    use std::{hint::black_box, time::Instant};
    for n in [0, 1, 1024, 65536] {
        let a: Vec<f32> = (0..n).map(|i| (i % 101) as f32 - 50.0).collect();
        let b: Vec<f32> = (0..n).map(|i| (i % 13) as f32 - 6.0).collect();
        let mut output = vec![0.0f32; n];
        let start = Instant::now();
        for _ in 0..20 {
            for ((a, b), out) in black_box(&a).iter().zip(black_box(&b)).zip(black_box(&mut output)) {
                let sum = *a + *b;
                let value = sum + 2.0;
                *out = if value <= 0.0 { 0.0 } else { value };
            }
            black_box(&output);
        }
        println!("native_n={n} preallocated_us={:.3}", start.elapsed().as_secs_f64() * 1e6 / 20.0);
        for (i, value) in output.iter().enumerate() {
            let expected = (((i % 101) as i32 - 50) + ((i % 13) as i32 - 6) + 2).max(0);
            assert_eq!(*value, expected as f32);
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn jsl_graph_and_teardown() {
        super::run().unwrap();
        super::run().unwrap();
    }
}
