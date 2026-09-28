//! Packed quaternion experiment: precision, drift, semantics through Luau.

#![allow(clippy::cast_possible_truncation, clippy::cast_precision_loss)]

#[path = "support/packed_quat.rs"]
mod support;

use l3i::Runtime;
use l3i::extension::{RuntimePlan, RuntimePolicy};
use support::{AnimationKey, PackedRotation, Quat, QuatExtension, Quaternion, random_quat};

#[test]
fn smallest_three_precision_over_random_rotations() {
    let mut rng = 0x9E37_79B9_7F4A_7C15u64;
    let (mut max_error, mut total) = (0.0f64, 0.0f64);
    const N: usize = 200_000;
    for _ in 0..N {
        let q = random_quat(&mut rng);
        let back = PackedRotation::encode(q).decode();
        let error = q.angle_to(back);
        max_error = max_error.max(error);
        total += error;
    }
    let mean = total / N as f64;
    eprintln!("smallest-three 18-bit: max angular error {max_error:.3e} rad, mean {mean:.3e} rad");
    // Quantisation step 2/2^18 per component over a range of sqrt(2): about 5e-6 per component.
    assert!(max_error < 2.0e-5, "max error {max_error}");
    assert!(mean < 1.0e-5, "mean error {mean}");
    // The packed form fits the payload and identity round-trips exactly.
    assert!(PackedRotation::encode(Quat::IDENTITY).0 < (1 << 56));
    assert!(Quat::IDENTITY.angle_to(PackedRotation::encode(Quat::IDENTITY).decode()) < 1e-9);
}

#[test]
fn packed_form_is_storage_not_an_accumulator() {
    // Re-encoding after every blend step accumulates quantisation error as a random walk. The
    // right pattern keeps the live state in f64 and packs only what is stored or sent, which
    // stays within one quantisation step. Both are measured here so the numbers are on record.
    let mut rng = 0xDEAD_BEEF_CAFE_F00Du64;
    let target = random_quat(&mut rng);
    let mut reference = Quat::IDENTITY;
    let mut chained = PackedRotation::encode(Quat::IDENTITY);
    let (mut worst_chained, mut worst_snapshot) = (0.0f64, 0.0f64);
    const STEPS: usize = 100_000;
    for step in 0..STEPS {
        let t = 0.001 + 0.004 * ((step % 100) as f64 / 100.0);
        reference = reference.slerp(target, t).normalize();
        chained = PackedRotation::encode(chained.decode().slerp(target, t));
        worst_chained = worst_chained.max(reference.angle_to(chained.decode()));
        worst_snapshot = worst_snapshot.max(reference.angle_to(PackedRotation::encode(reference).decode()));
    }
    eprintln!(
        "{STEPS} slerp steps: re-encoding every step diverges {worst_chained:.3e} rad; packing the f64 state each step stays within {worst_snapshot:.3e} rad"
    );
    assert!(worst_snapshot < 2.0e-5, "snapshot error {worst_snapshot}");
    assert!(worst_chained > worst_snapshot * 10.0, "expected the chained form to accumulate; got {worst_chained}");
    assert!(worst_chained < 1.0e-2, "chained drift {worst_chained} rad over {STEPS} steps");
}

#[test]
fn packed_quaternions_and_keys_are_distinct_kinds_through_luau() {
    let plan = RuntimePlan::builder()
        .policy(RuntimePolicy::new().compat_global("@dream/quat", "quat"))
        .extension(QuatExtension)
        .finalize()
        .unwrap();
    let runtime = Runtime::from_plan(&plan).unwrap();
    runtime
        .exec(
            "local a = quat.axisAngle(vector.create(0, 0, 1), math.pi / 2) \
             local b = quat.axisAngle(vector.create(0, 0, 1), math.pi / 2) \
             local c = quat.mul(a, b) \
             local v = quat.rotate(c, vector.create(1, 0, 0)) \
             assert(math.abs(v.x + 1) < 1e-4 and math.abs(v.y) < 1e-4, 'rotate 180 about z') \
             assert(quat.angleTo(a, b) < 1e-5, 'identical rotations') \
             local half = quat.slerp(a, c, 0.5) assert(quat.angleTo(half, quat.axisAngle(vector.create(0, 0, 1), math.pi * 0.75)) < 1e-4, 'slerp') \
             -- A packed quaternion is an integer physically, but not semantically: the wrong kind fails. \
             assert(type(a) == 'number' or type(a) == 'integer') \
             local key = quat.key(a, 5) assert(quat.keyFlags(key) == 5) assert(quat.angleTo(quat.keyRotation(key), a) < 1e-5) \
             local ok, err = pcall(quat.mul, a, key) assert(not ok and err:find('Quaternion'), err) \
             local ok2, err2 = pcall(quat.keyFlags, a) assert(not ok2 and err2:find('AnimationKey'), err2) \
             local ok3 = pcall(quat.mul, a, 42i) assert(not ok3) \
             -- The userdata baseline agrees. \
             local ua = quat.udAxisAngle(vector.create(0, 0, 1), math.pi / 2) local uc = ua:mul(ua) \
             local uv = uc:rotate(vector.create(1, 0, 0)) assert(math.abs(uv.x + 1) < 1e-4)",
        )
        .unwrap();
    let key = l3i::packed::Packed(AnimationKey { rotation: Quat::IDENTITY, flags: 9 });
    let back = l3i::packed::Packed::<AnimationKey>::from_bits(key.bits()).unwrap();
    assert_eq!(back.0.flags, 9);
    assert!(l3i::packed::Packed::<Quaternion>::from_bits(key.bits()).is_err());
}

