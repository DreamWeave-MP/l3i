//! `@dream/compute` through whole JSL invocations. The engine's kernels and planner are tested
//! against independent oracles in `src/compute`.

#[cfg(feature = "compute-testing")]
mod host;

use std::rc::Rc;
use std::sync::Arc;

use l3i::Runtime;
use l3i::compute::script::{ComputeExtension, ComputeLimits, ScriptTensor};
use l3i::compute::{Budget, Domain, Executor, Limits, Shape, Storage, Tensor};
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
    plan(ComputeExtension::new(ComputeLimits { memory_bytes, graph_nodes, programs, ..ComputeLimits::default() }))
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

/// Refused for real, refused by garbage that gives way, and every kind of stage: `out` holds what it
/// held until a call succeeds. The scratch sizes are chosen so the output itself would have fitted.
#[test]
fn a_call_that_raises_leaves_its_output_as_it_was() {
    let runtime = Runtime::from_plan(&limited(16 * 16 * 4 * 3, 64, 8)).unwrap();
    runtime
        .exec(
            "local size = 16 * 16 * 4
             local a, b = buffer.create(size), buffer.create(size)
             for i = 0, 255 do buffer.writef32(a, i * 4, (i % 7) * 0.5) buffer.writef32(b, i * 4, (i % 5) * 0.25) end
             local x, y = compute.slot(0, 16, 16), compute.slot(1, 16, 16)
             local programs = {
                 x:matmul(y):compile(), x:matmul(y):compileFast(),
                 x:matmul(y):add(x):relu():compile(), x:matmul(y):matmul(y):softmax():compile(),
             }
             local out = buffer.create(size)
             local function untouched()
                 for i = 0, size - 1 do if buffer.readu8(out, i) ~= 0x5a then return false end end
                 return true
             end
             local stages = x:add(y):relu():transpose():softmax():compile()
             table.insert(programs, stages)
             for index, program in programs do
                 buffer.fill(out, 0, 0x5a)
                 -- Everything live, so that garbage collection has nothing to give back.
                 local held = {}
                 repeat
                     local ok, tensor = pcall(compute.f32, buffer.create(64), 4, 4)
                     if ok then table.insert(held, tensor) end
                 until not ok
                 local ok, message = pcall(function() program:runInto(out, a, b) end)
                 assert(index == 5 or not ok, 'program ' .. index .. ' should need more than is left')
                 if not ok then
                     assert(string.find(message, 'budget', 1, true), message)
                     assert(untouched(), 'program ' .. index .. ' wrote before raising')
                 end
                 -- The same call with only garbage in the way succeeds, and retries from clean.
                 held = nil
                 buffer.fill(out, 0, 0x5a)
                 for _ = 1, 8 do compute.f32(buffer.create(size), 16, 16) end
                 program:runInto(out, a, b)
                 assert(not untouched(), 'program ' .. index .. ' wrote nothing')
             end",
        )
        .unwrap();
}

/// Each way a call can name the same bytes twice, in each position an input can take.
#[test]
fn inputs_may_share_bytes_with_each_other_and_never_with_the_output() {
    let runtime = Runtime::from_plan(&limited(1 << 20, 64, 8)).unwrap();
    runtime
        .exec(
            "local function one(v) local b = buffer.create(4) buffer.writef32(b, 0, v) return b end
             local function sum(count)
                 local total = compute.slot(0, 1, 1)
                 for slot = 1, count - 1 do total = total:add(compute.slot(slot, 1, 1)) end
                 return total:compile()
             end
             local out = buffer.create(4)
             local shared = one(2)
             sum(2):runInto(out, shared, shared)
             assert(buffer.readf32(out, 0) == 4, 'twice in the fixed parameters')
             sum(5):runInto(out, shared, shared, shared, shared, shared)
             assert(buffer.readf32(out, 0) == 10, 'past them too')
             for count = 1, 5 do
                 for position = 1, count do
                     local inputs = {}
                     for slot = 1, count do inputs[slot] = if slot == position then out else one(1) end
                     buffer.writef32(out, 0, 99)
                     local program = sum(count)
                     assert(not pcall(function() program:runInto(out, table.unpack(inputs)) end),
                         'out as input ' .. position .. ' of ' .. count)
                     assert(buffer.readf32(out, 0) == 99, 'wrote while refusing')
                 end
             end
             -- No bytes means nothing to share: one empty buffer may be every operand.
             local none = buffer.create(0)
             compute.slot(0, 0, 3):relu():compile():runInto(none, none)
             compute.slot(0, 3, 0):add(compute.slot(1, 3, 0)):compile():runInto(none, none, none)
             -- An empty product of nonempty operands reads neither, and still can't alias them.
             local operand = buffer.create(12)
             local empty = compute.slot(0, 0, 3):matmul(compute.slot(1, 3, 2)):compile()
             empty:runInto(none, none, buffer.create(24))
             assert(not pcall(function() empty:runInto(none, operand, buffer.create(24)) end), 'wrong input size')
             -- Tensors cannot be the output's bytes, and buffers they came from are not them.
             local source = one(5)
             local tensor = compute.f32(source, 1, 1)
             local relu = compute.slot(0, 1, 1):relu():compile()
             relu:runTensorsInto(source, tensor)
             assert(buffer.readf32(source, 0) == 5, 'a tensor owns a copy')",
        )
        .unwrap();
}

