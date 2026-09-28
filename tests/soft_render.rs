//! The dream-soft-render extension: a scene drawn from Luau is byte-identical to the same scene
//! drawn from Rust, malformed input fails loudly, textures free themselves, and the extension
//! works in two runtimes with different tags.

use dream_soft_render::{ClipRect, Color, Mesh, Rect, SoftwareRenderer, Vertex};
use l3i::Runtime;
use l3i::convert::BytesView;
use l3i::extension::{RuntimePlan, RuntimePolicy};
use l3i::raster::RasterExtension;
use l3i::soft_render::SoftRenderExtension;

const WIDTH: usize = 64;
const HEIGHT: usize = 48;

fn plan(policy: RuntimePolicy) -> std::rc::Rc<RuntimePlan> {
    RuntimePlan::builder()
        .policy(policy.compat_global("@dream/raster", "raster").compat_global("@dream/soft-render", "soft"))
        .extension(RasterExtension)
        .extension(SoftRenderExtension)
        .finalize()
        .unwrap()
}

fn checkerboard() -> Vec<u8> {
    vec![255, 255, 255, 255, 0, 0, 0, 255, 0, 0, 0, 255, 255, 255, 255, 255]
}

/// The reference scene, drawn natively.
fn native_scene() -> Vec<u8> {
    let mut renderer = SoftwareRenderer::default();
    let texture = renderer.create_texture(2, 2, &checkerboard()).unwrap();
    let mut frame = renderer.begin_frame(WIDTH, HEIGHT).unwrap();
    frame.clear(Color::from_rgb(17, 20, 28));
    frame
        .fill_rect(
            Rect::from_min_max([4.0, 4.0], [28.0, 16.0]),
            Color::from_rgba_unmultiplied(80, 160, 255, 192),
            ClipRect::ALL,
        )
        .unwrap();
    frame
        .fill_rect(
            Rect::from_min_max([0.0, 0.0], [64.0, 48.0]),
            Color::from_rgba_unmultiplied(0, 255, 0, 64),
            ClipRect::new(2, 20, 20, 30),
        )
        .unwrap();
    frame
        .textured_rect(Rect::from_min_max([32.0, 4.0], [48.0, 20.0]), Rect::FULL_UV, texture, Color::WHITE, ClipRect::ALL)
        .unwrap();
    renderer.update_texture(texture, 1, 1, 1, 1, &[255, 0, 0, 255]).unwrap();
    let mut frame = renderer.begin_frame(WIDTH, HEIGHT).unwrap();
    frame
        .textured_rect(
            Rect::from_min_max([50.0, 4.0], [62.0, 16.0]),
            Rect::FULL_UV,
            texture,
            Color::from_rgba_unmultiplied(255, 255, 255, 128),
            ClipRect::ALL,
        )
        .unwrap();
    let warning = Color::from_rgb(220, 40, 40);
    frame
        .mesh(
            Mesh {
                vertices: &[
                    Vertex::new([8.0, 40.0], [0.0, 0.0], warning),
                    Vertex::new([24.0, 24.0], [0.0, 0.0], warning),
                    Vertex::new([40.0, 40.0], [0.0, 0.0], warning),
                    Vertex::new([44.0, 44.0], [0.0, 0.0], Color::WHITE),
                    Vertex::new([60.0, 30.0], [1.0, 0.0], Color::WHITE),
                    Vertex::new([60.0, 46.0], [1.0, 1.0], Color::WHITE),
                ],
                indices: &[0, 1, 2, 3, 4, 5],
                texture: Some(texture),
            },
            ClipRect::ALL,
        )
        .unwrap();
    frame.surface().pixels.clone()
}

