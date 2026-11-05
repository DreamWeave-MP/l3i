//! The compute engine through whole JSL invocations, on the shared host. The engine's kernels and
//! planner are tested against independent oracles in `src/compute`.

mod host;

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

/// Sub-ranges with different starts still count: start-address equality would miss these.
#[test]
fn overlap_is_a_range_check() {
    let bytes = [0u8; 16];
    assert!(host::overlaps(&bytes[0..12], &bytes[4..8]), "contained, different start");
    assert!(host::overlaps(&bytes[0..8], &bytes[4..12]), "partial");
    assert!(host::overlaps(&bytes[4..12], &bytes[0..8]), "partial, other order");
    assert!(host::overlaps(&bytes, &bytes), "identical");
    assert!(!host::overlaps(&bytes[0..8], &bytes[8..16]), "adjacent");
    assert!(!host::overlaps(&bytes[8..16], &bytes[0..8]), "adjacent, other order");
    assert!(!host::overlaps(&bytes[4..4], &bytes), "empty");
    assert!(!host::overlaps(&bytes, &bytes[4..4]), "empty, other side");
}

#[test]
fn jsl_programs_and_teardown() {
    run().unwrap();
    run().unwrap();
}

/// The checked-in shader is what the planner emits today, so `jess run` on it tests current code.
#[test]
fn generated_shader_is_current() {
    let runtime = host::runtime().unwrap();
    runtime.exec(include_str!("compute/example.jsl")).unwrap();
    runtime.exec(include_str!("compute/slots.jsl")).unwrap();
    let source: String = runtime.eval("return shared.state.shader_artifact").unwrap();
    assert_eq!(source, include_str!("compute/generated-shader.jsl"));
}

/// Without the code generator, a whole program maps no executable memory at runtime.
#[cfg(all(not(feature = "jit"), target_os = "linux"))]
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
