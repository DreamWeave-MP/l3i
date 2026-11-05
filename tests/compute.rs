//! `@dream/compute` through whole JSL invocations. The engine's kernels and planner are tested
//! against independent oracles in `src/compute`.

#[cfg(feature = "compute-testing")]
mod host;

use std::rc::Rc;
use std::sync::Arc;

use l3i::Runtime;
use l3i::compute::script::{ComputeExtension, ComputeLimits, ScriptTensor};
use l3i::compute::{Budget, Domain, Shape, Storage, Tensor};
use l3i::extension::{RuntimePlan, RuntimePolicy};
use l3i::userdata::Owned;

fn plan(extension: ComputeExtension) -> Rc<RuntimePlan> {
    RuntimePlan::builder()
        .policy(RuntimePolicy::new().compat_global("@dream/compute", "compute"))
        .extension(extension)
        .finalize()
        .unwrap()
}

fn limited(memory_bytes: usize, graph_nodes: usize, programs: usize) -> Rc<RuntimePlan> {
    plan(ComputeExtension::new(ComputeLimits { memory_bytes, graph_nodes, programs }))
}

#[test]
fn every_runtime_from_a_plan_gets_its_own_domain() {
    let plan = limited(1024, 64, 8);
    let (first, second) = (Runtime::from_plan(&plan).unwrap(), Runtime::from_plan(&plan).unwrap());
    first
        .exec(
            "held = compute.f32(buffer.create(800), 1, 200); assert(compute.used() == 800 and compute.limit() == 1024)",
        )
        .unwrap();
    second.exec("assert(compute.used() == 0); held = compute.f32(buffer.create(800), 1, 200)").unwrap();
}