/// The same scene from Luau: colors through `raster`, geometry through buffers.
const LUAU_SCENE: &str = r"
    local renderer = soft.renderer()
    local checker = buffer.create(16)
    for i, v in { 255, 255, 255, 255, 0, 0, 0, 255, 0, 0, 0, 255, 255, 255, 255, 255 } do buffer.writeu8(checker, i - 1, v) end
    local texture = renderer:createTexture(2, 2, checker)
    assert(texture.width == 2 and texture.height == 2, 'texture fields')
    local frame = renderer:beginFrame(64, 48)
    assert(frame.width == 64 and frame.height == 48 and renderer.width == 64, 'frame fields')
    frame:clear(raster.rgb8(17, 20, 28))
    frame:rect(vector.create(4, 4), vector.create(28, 16), soft.premultiply(raster.rgba8(80, 160, 255, 192)), raster.CLIP_ALL)
    frame:rect(vector.create(0, 0), vector.create(64, 48), soft.premultiply(raster.rgba8(0, 255, 0, 64)), raster.clip(2, 20, 20, 30))
    frame:image(vector.create(32, 4), vector.create(48, 20), vector.zero, vector.one, texture, raster.WHITE, raster.CLIP_ALL)
    frame:finish()
    texture:update(1, 1, 1, 1, string.char(255, 0, 0, 255))
    frame = renderer:beginFrame(64, 48)
    frame:image(vector.create(50, 4), vector.create(62, 16), vector.zero, vector.one, texture, soft.premultiply(raster.rgba8(255, 255, 255, 128)), raster.CLIP_ALL)
    local warning = raster.packed(raster.rgb8(220, 40, 40))
    local white = raster.packed(raster.WHITE)
    local vertices = buffer.create(6 * soft.VERTEX_BYTES)
    local function vertex(i, x, y, u, v, color)
        local o = i * soft.VERTEX_BYTES
        buffer.writef32(vertices, o, x) buffer.writef32(vertices, o + 4, y)
        buffer.writef32(vertices, o + 8, u) buffer.writef32(vertices, o + 12, v)
        buffer.writeu32(vertices, o + 16, color)
    end
    vertex(0, 8, 40, 0, 0, warning) vertex(1, 24, 24, 0, 0, warning) vertex(2, 40, 40, 0, 0, warning)
    vertex(3, 44, 44, 0, 0, white) vertex(4, 60, 30, 1, 0, white) vertex(5, 60, 46, 1, 1, white)
    local indices = buffer.create(6 * 4)
    for i = 0, 5 do buffer.writeu32(indices, i * 4, i) end
    frame:mesh(vertices, indices, texture, raster.CLIP_ALL)
    frame:finish()
    out = buffer.create(64 * 48 * 4)
    assert(renderer:readInto(out) == 64 * 48 * 4, 'readInto returns the byte count')
";

fn bytes_of_global(runtime: &Runtime, name: &str) -> Vec<u8> {
    let value = runtime.global(name).unwrap();
    runtime
        .stack()
        .with_frame(|frame| {
            let view = value.push_to(frame)?;
            Ok(view.read::<BytesView>()?.to_vec())
        })
        .unwrap()
}

#[test]
fn luau_scene_matches_the_native_scene_byte_for_byte() {
    let runtime = Runtime::from_plan(&plan(RuntimePolicy::new())).unwrap();
    runtime.exec(LUAU_SCENE).unwrap();
    let native = native_scene();
    let luau = bytes_of_global(&runtime, "out");
    assert_eq!(luau.len(), WIDTH * HEIGHT * 4);
    if luau != native {
        let diffs: Vec<usize> = (0..native.len() / 4).filter(|i| luau[i * 4..i * 4 + 4] != native[i * 4..i * 4 + 4]).collect();
        let first = diffs[0];
        eprintln!(
            "{} pixels differ; first at ({}, {}): luau {:?} native {:?}; last at ({}, {})",
            diffs.len(),
            first % WIDTH,
            first / WIDTH,
            &luau[first * 4..first * 4 + 4],
            &native[first * 4..first * 4 + 4],
            diffs[diffs.len() - 1] % WIDTH,
            diffs[diffs.len() - 1] / WIDTH
        );
        panic!("the Luau scene diverges from the native scene");
    }
    // Something was drawn: not every pixel is the background.
    assert!(luau.chunks(4).any(|p| p != [17, 20, 28, 255]));
}

