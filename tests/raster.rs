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
             assert(raster.WHITE == raster.rgba8(255, 255, 255, 255), 'folded constant') \
             -- The four bytes of a color in a buffer are r, g, b, a: the pixel layout. \
             local buf = buffer.create(4) buffer.writeu32(buf, 0, 0x44332211) \
             assert(buffer.readu8(buf, 0) == 0x11 and buffer.readu8(buf, 3) == 0x44, 'byte order') \
             local ok, err = pcall(raster.rgba8, 256, 0, 0, 0) assert(not ok and err:find('outside 0..=255'), err) \
             local clip = raster.clip(1, 2, 640, 480) \
             local x0, y0, x1, y1 = raster.clipBounds(clip) assert(x0 == 1 and y0 == 2 and x1 == 640 and y1 == 480, 'bounds') \
             local ok2, err2 = pcall(raster.clip, 5, 0, 4, 0) assert(not ok2 and err2:find('exceeds max'), err2) \
             local ok3, err3 = pcall(raster.clip, 0, 0, 16384, 0) assert(not ok3 and err3:find('16383'), err3) \
             assert(raster.clipBounds(raster.CLIP_ALL) == raster.CLIP_MAX_COORD, 'ALL reaches the limit') \
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
