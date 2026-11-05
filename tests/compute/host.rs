//! The runtime the compute tests and benches share: the production `@dream/compute` extension, plus
//! an extension of the oracles and instruments only they use, none of which a real runtime has.

use std::sync::Arc;

use l3i::bind::{Call, VarArgs};
use l3i::compute::script::{ComputeExtension, ScriptProgram, ScriptTensor};
use l3i::compute::{Budget, Domain, View};
use l3i::convert::BufferView;
use l3i::extension::{Extension, ExtensionDescriptor, InstallContext, RuntimePlan, RuntimePolicy};
use l3i::source::{CompileOptions, LoadScope};
use l3i::userdata::{Owned, Userdata};
use l3i::{Error, Result, Runtime};

const TESTING: &str = "@compute/testing";

struct ComputeTesting {
    budget: Arc<Budget>,
}

impl Extension for ComputeTesting {
    fn id(&self) -> &'static str {
        "compute.testing"
    }

    fn describe(&self, d: &mut ExtensionDescriptor) -> Result<()> {
        d.requires("dream.compute");
        let mut tensor = d.augment_userdata::<ScriptTensor>(ScriptTensor::NAME);
        tensor.method("shares", |a: &ScriptTensor, b: &ScriptTensor| a.tensor().shares_storage(b.tensor())).untyped();
        let mut program = d.augment_userdata::<ScriptProgram>(ScriptProgram::NAME);
        program
            .method("runReference", |p: &ScriptProgram, call: &Call<'_>, tensors: VarArgs<&ScriptTensor>| {
                let views: Vec<View<'_>> = tensors.values.iter().map(|t| t.tensor().borrow()).collect();
                let budget = p.budget();
                let denials = budget.denials();
                let mut result = p.program().run_reference(&views, budget);
                if result.is_err() && budget.denials() != denials {
                    call.collect_garbage();
                    result = p.program().run_reference(&views, budget);
                }
                Ok::<_, Error>(Owned(ScriptTensor::new(result?, budget.clone())))
            })
            .untyped();
        program.method("native_source", |p: &ScriptProgram| p.program().native_source()).untyped();
        program.method("shader_source", |p: &ScriptProgram, mode: String| p.program().shader_source(&mode)).untyped();
        program.method("ir", |p: &ScriptProgram| p.program().dump()).untyped();
        program.method("probe0", |_: &ScriptProgram| ()).untyped();
        program.method("probe1", |_: &ScriptProgram, _: BufferView<'_>| ()).untyped();
        program.method("probeCall", |_: &ScriptProgram, _: &Call<'_>, _: BufferView<'_>| ()).untyped();
        program
            .method(
                "probeOptional",
                |_: &ScriptProgram,
                 _: BufferView<'_>,
                 _: Option<BufferView<'_>>,
                 _: Option<BufferView<'_>>,
                 _: Option<BufferView<'_>>| (),
            )
            .untyped();
        program
            .method(
                "probeRest",
                |_: &ScriptProgram,
                 _: BufferView<'_>,
                 _: Option<BufferView<'_>>,
                 _: Option<BufferView<'_>>,
                 _: Option<BufferView<'_>>,
                 _: VarArgs<BufferView<'_>>| (),
            )
            .untyped();
        program
            .method("native", |p: &ScriptProgram, call: &Call<'_>| -> Result<l3i::value::Function> {
                // Without the generator the same source runs interpreted: slower, identical results.
                #[cfg(feature = "jit")]
                if !l3i::native_code::NativeCodeGen::for_scope(call)
                    .is_some_and(l3i::native_code::NativeCodeGen::is_available)
                {
                    return Err(Error::runtime("native backend unavailable"));
                }
                let chunk = call.load_source(
                    "=tensor-native",
                    &p.program().native_source()?,
                    &CompileOptions { type_info_level: 1, ..Default::default() },
                )?;
                chunk.invoke::<l3i::value::Function, _>(call, ())
            })
            .untyped();
        let module = d.module(TESTING);
        module.installed("peak").untyped();
        module.installed("allocations").untyped();
        module.installed("clock").untyped();
        module.function("collect", |call: &Call<'_>| call.collect_garbage()).untyped();
        module.function("noop", || ()).untyped();
        #[cfg(feature = "jit")]
        module
            .function("setNative", |call: &Call<'_>, enabled: bool| {
                if let Some(generator) = l3i::native_code::NativeCodeGen::for_scope(call) {
                    generator.set_native_execution_enabled(call, enabled);
                }
            })
            .untyped();
        #[cfg(not(feature = "jit"))]
        module.function("setNative", |_enabled: bool| ()).untyped();
        Ok(())
    }

    fn install(&self, context: &mut InstallContext<'_>) -> Result<()> {
        let module = context.module(TESTING)?;
        let budget = self.budget.clone();
        module.function("peak", move || budget.peak())?;
        let budget = self.budget.clone();
        module.function("allocations", move || budget.allocations())?;
        let start = std::time::Instant::now();
        module.function("clock", move || start.elapsed().as_secs_f64())?;
        Ok(())
    }
}

pub(super) fn runtime() -> Result<Runtime> {
    let policy = RuntimePolicy::new()
        .compat_global("@dream/data", "data")
        .compat_global("@dream/compute", "compute")
        .compat_global(TESTING, "testing");
    #[cfg(feature = "jit")]
    let policy =
        policy.native_code(l3i::extension::NativeCodePolicy { record_counters: cfg!(test), ..Default::default() });
    let budget = Budget::new(64 * 1024 * 1024);
    let plan = RuntimePlan::builder()
        .policy(policy)
        .extension(l3i::data::DataExtension)
        .extension(ComputeExtension::shared(budget.clone(), Domain::new(4096, 512)))
        .extension(ComputeTesting { budget })
        .finalize()?;
    let runtime = Runtime::from_plan(&plan)?;
    let kernels = runtime.stack().load_source(
        "=luau-kernels",
        include_str!("luau-kernels.jsl"),
        &CompileOptions { type_info_level: 1, ..Default::default() },
    )?;
    let kernels: l3i::value::Value = kernels.invoke(&runtime.stack(), ())?;
    runtime.set_global("kernels", &kernels)?;
    runtime.exec("shared = { state = {} }")?;
    // Luau's builtin fast paths and JIT guards require a safe environment. Without this, every
    // `buffer.readf32` is a table lookup and a full call, with or without native code.
    runtime.sandbox_globals()?;
    Ok(runtime)
}