#[test]
fn malformed_input_fails_loudly() {
    let runtime = Runtime::from_plan(&plan(RuntimePolicy::new())).unwrap();
    runtime.exec("renderer = soft.renderer() frame = renderer:beginFrame(8, 8)").unwrap();
    let cases: &[(&str, &str)] = &[
        ("frame:mesh(buffer.create(21), buffer.create(12), nil, raster.CLIP_ALL)", "not a multiple of 20"),
        ("frame:mesh(buffer.create(60), buffer.create(6), nil, raster.CLIP_ALL)", "not a multiple of 4"),
        (
            "local i = buffer.create(12) buffer.writeu32(i, 8, 9) frame:mesh(buffer.create(60), i, nil, raster.CLIP_ALL)",
            "out of range",
        ),
        ("frame:mesh(buffer.create(60), buffer.create(8), nil, raster.CLIP_ALL)", "not a multiple of three"),
        ("frame:rect(vector.create(0 / 0, 0), vector.create(4, 4), raster.WHITE, raster.CLIP_ALL)", "non-finite"),
        ("frame:rect(vector.create(0, 0), vector.create(4, 4), raster.CLIP_ALL, raster.CLIP_ALL)", "Color"),
        ("frame:rect(vector.create(0, 0), vector.create(4, 4), raster.WHITE, raster.WHITE)", "ClipRect"),
        ("renderer:createTexture(2, 2, buffer.create(15))", "15 bytes, expected 16"),
        ("renderer:createTexture(0, 2, buffer.create(0))", "empty or overflows"),
        ("renderer:beginFrame(1281, 721)", "pixel budget"),
        ("renderer:readInto(buffer.create(8 * 8 * 4 - 1))", "out of bounds"),
        (
            "local t = renderer:createTexture(1, 1, buffer.create(4)) t:update(1, 0, 1, 1, buffer.create(4))",
            "exceeds the bounds",
        ),
        ("local t = renderer:createTexture(1, 1, buffer.create(4)) t:free() t:free()", "was freed"),
        (
            "local t = renderer:createTexture(1, 1, buffer.create(4)) t:free() \
             frame:image(vector.zero, vector.one, vector.zero, vector.one, t, raster.WHITE, raster.CLIP_ALL)",
            "was freed",
        ),
        ("frame:finish() frame:clear(raster.BLACK)", "frame is finished"),
        ("local f = renderer:beginFrame(8, 8) renderer:beginFrame(8, 8) f:clear(raster.BLACK)", "frame is finished"),
    ];
    for (source, expected) in cases {
        let error = runtime.exec(source).unwrap_err().to_string();
        assert!(error.contains(expected), "{source}\n  expected '{expected}', got: {error}");
    }
}

#[test]
fn textures_free_their_storage_when_collected() {
    use l3i::memory::GcControl;
    let runtime = Runtime::from_plan(&plan(RuntimePolicy::new())).unwrap();
    // Six 2 MiB textures exceed the 8 MiB budget unless collected ones are freed. Luau has no
    // collectgarbage(); the host drives the collector between iterations.
    runtime.exec("renderer = soft.renderer() pixels = buffer.create(1024 * 512 * 4)").unwrap();
    for _ in 0..6 {
        runtime.exec("local t = renderer:createTexture(1024, 512, pixels)").unwrap();
        runtime.gc(GcControl::Collect);
    }
    runtime
        .exec(
            "local held = {} for i = 1, 4 do held[i] = renderer:createTexture(1024, 512, pixels) end \
             local ok, err = pcall(renderer.createTexture, renderer, 1024, 512, pixels) \
             assert(not ok and err:find('budget'), err) \
             held[1]:free() renderer:createTexture(1024, 512, pixels)",
        )
        .unwrap();
}

#[test]
fn two_runtimes_with_different_tags_draw_the_same_bytes() {
    let a = Runtime::from_plan(&plan(RuntimePolicy::new())).unwrap();
    let b = Runtime::from_plan(&plan(RuntimePolicy::new().first_tag(40))).unwrap();
    assert_ne!(
        a.plan().unwrap().tag_of("dream.soft_render.Frame"),
        b.plan().unwrap().tag_of("dream.soft_render.Frame")
    );
    a.exec(LUAU_SCENE).unwrap();
    b.exec(LUAU_SCENE).unwrap();
    let (from_a, from_b) = (bytes_of_global(&a, "out"), bytes_of_global(&b, "out"));
    assert!(from_a.iter().eq(from_b.iter()), "the two runtimes drew different bytes");
    assert!(a.type_definitions().unwrap().contains("declare class dream_soft_render_Frame"));
}
