//! Packed quaternions through Luau: the `dream.quat` extension against the f32 userdata
//! baseline, kind checks, and (jit) the native lowering.

#![allow(clippy::cast_possible_truncation, clippy::cast_precision_loss)]

#[path = "support/quat_baseline.rs"]
mod support;

use l3i::Runtime;
use l3i::extension::{RuntimePlan, RuntimePolicy};
use l3i::packed::Packed;
use l3i::quat::{Quat, QuatExtension, Quaternion};
use support::{AnimationKey, QuatBaseline};

fn policy() -> RuntimePolicy {
    RuntimePolicy::new().compat_global("@dream/quat", "quat").compat_global("@dream/quatud", "quatud")
}

#[test]
fn packed_quaternions_and_keys_are_distinct_kinds_through_luau() {
    let plan =
        RuntimePlan::builder().policy(policy()).extension(QuatExtension).extension(QuatBaseline).finalize().unwrap();
    let runtime = Runtime::from_plan(&plan).unwrap();
    runtime
        .exec(
            "local a = quat.axisAngle(vector.create(0, 0, 1), math.pi / 2) \
             local b = quat.axisAngle(vector.create(0, 0, 1), math.pi / 2) \
             local c = quat.mul(a, b) \
             local v = quat.rotate(c, vector.create(1, 0, 0)) \
             assert(math.abs(v.x + 1) < 1e-4 and math.abs(v.y) < 1e-4, 'rotate 180 about z') \
             assert(quat.angleTo(a, b) < 1e-5, 'identical rotations') \
             assert(quat.angleTo(quat.mul(a, quat.inverse(a)), quat.IDENTITY) < 1e-5, 'inverse') \
             local x, y, z, w = quat.toXYZW(quat.IDENTITY) assert(x == 0 and y == 0 and z == 0 and w == 1, 'toXYZW') \
             assert(quat.angleTo(quat.fromXYZW(0, 0, 2, 2), quat.axisAngle(vector.create(0, 0, 1), math.pi / 2)) < 1e-5, 'fromXYZW normalises') \
             local half = quat.slerp(a, c, 0.5) assert(quat.angleTo(half, quat.axisAngle(vector.create(0, 0, 1), math.pi * 0.75)) < 1e-4, 'slerp') \
             -- A packed quaternion is an integer physically, but not semantically: the wrong kind fails. \
             assert(type(a) == 'number' or type(a) == 'integer') \
             local key = quatud.key(a, 5) assert(quatud.keyFlags(key) == 5) assert(quat.angleTo(quatud.keyRotation(key), a) < 1e-5) \
             local ok, err = pcall(quat.mul, a, key) assert(not ok and err:find('Quaternion'), err) \
             local ok2, err2 = pcall(quatud.keyFlags, a) assert(not ok2 and err2:find('AnimationKey'), err2) \
             local ok3 = pcall(quat.mul, a, 42i) assert(not ok3) \
             -- The userdata baseline agrees. \
             local ua = quatud.axisAngle(vector.create(0, 0, 1), math.pi / 2) local uc = ua:mul(ua) \
             local uv = uc:rotate(vector.create(1, 0, 0)) assert(math.abs(uv.x + 1) < 1e-4)",
        )
        .unwrap();
    let key = Packed(AnimationKey { rotation: Quat::IDENTITY, flags: 9 });
    let back = Packed::<AnimationKey>::from_bits(key.bits()).unwrap();
    assert_eq!(back.0.flags, 9);
    assert!(Packed::<Quaternion>::from_bits(key.bits()).is_err());
    assert_eq!(Packed::<Quaternion>::from_bits(Quaternion::pack(Quat::IDENTITY).bits()).unwrap().0.0, Quat::IDENTITY);
}

/// Native lowering: the same operations through the `dream_quat_Math` receiver compile to IR
/// with no C call, agree with the binder path, and fall back to it on a wrong kind.
#[cfg(feature = "jit")]
#[test]
fn packed_quaternion_operations_lower_to_native_code() {
    use l3i::extension::NativeCodePolicy;
    use l3i::native_code::{NativeCodeMode, NativeCodeStatus};
    use l3i::quat::lowering::lowered_sites;
    use l3i::runtime::{CallContext, MemoryCategory};
    use l3i::sandbox::{InstanceSpec, SandboxOptions};

    let policy = policy().native_code(NativeCodePolicy {
        mode: NativeCodeMode::Eager,
        record_counters: true,
        ..NativeCodePolicy::default()
    });
    // The userdata baseline is pinned to tag 1, so the receiver is the second compiler userdata
    // type and the hook must look its index up rather than assume the first.
    let plan = RuntimePlan::builder()
        .policy(policy)
        .pin_tag("dream.quat.Quat", 1)
        .extension(QuatExtension)
        .extension(QuatBaseline)
        .finalize()
        .unwrap();
    assert_eq!(plan.tag_of("dream.quat.Math"), Some(2));
    let runtime = Runtime::from_plan(&plan).unwrap();
    let generator = runtime.native_code().expect("built with native code");
    if !generator.is_available() {
        eprintln!("no Luau code generator on this platform; skipping");
        return;
    }
    let sandbox = runtime
        .sandbox(|_| {}, SandboxOptions { compile_options: runtime.compile_options(), ..SandboxOptions::default() })
        .unwrap();
    let before = lowered_sites();
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
             local key = quatud.key(a, 3)\n\
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
    assert_eq!(lowered_sites() - before, 4, "the hook lowered the two loop sites and the two closures");
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