/// Native lowering: the same operations through the `dream_quat_Math` receiver compile to IR
/// with no C call, agree with the binder path, and fall back to it on a wrong kind.
#[cfg(feature = "jit")]
#[test]
fn packed_quaternion_operations_lower_to_native_code() {
    use std::sync::atomic::Ordering;

    use l3i::extension::NativeCodePolicy;
    use l3i::native_code::{NativeCodeMode, NativeCodeStatus};
    use l3i::runtime::{CallContext, MemoryCategory};
    use l3i::sandbox::{InstanceSpec, SandboxOptions};
    use support::lowering::LOWERED;

    let policy = RuntimePolicy::new()
        .compat_global("@dream/quat", "quat")
        .native_code(NativeCodePolicy { mode: NativeCodeMode::Eager, record_counters: true, ..NativeCodePolicy::default() });
    let plan = RuntimePlan::builder().policy(policy).extension(QuatExtension).finalize().unwrap();
    assert_eq!(plan.tag_of("dream.quat.Math"), Some(1), "the receiver must be userdata type index 0");
    let runtime = Runtime::from_plan(&plan).unwrap();
    let generator = runtime.native_code().expect("built with native code");
    if !generator.is_available() {
        eprintln!("no Luau code generator on this platform; skipping");
        return;
    }
    let sandbox = runtime
        .sandbox(|_| {}, SandboxOptions { compile_options: runtime.compile_options(), ..SandboxOptions::default() })
        .unwrap();
    let before = LOWERED.load(Ordering::Relaxed);
    let template = sandbox
        .load_template(
            &runtime,
            "quat.lua",
            "--!native\n\
             local Q: dream_quat_Math = quat.math()\n\
             local a = quat.axisAngle(vector.create(0, 0, 1), 0.3)\n\
             local b = quat.axisAngle(vector.create(1, 0, 0), 0.7)\n\
             local v = vector.create(1, 2, 3)\n\
             local worst = 0\n\
             for i = 1, 200 do\n\
                 local x = quat.axisAngle(vector.create(math.sin(i), math.cos(i * 0.7), 0.5), i * 0.05)\n\
                 local lowered = Q:rotate(x, v)\n\
                 local binder = quat.rotate(x, v)\n\
                 worst = math.max(worst, vector.magnitude(lowered - binder))\n\
                 local m1 = Q:mul(x, b)\n\
                 local m2 = quat.mul(x, b)\n\
                 worst = math.max(worst, quat.angleTo(m1, m2))\n\
             end\n\
             local key = quat.key(a, 3)\n\
             -- Single-result calls so the hook lowers these sites too; the kind check then\n\
             -- exits to the interpreter, whose C method raises the type error.\n\
             local ok, err = pcall(function() local r = Q:mul(a, key) return r end)\n\
             assert(not ok and string.find(err, 'Quaternion'), err)\n\
             local ok2 = pcall(function() local r = Q:rotate(42i, v) return r end)\n\
             assert(not ok2)\n\
             return worst",
        )
        .unwrap();
    let native = template.native_code().expect("compiled");
    assert_eq!(native.status, NativeCodeStatus::Success, "{native:?}");
    assert_eq!(LOWERED.load(Ordering::Relaxed) - before, 4, "the hook lowered the two loop sites and the two closures");
    let loader = runtime.load_function("return function(name) error('module ' .. name .. ' not found') end").unwrap();
    let instance = sandbox
        .new_instance(&runtime, &InstanceSpec { name: "q", packages: &[], hidden_data: None, loader: &loader })
        .unwrap();
    let results = sandbox.run(&runtime, &template, &instance, CallContext { id: 1, category: MemoryCategory(0) }).unwrap();
    let worst: f64 = results[0].push_to(&runtime.stack().frame()).map(|v| v.read::<f64>().unwrap()).unwrap();
    eprintln!("lowered vs binder worst divergence: {worst:.3e}");
    assert!(worst < 5e-5, "lowered results diverge from the binder: {worst}");
    let stats = generator.execution_stats(&runtime.stack());
    assert!(stats.regular_blocks_executed > 0, "{stats:?}");
    // Exactly the two wrong-kind calls exit; the 400 lowered calls in the loop run natively.
    assert_eq!(stats.vm_exits_taken, 2, "only the wrong-kind calls exit to the interpreter: {stats:?}");
}
