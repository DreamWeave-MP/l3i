//! Colors and clip rectangles through Luau: construction, kind checks, and the byte layout.

use l3i::Runtime;
use l3i::extension::{RuntimePlan, RuntimePolicy};
use l3i::raster::{Color, RasterExtension};

#[test]
fn colors_and_clips_are_distinct_packed_kinds() {
    let plan = RuntimePlan::builder()
        .policy(RuntimePolicy::new().compat_global("@dream/raster", "raster"))
        .extension(RasterExtension)
        .finalize()
        .unwrap();
    let runtime = Runtime::from_plan(&plan).unwrap();
    runtime
        .exec(
            "local c = raster.rgba8(0x11, 0x22, 0x33, 0x44) \
             local r, g, b, a = raster.channels(c) assert(r == 0x11 and g == 0x22 and b == 0x33 and a == 0x44, 'channels') \
             assert(raster.rgb8(1, 2, 3) == raster.rgba8(1, 2, 3, 255), 'rgb8 is opaque') \
             assert(raster.withAlpha(c, 9) == raster.rgba8(0x11, 0x22, 0x33, 9), 'withAlpha') \
             assert(raster.lerp(raster.BLACK, raster.WHITE, 0.5) == raster.rgba8(128, 128, 128, 255), 'lerp') \
             assert(raster.mul(raster.rgba8(200, 100, 50, 255), raster.rgba8(128, 255, 0, 255)) == raster.rgba8(100, 100, 0, 255), 'mul') \
             assert(raster.add(raster.rgba8(200, 0, 0, 10), raster.rgba8(100, 0, 0, 250)) == raster.rgba8(255, 0, 0, 255), 'add') \
             assert(raster.scale(raster.rgba8(200, 100, 50, 7), 0.5) == raster.rgba8(100, 50, 25, 7), 'scale') \
             assert(raster.premultiply(raster.rgba8(255, 128, 1, 128)) == raster.rgba8(128, 64, 1, 128), 'premultiply') \
             local M = raster.math() \
             assert(M:rgba8(-4, 255.4, 254.5, 1e9) == raster.rgba8(0, 255, 255, 255), 'math clamps and rounds') \
             assert(M:lerp(raster.BLACK, raster.WHITE, 0 / 0) == raster.TRANSPARENT, 'NaN gives zero channels') \
             local r2, g2, b2, a2 = M:channels(c) assert(r2 == 0x11 and a2 == 0x44, 'math channels') \
             assert(M:red(c) == 0x11 and M:alpha(c) == 0x44, 'math getters') \
             assert(raster.WHITE == raster.rgba8(255, 255, 255, 255), 'folded constant') \
             -- The four bytes of a color in a buffer are r, g, b, a: the pixel layout. \
             local buf = buffer.create(4) buffer.writeu32(buf, 0, raster.packed(c)) assert(raster.packed(c) == 0x44332211, 'packed') \
             assert(buffer.readu8(buf, 0) == 0x11 and buffer.readu8(buf, 3) == 0x44, 'byte order') \
             local ok, err = pcall(raster.rgba8, 256, 0, 0, 0) assert(not ok and err:find('outside 0..=255'), err) \
             local clip = raster.clip(1, 2, 640, 480) \
             local x0, y0, x1, y1 = raster.clipBounds(clip) assert(x0 == 1 and y0 == 2 and x1 == 640 and y1 == 480, 'bounds') \
             local ok2, err2 = pcall(raster.clip, 5, 0, 4, 0) assert(not ok2 and err2:find('exceeds max'), err2) \
             local ok3, err3 = pcall(raster.clip, 0, 0, 16384, 0) assert(not ok3 and err3:find('16383'), err3) \
             local ax0, ay0, ax1, ay1 = raster.clipBounds(raster.CLIP_ALL) \
             assert(ax0 == 0 and ay0 == 0 and ax1 == raster.CLIP_MAX_COORD and ay1 == raster.CLIP_MAX_COORD, 'ALL spans origin to limit') \
             -- Kinds are checked: a clip is not a color, a color is not a clip, an integer is neither. \
             local ok4, err4 = pcall(raster.channels, clip) assert(not ok4 and err4:find('Color'), err4) \
             local ok5, err5 = pcall(raster.clipBounds, c) assert(not ok5 and err5:find('ClipRect'), err5) \
             assert(not pcall(raster.channels, 42i))",
        )
        .unwrap();
    // The Rust side agrees on the bit pattern scripts see.
    let function = runtime.load_function("return function() return raster.rgba8(0x11, 0x22, 0x33, 0x44) end").unwrap();
    let value = function.invoke::<l3i::convert::Integer, _>(&runtime.stack(), ()).unwrap();
    assert_eq!(value.0, Color::rgba(0x11, 0x22, 0x33, 0x44).pack().bits());
}