#[test]
fn defaults_hold_a_real_workload_and_no_more_than_their_limit() {
    let runtime = Runtime::from_plan(&plan(ComputeExtension::default())).unwrap();
    runtime
        .exec(
            "assert(compute.limit() == 64 * 1024 * 1024)
             local held = {}
             local ok = true
             for _ = 1, 128 do
                 local success, tensor = pcall(compute.f32, buffer.create(1024 * 1024), 1, 256 * 1024)
                 if not success then ok = false break end
                 table.insert(held, tensor)
             end
             assert(not ok and #held == 64)",
        )
        .unwrap();
}

#[test]
fn a_refused_call_collects_garbage_retries_and_otherwise_raises_recoverably() {
    let runtime = Runtime::from_plan(&limited(1024, 64, 8)).unwrap();
    runtime
        .exec(
            "-- Garbage that holds the budget gives way without an error.
             for _ = 1, 64 do compute.f32(buffer.create(800), 1, 200) end
             -- What is really held does not, and asking is not fatal.
             local held = compute.f32(buffer.create(800), 1, 200)
             local ok, message = pcall(compute.f32, buffer.create(800), 1, 200)
             assert(not ok and string.find(message, 'budget', 1, true), message)
             assert(compute.used() == 800)
             held = nil
             local again = compute.f32(buffer.create(800), 1, 200)
             assert(compute.used() == 800)
             -- A program whose result cannot be held refuses the same way, and later runs work.
             local x = compute.slot(0, 1, 200)
             local program = x:relu():compile()
             local ok2 = pcall(function() return program:run(again) end)
             assert(not ok2)
             local out = buffer.create(800)
             program:runTensorsInto(out, again)",
        )
        .unwrap();
}

#[test]
fn graph_limits_are_the_hosts_and_come_back_when_collected() {
    let (budget, domain) = (Budget::new(1024), Domain::new(4, 1));
    let runtime = Runtime::from_plan(&plan(ComputeExtension::shared(budget, domain.clone()))).unwrap();
    runtime
        .exec(
            "a, b = compute.slot(0, 1, 1), compute.slot(1, 1, 1)
             program = a:add(b):compile()
             local ok, message = pcall(function() return a:add(b):compile() end)
             assert(not ok and string.find(message, 'budget', 1, true), message)
             c, d = a:relu(), a:relu()",
        )
        .unwrap();
    assert_eq!((domain.live_nodes(), domain.live_plans()), (4, 1), "held, with garbage collected on demand");
    runtime
        .exec(
            "local ok, message = pcall(function() return a:relu() end)
             assert(not ok and string.find(message, 'budget', 1, true), 'a fifth node')",
        )
        .unwrap();
    runtime.exec("a, b, c, d, program = nil, nil, nil, nil, nil").unwrap();
    runtime.collect_garbage();
    assert_eq!((domain.live_nodes(), domain.live_plans()), (0, 0));
    runtime.exec("local x = compute.slot(0, 1, 1); x:relu():compile()").unwrap();
}

#[test]
fn runtimes_sharing_a_domain_charge_it_together_and_closing_them_returns_everything() {
    let (budget, domain) = (Budget::new(1024), Domain::new(64, 8));
    let plan = plan(ComputeExtension::shared(budget.clone(), domain.clone()));
    let (first, second) = (Runtime::from_plan(&plan).unwrap(), Runtime::from_plan(&plan).unwrap());
    first
        .exec(
            "held = compute.f32(buffer.create(800), 1, 200)
             expression = compute.slot(0, 1, 200):relu()
             program = expression:compile()",
        )
        .unwrap();
    assert_eq!(budget.used(), 800);
    assert!(domain.live_nodes() > 0);
    assert_eq!(domain.live_plans(), 1);
    second
        .exec(
            "assert(compute.used() == 800)
             local ok = pcall(compute.f32, buffer.create(800), 1, 200)
             assert(not ok, 'the domain is shared')",
        )
        .unwrap();
    drop(first);
    assert_eq!(budget.used(), 0, "closing a runtime releases what it held");
    assert_eq!((domain.live_nodes(), domain.live_plans()), (0, 0));
    second.exec("kept = compute.f32(buffer.create(800), 1, 200)").unwrap();
    assert_eq!(budget.used(), 800);
    drop(second);
    assert_eq!(budget.used(), 0);
}

#[test]
fn closing_a_runtime_releases_prepared_weights_views_and_scratchless_results() {
    let budget = Budget::new(1 << 20);
    let plan = plan(ComputeExtension::shared(budget.clone(), Domain::new(64, 8)));
    let runtime = Runtime::from_plan(&plan).unwrap();
    runtime
        .exec(
            "weights = compute.f32(buffer.create(64), 4, 4)
             left, right = weights:prepareLeft(), weights:prepareRight()
             column = weights:view(4, 1, 4, 16, 0)
             product = compute.slot(0, 4, 4):matmul(compute.slot(1, 4, 1)):compile():run(left, column)",
        )
        .unwrap();
    assert!(budget.used() > 64);
    drop(runtime);
    assert_eq!(budget.used(), 0);
}

#[derive(Debug)]
struct HostBytes(Vec<u8>);
impl Storage for HostBytes {
    fn bytes(&self) -> &[u8] {
        &self.0
    }
}

#[test]
fn a_host_owned_tensor_lives_as_long_as_a_script_holds_it_and_never_longer() {
    let budget = Budget::new(1 << 20);
    let plan = plan(ComputeExtension::shared(budget.clone(), Domain::new(64, 8)));
    let runtime = Runtime::from_plan(&plan).unwrap();
    let bytes: Vec<u8> = [1.0f32, 2.0, 3.0, 4.0].iter().flat_map(|v| v.to_le_bytes()).collect();
    let owner = Arc::new(HostBytes(bytes));
    let tensor = Tensor::from_storage(owner.clone(), Shape::Matrix(2, 2), 0, [8, 4]).unwrap();
    runtime.set_global("hosted", &Owned(ScriptTensor::new(tensor, budget.clone()))).unwrap();
    assert_eq!(budget.used(), 0, "the host accounts for its own storage");
    assert_eq!(Arc::strong_count(&owner), 2);
    runtime
        .exec(
            "local sums = compute.slot(0, 2, 2):sumRows():compile():run(hosted):read()
             assert(buffer.readf32(sums, 0) == 3 and buffer.readf32(sums, 4) == 7)
             shared_view = hosted:view(1, 2, 8, 8, 4)",
        )
        .unwrap();
    assert_eq!(Arc::strong_count(&owner), 3);
    drop(runtime);
    assert_eq!(Arc::strong_count(&owner), 1, "closing the runtime lets go of the host's storage");
    assert_eq!(budget.used(), 0);
}

#[test]
fn tensors_and_packed_weights_ignore_later_writes_to_the_buffers_they_came_from() {
    let runtime = Runtime::from_plan(&limited(1 << 20, 64, 8)).unwrap();
    runtime
        .exec(
            "local source = buffer.create(16)
             for i = 0, 3 do buffer.writef32(source, i * 4, i + 1) end
             local weights = compute.f32(source, 2, 2)
             local prepared = weights:prepareLeft()
             local vector = compute.f32(source, 2, 2):view(2, 1, 0, 8, 0)
             local program = compute.slot(0, 2, 2):matmul(compute.slot(1, 2, 1)):compile()
             local before = program:run(prepared, vector):read()
             buffer.fill(source, 0, 0xff)
             local after = program:run(prepared, vector):read()
             assert(buffer.tostring(before) == buffer.tostring(after))
             assert(buffer.readf32(before, 0) == 7, 'first row: 1*1 + 2*3')",
        )
        .unwrap();
}

#[test]
fn malformed_calls_raise_and_write_nothing() {
    let runtime = Runtime::from_plan(&limited(1 << 20, 64, 8)).unwrap();
    runtime
        .exec(
            "local program = compute.slot(0, 1, 2):relu():compile()
             local input, out = buffer.create(8), buffer.create(8)
             buffer.fill(out, 0, 0x55)
             local function untouched() return buffer.readu32(out, 0) == 0x55555555 and buffer.readu32(out, 4) == 0x55555555 end
             assert(not pcall(function() program:runInto(out) end), 'too few inputs')
             assert(not pcall(function() program:runInto(out, input, input) end), 'too many inputs')
             assert(not pcall(function() program:runInto(buffer.create(4), input) end), 'short output')
             assert(not pcall(function() program:runInto(out, buffer.create(4)) end), 'short input')
             assert(not pcall(function() program:runInto(out, out) end), 'output is the input')
             assert(untouched())
             assert(not pcall(compute.f32, buffer.create(4), 1, 2), 'bytes do not fill the shape')
             assert(not pcall(compute.slot, 0.5, 1, 1), 'a slot is a whole number')
             assert(not pcall(compute.slot, 0, -1, 1), 'a shape is not negative')
             local tensor = compute.f32(input, 1, 2)
             assert(not pcall(function() return tensor:view(1, 2, 4, 0, 4) end), 'past its elements')
             assert(not pcall(function() return compute.slot(0, 1, 2):matmul(compute.slot(1, 3, 1)) end), 'inner dimensions differ')
             assert(not pcall(function() return compute.slot(0, 1, 2):rmsNorm(0) end), 'epsilon must be positive')",
        )
        .unwrap();
}

#[test]
fn the_module_is_frozen_and_exposes_only_the_supported_surface() {
    let runtime = Runtime::from_plan(&limited(1 << 20, 64, 8)).unwrap();
    runtime
        .exec(
            "assert(not pcall(function() compute.slot = nil end), 'the module is frozen')
             for _, name in { 'collect', 'clock', 'noop', 'peak', 'allocations', 'setNative' } do
                 assert(compute[name] == nil, name)
             end
             local program = compute.slot(0, 1, 1):compile()
             for _, name in { 'runReference', 'ir', 'native', 'native_source', 'shader_source' } do
                 local ok, value = pcall(function() return (program :: any)[name] end)
                 assert(not ok or value == nil, name)
             end",
        )
        .unwrap();
}

#[cfg(feature = "compute-testing")]
fn run() -> l3i::Result<()> {
    let runtime = host::runtime()?;
    runtime.exec(include_str!("compute/example.jsl"))?;
    runtime.exec(include_str!("compute/slots.jsl"))?;
    runtime.exec(include_str!("compute/rows.jsl"))?;
    #[cfg(feature = "jit")]
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
    runtime.exec("assert(compute.used()==0)")
}

#[cfg(feature = "compute-testing")]
#[test]
fn jsl_programs_and_teardown() {
    run().unwrap();
    run().unwrap();
}

/// The checked-in shader is what the planner emits today, so `jess run` on it tests current code.
#[cfg(feature = "compute-testing")]
#[test]
fn generated_shader_is_current() {
    let runtime = host::runtime().unwrap();
    runtime.exec(include_str!("compute/example.jsl")).unwrap();
    runtime.exec(include_str!("compute/slots.jsl")).unwrap();
    let source: String = runtime.eval("return shared.state.shader_artifact").unwrap();
    // A Windows checkout may have converted the file's line endings.
    assert_eq!(source, include_str!("compute/generated-shader.jsl").replace("\r\n", "\n"));
}

/// Without the code generator, a whole program maps no executable memory at runtime.
#[cfg(all(feature = "compute-testing", not(feature = "jit"), target_os = "linux"))]
#[test]
fn no_jit_maps_no_executable_memory() {
    run().unwrap();
    let maps = std::fs::read_to_string("/proc/self/maps").unwrap();
    let anonymous_executable = maps.lines().filter(|line| {
        let mut fields = line.split_whitespace();
        let permissions = fields.nth(1).unwrap_or("");
        permissions.contains('x') && fields.nth(3).is_none()
    });
    assert_eq!(anonymous_executable.count(), 0, "no-JIT must map no anonymous executable memory:\n{maps}");
}

/// Expressions and programs are tiny to Luau's collector, so nothing else would ever free these.
#[test]
fn building_graphs_in_a_loop_never_runs_out_of_what_it_no_longer_holds() {
    let runtime = Runtime::from_plan(&limited(1 << 20, 16, 2)).unwrap();
    runtime
        .exec(
            "for _ = 1, 2000 do
                 compute.slot(0, 1, 1):relu():add(compute.constant(1)):compile()
             end",
        )
        .unwrap();
}

#[test]
fn run_into_takes_any_number_of_inputs_in_order_and_refuses_gaps() {
    let runtime = Runtime::from_plan(&limited(1 << 20, 64, 8)).unwrap();
    runtime
        .exec(
            "local function one(v) local b = buffer.create(4) buffer.writef32(b, 0, v) return b end
             local sum = compute.slot(0, 1, 1)
             for slot = 1, 4 do sum = sum:mul(compute.constant(10)):add(compute.slot(slot, 1, 1)) end
             local program = sum:compile()
             local out = buffer.create(4)
             program:runInto(out, one(1), one(2), one(3), one(4), one(5))
             assert(buffer.readf32(out, 0) == 12345, 'inputs past the fixed parameters keep their order')
             assert(not pcall(function() program:runInto(out, one(1), nil, one(3), one(4), one(5)) end), 'a gap')
             assert(not pcall(function() program:runInto(out, nil, one(1), one(3), one(4), one(5)) end), 'a leading gap')
             local six = out
             assert(not pcall(function() program:runInto(six, one(1), one(2), one(3), one(4), six) end), 'aliased past the fixed parameters')
             assert(not pcall(function() program:runInto(out, one(1), one(2), one(3), one(4)) end), 'too few')
             assert(not pcall(function() program:runInto(out, one(1), one(2), one(3), one(4), one(5), one(6)) end), 'too many')",
        )
        .unwrap();
}