/// Workers a host lends reach large runs and leave small ones alone, and the answer doesn't change.
#[test]
fn a_host_can_lend_workers_to_what_scripts_run() {
    #[derive(Debug, Default)]
    struct Counting(std::sync::atomic::AtomicUsize);
    impl Executor for Counting {
        fn workers(&self) -> usize {
            4
        }
        fn run(&self, tasks: usize, task: &(dyn Fn(usize) + Sync)) {
            self.0.fetch_add(tasks, std::sync::atomic::Ordering::Relaxed);
            std::thread::scope(|scope| {
                for index in 0..tasks {
                    scope.spawn(move || task(index));
                }
            });
        }
    }
    let script = "local n = 256
         local bytes = buffer.create(n * n * 4)
         for i = 0, n * n - 1 do buffer.writef32(bytes, i * 4, (i % 17) * 0.25) end
         local x, y = compute.slot(0, n, n), compute.slot(1, n, n)
         local out = buffer.create(n * n * 4)
         x:matmul(y):compile():runInto(out, bytes, bytes)
         local tensor = compute.f32(bytes, n, n)
         local out2 = buffer.create(n * n * 4)
         x:matmul(y):compileFast():runTensorsInto(out2, tensor, tensor)
         local result = x:matmul(y):compile():run(tensor, tensor):read()
         assert(buffer.readf32(result, 0) == buffer.readf32(out, 0), 'the same from a tensor')
         local small = compute.slot(0, 2, 2):relu():compile()
         small:runInto(buffer.create(16), buffer.create(16))
         return buffer.readf32(out, 0) + buffer.readf32(out, n * n * 4 - 4)";
    let limits = ComputeLimits {
        memory_bytes: 1 << 26,
        graph: Limits { elements: 1 << 20, work: 1 << 28 },
        ..ComputeLimits::default()
    };
    let alone = Runtime::from_plan(&plan(ComputeExtension::new(limits))).unwrap();
    let want: f64 = alone.eval(script).unwrap();
    let counting = Arc::new(Counting::default());
    let shared = ComputeExtension::new(limits).with_executor(counting.clone());
    let lent = Runtime::from_plan(&plan(shared)).unwrap();
    let got: f64 = lent.eval(script).unwrap();
    assert_eq!(got.to_bits(), want.to_bits());
    assert_eq!(counting.0.load(std::sync::atomic::Ordering::Relaxed), 12, "three products of four shares each");
}

/// A host's storage is its own business, even bytes it shares with a buffer: this checks them anyway.
#[test]
fn run_tensors_into_refuses_a_host_tensor_over_the_output() {
    #[derive(Debug)]
    struct Borrowed(*const u8, usize);
    // SAFETY: the test holds the buffer alive and writes it only through the refused call.
    unsafe impl Send for Borrowed {}
    // SAFETY: as above.
    unsafe impl Sync for Borrowed {}
    impl Storage for Borrowed {
        fn bytes(&self) -> &[u8] {
            // SAFETY: as above.
            unsafe { std::slice::from_raw_parts(self.0, self.1) }
        }
    }
    let plan = plan(ComputeExtension::default());
    let runtime = Runtime::from_plan(&plan).unwrap();
    let budget = Budget::new(1 << 20);
    let host_budget = budget.clone();
    let over_buffer = runtime
        .bind_function("dream.test.overBuffer", move |bytes: l3i::convert::BufferView<'_>| {
            // SAFETY: only this function's own test reads through it, while the buffer lives.
            let pointer = unsafe { bytes.bytes_unchecked() };
            let storage = Arc::new(Borrowed(pointer.as_ptr(), pointer.len()));
            let tensor = Tensor::from_storage(storage, Shape::Matrix(1, 1), 0, [4, 4])?;
            Ok::<_, l3i::Error>(Owned(ScriptTensor::new(tensor, host_budget.clone())))
        })
        .unwrap();
    runtime.set_global("overBuffer", &over_buffer).unwrap();
    runtime
        .exec(
            "local out = buffer.create(4)
             buffer.writef32(out, 0, -1)
             local tensor = overBuffer(out)
             local relu = compute.slot(0, 1, 1):relu():compile()
             assert(not pcall(function() relu:runTensorsInto(out, tensor) end), 'the output is an input')
             assert(buffer.readf32(out, 0) == -1, 'wrote while refusing')",
        )
        .unwrap();
}