/// The receiver's methods lowered to native code agree with the module functions (the same Rust
/// arithmetic) across a grid of colors, factors, out-of-range and NaN inputs; wrong kinds exit.
#[cfg(feature = "jit")]
#[test]
fn color_math_lowers_to_native_code() {
    use l3i::extension::NativeCodePolicy;
    use l3i::native_code::{NativeCodeMode, NativeCodeStatus};
    use l3i::raster::lowering::lowered_sites;
    use l3i::runtime::{CallContext, MemoryCategory};
    use l3i::sandbox::{InstanceSpec, SandboxOptions};

    let policy = RuntimePolicy::new().compat_global("@dream/raster", "raster").native_code(NativeCodePolicy {
        mode: NativeCodeMode::Eager,
        record_counters: true,
        ..NativeCodePolicy::default()
    });
    let plan = RuntimePlan::builder().policy(policy).extension(RasterExtension).finalize().unwrap();
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
            "colors.lua",
            "--!native
             local M: dream_raster_Math = raster.math()
             local values = { -300, -1, 0, 0.4, 0.5, 1, 17, 127.5, 128, 200, 254.5, 255, 256, 1e9, 1 / 0, -1 / 0, 0 / 0 }
             local factors = { -1, 0, 0.25, 0.5, 1, 1.5, 3, 0 / 0 }
             local colors = {}
             for _, r in values do for _, a in values do
                 local lowered = M:rgba8(r, a, 255 - (r % 256), a)
                 table.insert(colors, lowered)
             end end
             local checks = 0
             for i, a in colors do
                 local b = colors[(i * 7) % #colors + 1]
                 assert(M:lerp(a, b, 0.3) == raster.lerp(a, b, 0.3), 'lerp')
                 assert(M:mul(a, b) == raster.mul(a, b), 'mul')
                 assert(M:add(a, b) == raster.add(a, b), 'add')
                 assert(M:premultiply(a) == raster.premultiply(a), 'premultiply')
                 assert(M:withAlpha(a, i) == raster.withAlpha(a, i), 'withAlpha')
                 local r, g, bb, al = M:channels(a)
                 local r1, g1, b1, a1 = raster.channels(a)
                 assert(r == r1 and g == g1 and bb == b1 and al == a1, 'channels')
                 assert(M:red(a) == r1 and M:green(a) == g1 and M:blue(a) == b1 and M:alpha(a) == a1, 'getters')
                 assert(M:rgb8(r1, g1, b1) == raster.rgb8(r1, g1, b1), 'rgb8')
                 for _, f in factors do
                     assert(M:scale(a, f) == raster.scale(a, f), 'scale')
                     assert(M:lerp(a, b, f) == raster.lerp(a, b, f), 'lerp factor')
                 end
                 checks += 1
             end
             -- Wrong kinds exit to the interpreter, whose method raises the type error.
             local clip = raster.clip(0, 0, 1, 1)
             local ok, err = pcall(function() local r = M:red(clip) return r end)
             assert(not ok and string.find(err, 'Color'), err)
             local ok2 = pcall(function() local r = M:mul(colors[1], 42i) return r end)
             assert(not ok2)
             return checks",
        )
        .unwrap();
    let native = template.native_code().expect("compiled");
    assert_eq!(native.status, NativeCodeStatus::Success, "{native:?}");
    let sites = lowered_sites() - before;
    assert!(sites >= 15, "expected every receiver call site to lower, got {sites}");
    let loader = runtime.load_function("return function(name) error('module ' .. name .. ' not found') end").unwrap();
    let instance = sandbox
        .new_instance(&runtime, &InstanceSpec { name: "c", packages: &[], hidden_data: None, loader: &loader })
        .unwrap();
    let results = sandbox.run(&runtime, &template, &instance, CallContext { id: 1, category: MemoryCategory(0) }).unwrap();
    let checks: f64 = runtime.stack().with_frame(|frame| results[0].push_to(frame)?.read::<f64>()).unwrap();
    assert_eq!(checks as usize, 17 * 17);
    let stats = generator.execution_stats(&runtime.stack());
    assert_eq!(stats.vm_exits_taken, 2, "only the two wrong-kind calls exit: {stats:?}");
}