/// The sizes a host allows are its own, and a default script can't reach past them.
#[test]
fn what_one_graph_may_do_is_the_hosts_to_raise() {
    let script = "local n = 512
         local bytes = buffer.create(n * n * 4)
         for i = 0, n * n - 1 do buffer.writef32(bytes, i * 4, 1) end
         local x, y = compute.slot(0, n, n), compute.slot(1, n, n)
         local program = x:matmul(y):compile()
         local out = buffer.create(n * n * 4)
         program:runInto(out, bytes, bytes)
         assert(buffer.readf32(out, 0) == n, 'a product of ones counts its inner dimension')
         local tall = compute.f32(buffer.create(1048580), 1, 262145)
         local large = compute.slot(0, 2048, 1024):relu():compile()
         local input = buffer.create(2048 * 1024 * 4)
         large:runInto(buffer.create(2048 * 1024 * 4), input)";
    let cautious = Runtime::from_plan(&plan(ComputeExtension::default())).unwrap();
    assert!(cautious.exec(script).is_err(), "the defaults refuse a 512x512 product");
    let trusted = ComputeExtension::new(ComputeLimits {
        memory_bytes: 1 << 26,
        graph: Limits { elements: 1 << 22, work: 1 << 28 },
        ..ComputeLimits::default()
    });
    Runtime::from_plan(&plan(trusted)).unwrap().exec(script).unwrap();
}

/// A size refused by policy allocates nothing, can't be helped by collecting and doesn't pretend it
/// can, while memory under a raised size limit still refuses, retries and recovers.
#[test]
fn raised_sizes_still_answer_to_the_memory_limit_and_its_garbage() {
    let budget = Budget::with_elements(1 << 20, 1 << 22);
    let domain = Domain::with_limits(64, 8, Limits { elements: 1 << 22, work: 1 << 30 });
    let plan = plan(ComputeExtension::shared(budget.clone(), domain.clone()));
    let runtime = Runtime::from_plan(&plan).unwrap();
    runtime
        .exec(
            "-- A shape over the policy: no allocation, no retry.
             assert(not pcall(compute.slot, 0, 4194305, 1))
             local tensor = compute.f32(buffer.create(4), 1, 1)
             assert(not pcall(function() return tensor:view(2097152, 4, 0, 0, 0) end), 'repeats past the limit')
             local repeated = tensor:view(2048, 2048, 0, 0, 0)
             -- Allowed by size, refused by memory: its result would be 16 MB under a 1 MB limit.
             local program = compute.slot(0, 2048, 2048):relu():compile()
             local refused, message = pcall(function() return program:run(repeated) end)
             assert(not refused and string.find(message, 'budget', 1, true), message)
             -- Garbage in the way gives way, at the same raised sizes.
             for _ = 1, 8 do compute.f32(buffer.create(524288), 512, 256) end
             local kept = compute.f32(buffer.create(524288), 512, 256)
             assert(compute.used() == 4 + 524288)",
        )
        .unwrap();
    assert_eq!(domain.denials(), 0, "no graph limit was involved");
    let refused = budget.denials();
    runtime
        .exec(
            "local first = compute.f32(buffer.create(524288), 512, 256)
             local second = compute.f32(buffer.create(524288), 512, 256)
             assert(not pcall(compute.f32, buffer.create(524288), 512, 256), 'three of these cannot fit')
             assert(first:read() and second:read())",
        )
        .unwrap();
    assert!(budget.denials() > refused, "memory refused after collecting");
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
