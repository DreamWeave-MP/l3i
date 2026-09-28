//! The dream-soft-render bridge (`soft-render` feature), in the three layers the integration
//! handoff asks for: the native renderer alone, the same workloads driven from Luau, and the
//! per-call binding costs. Workloads mirror dream-soft-render's own `benches/draw.rs` at 640x480:
//! 48 solid panels, 3200 glyph quads from an atlas, and a 256-triangle fan.

#![allow(clippy::cast_precision_loss, clippy::semicolon_if_nothing_returned, clippy::missing_panics_doc)]

use std::time::Duration;

use criterion::{Criterion, Throughput, criterion_group, criterion_main};
use dream_soft_render::{ClipRect, Color, Mesh, Rect, SoftwareRenderer, Vertex};
use l3i::Runtime;
use l3i::extension::{RuntimePlan, RuntimePolicy};
use l3i::raster::RasterExtension;
use l3i::soft_render::SoftRenderExtension;
use l3i::value::Function;

const WIDTH: usize = 640;
const HEIGHT: usize = 480;
const ATLAS_WIDTH: usize = 256;
const ATLAS_HEIGHT: usize = 64;
const GLYPH_WIDTH: usize = 7;
const GLYPH_HEIGHT: usize = 9;
const PANELS: u64 = 48;
const GLYPHS: u64 = 40 * 80;

fn coverage_atlas() -> Vec<u8> {
    let mut pixels = vec![0u8; ATLAS_WIDTH * ATLAS_HEIGHT * 4];
    for y in 0..ATLAS_HEIGHT {
        for x in 0..ATLAS_WIDTH {
            let coverage = match (x % GLYPH_WIDTH, y % GLYPH_HEIGHT) {
                (0, _) | (_, 0) => 0,
                (1, _) | (_, 1) => 96,
                _ => 255,
            };
            let at = (y * ATLAS_WIDTH + x) * 4;
            pixels[at..at + 4].copy_from_slice(&[coverage, coverage, coverage, coverage]);
        }
    }
    pixels
}

fn fan_vertices() -> Vec<Vertex> {
    let center = [320.0, 240.0];
    let mut vertices = vec![Vertex::new(center, [0.0, 0.0], Color::WHITE)];
    for step in 0..=256_u16 {
        let angle = f32::from(step) / 256.0 * std::f32::consts::TAU;
        let shade = u8::try_from(step % 256).unwrap_or(u8::MAX);
        vertices.push(Vertex::new(
            [center[0] + 200.0 * angle.cos(), center[1] + 180.0 * angle.sin()],
            [0.0, 0.0],
            Color::from_rgba_unmultiplied(shade, 255 - shade, 128, 220),
        ));
    }
    vertices
}

fn fan_indices() -> Vec<u32> {
    (1..=256).flat_map(|edge| [0, edge, edge + 1]).collect()
}

fn glyph_rects() -> Vec<(Rect, Rect)> {
    let columns = ATLAS_WIDTH / GLYPH_WIDTH;
    let rows = ATLAS_HEIGHT / GLYPH_HEIGHT;
    let f = |v: usize| v as f32;
    let mut out = Vec::new();
    for line in 0..40 {
        for column in 0..80 {
            let glyph = (line * 31 + column * 7) % (columns * rows);
            let (u, v) = ((glyph % columns) * GLYPH_WIDTH, (glyph / columns) * GLYPH_HEIGHT);
            let uv = Rect::from_min_max(
                [(f(u) + 0.5) / f(ATLAS_WIDTH - 1), (f(v) + 0.5) / f(ATLAS_HEIGHT - 1)],
                [(f(u + GLYPH_WIDTH) - 0.5) / f(ATLAS_WIDTH - 1), (f(v + GLYPH_HEIGHT) - 0.5) / f(ATLAS_HEIGHT - 1)],
            );
            let rect = Rect::from_min_size(
                [f(4 + column * (GLYPH_WIDTH + 1)), f(4 + line * (GLYPH_HEIGHT + 3))],
                [f(GLYPH_WIDTH), f(GLYPH_HEIGHT)],
            );
            out.push((rect, uv));
        }
    }
    out
}

/// Layer one: the renderer with no Luau at all.
fn native(c: &mut Criterion) {
    let mut renderer = SoftwareRenderer::default();
    let atlas = renderer.create_texture(ATLAS_WIDTH, ATLAS_HEIGHT, &coverage_atlas()).unwrap();
    let (vertices, indices, glyphs) = (fan_vertices(), fan_indices(), glyph_rects());
    let tint = Color::from_rgb(220, 224, 230);
    let mut group = c.benchmark_group("soft_render_native");
    group.bench_function("panels frame", |b| {
        b.iter(|| {
            let mut frame = renderer.begin_frame(WIDTH, HEIGHT).unwrap();
            frame.clear(Color::from_rgb(17, 20, 28));
            for panel in 0..48_usize {
                let (x, y) = (((panel % 8) * 80 + 4) as f32, ((panel / 8) * 80 + 4) as f32);
                let color =
                    if panel % 3 == 0 { Color::from_rgb(40, 44, 58) } else { Color::from_rgba_unmultiplied(90, 140, 220, 96) };
                frame.fill_rect(Rect::from_min_size([x, y], [120.0, 60.0]), color, ClipRect::ALL).unwrap();
            }
        })
    });
    group.bench_function("glyphs frame", |b| {
        b.iter(|| {
            let mut frame = renderer.begin_frame(WIDTH, HEIGHT).unwrap();
            frame.clear(Color::from_rgb(17, 20, 28));
            for (rect, uv) in &glyphs {
                frame.textured_rect(*rect, *uv, atlas, tint, ClipRect::ALL).unwrap();
            }
        })
    });
    group.throughput(Throughput::Elements(1000));
    group.bench_function("offscreen rect call", |b| {
        b.iter(|| {
            let mut frame = renderer.begin_frame(WIDTH, HEIGHT).unwrap();
            for _ in 0..1000 {
                frame.fill_rect(Rect::from_min_max([700.0, 700.0], [701.0, 701.0]), Color::WHITE, ClipRect::ALL).unwrap();
            }
        })
    });
    group.throughput(Throughput::Elements(258));
    group.bench_function("fan vertices: Vertex::new", |b| {
        b.iter(|| {
            let vertices = fan_vertices();
            std::hint::black_box(vertices);
        })
    });
    group.throughput(Throughput::Elements(1));
    group.bench_function("fan mesh frame", |b| {
        b.iter(|| {
            let mut frame = renderer.begin_frame(WIDTH, HEIGHT).unwrap();
            frame.clear(Color::from_rgb(17, 20, 28));
            frame.mesh(Mesh { vertices: &vertices, indices: &indices, texture: None }, ClipRect::ALL).unwrap();
        })
    });
    group.finish();
}

fn runtime() -> Runtime {
    let plan = RuntimePlan::builder()
        .policy(RuntimePolicy::new().compat_global("@dream/raster", "raster").compat_global("@dream/soft-render", "soft"))
        .extension(RasterExtension)
        .extension(SoftRenderExtension)
        .finalize()
        .unwrap();
    let runtime = Runtime::from_plan(&plan).unwrap();
    // The atlas and the fan as Luau-side resources, built once.
    runtime
        .exec(&format!(
            "renderer = soft.renderer() \
             local atlas = buffer.create({ATLAS_WIDTH} * {ATLAS_HEIGHT} * 4) \
             for y = 0, {ATLAS_HEIGHT} - 1 do for x = 0, {ATLAS_WIDTH} - 1 do \
                 local cx, cy = x % {GLYPH_WIDTH}, y % {GLYPH_HEIGHT} \
                 local coverage = if cx == 0 or cy == 0 then 0 elseif cx == 1 or cy == 1 then 96 else 255 \
                 local at = (y * {ATLAS_WIDTH} + x) * 4 \
                 buffer.writeu8(atlas, at, coverage) buffer.writeu8(atlas, at + 1, coverage) \
                 buffer.writeu8(atlas, at + 2, coverage) buffer.writeu8(atlas, at + 3, coverage) \
             end end \
             texture = renderer:createTexture({ATLAS_WIDTH}, {ATLAS_HEIGHT}, atlas) \
             fanVertices = buffer.create(258 * soft.VERTEX_BYTES) \
             local function vertex(i, x, y, color) \
                 local o = i * soft.VERTEX_BYTES \
                 buffer.writef32(fanVertices, o, x) buffer.writef32(fanVertices, o + 4, y) \
                 buffer.writef32(fanVertices, o + 8, 0) buffer.writef32(fanVertices, o + 12, 0) \
                 buffer.writeu32(fanVertices, o + 16, raster.packed(color)) \
             end \
             vertex(0, 320, 240, raster.WHITE) \
             for step = 0, 256 do \
                 local angle = step / 256 * 2 * math.pi local shade = step % 256 \
                 vertex(step + 1, 320 + 200 * math.cos(angle), 240 + 180 * math.sin(angle), soft.premultiply(raster.rgba8(shade, 255 - shade, 128, 220))) \
             end \
             fanIndices = buffer.create(256 * 3 * 4) \
             for edge = 1, 256 do local o = (edge - 1) * 12 buffer.writeu32(fanIndices, o, 0) buffer.writeu32(fanIndices, o + 4, edge) buffer.writeu32(fanIndices, o + 8, edge + 1) end \
             glyphs = {{}} \
             local columns, rows = {ATLAS_WIDTH} // {GLYPH_WIDTH}, {ATLAS_HEIGHT} // {GLYPH_HEIGHT} \
             for line = 0, 39 do for column = 0, 79 do \
                 local glyph = (line * 31 + column * 7) % (columns * rows) \
                 local u, v = (glyph % columns) * {GLYPH_WIDTH}, (glyph // columns) * {GLYPH_HEIGHT} \
                 table.insert(glyphs, {{ \
                     vector.create(4 + column * ({GLYPH_WIDTH} + 1), 4 + line * ({GLYPH_HEIGHT} + 3)), \
                     vector.create(4 + column * ({GLYPH_WIDTH} + 1) + {GLYPH_WIDTH}, 4 + line * ({GLYPH_HEIGHT} + 3) + {GLYPH_HEIGHT}), \
                     vector.create((u + 0.5) / ({ATLAS_WIDTH} - 1), (v + 0.5) / ({ATLAS_HEIGHT} - 1)), \
                     vector.create((u + {GLYPH_WIDTH} - 0.5) / ({ATLAS_WIDTH} - 1), (v + {GLYPH_HEIGHT} - 0.5) / ({ATLAS_HEIGHT} - 1)) }}) \
             end end",
        ))
        .unwrap();
    runtime
}

/// Layer two: the same frames driven from Luau; layer three: single calls.
fn through_luau(c: &mut Criterion) {
    let runtime = runtime();
    let cases: [(&str, u64, &str); 8] = [
        (
            "panels frame",
            PANELS,
            "local frame = renderer:beginFrame(640, 480) frame:clear(background) \
             for panel = 0, 47 do \
                 local x, y = (panel % 8) * 80 + 4, (panel // 8) * 80 + 4 \
                 frame:rect(vector.create(x, y), vector.create(x + 120, y + 60), if panel % 3 == 0 then solid else glass, raster.CLIP_ALL) \
             end frame:finish()",
        ),
        (
            "glyphs frame",
            GLYPHS,
            "local frame = renderer:beginFrame(640, 480) frame:clear(background) \
             for _, g in glyphs do frame:image(g[1], g[2], g[3], g[4], texture, tint, raster.CLIP_ALL) end frame:finish()",
        ),
        (
            "fan mesh frame",
            1,
            "local frame = renderer:beginFrame(640, 480) frame:clear(background) \
             frame:mesh(fanVertices, fanIndices, nil, raster.CLIP_ALL) frame:finish()",
        ),
        ("rgba8 call", 1000, "for i = 1, 1000 do c = raster.rgba8(i % 256, 40, 40, 255) end"),
        (
            "offscreen rect call",
            1000,
            "local frame = renderer:beginFrame(640, 480) \
             for i = 1, 1000 do frame:rect(vector.create(700, 700), vector.create(701, 701), solid, raster.CLIP_ALL) end",
        ),
        ("readInto", 1, "renderer:readInto(output)"),
        (
            "fan vertices: 5 buffer writes",
            258,
            "local off = 0 \
             for step = 0, 257 do \
                 local angle = step / 256 * 2 * math.pi \
                 buffer.writef32(target, off, 320 + 200 * math.cos(angle)) buffer.writef32(target, off + 4, 240 + 180 * math.sin(angle)) \
                 buffer.writef32(target, off + 8, 0) buffer.writef32(target, off + 12, 0) \
                 buffer.writeu32(target, off + 16, raster.packed(tint)) off += 20 \
             end",
        ),
        (
            "fan vertices: Vertices:write",
            258,
            "local off = 0 \
             for step = 0, 257 do \
                 local angle = step / 256 * 2 * math.pi \
                 off = V:write(target, off, vector.create(320 + 200 * math.cos(angle), 240 + 180 * math.sin(angle)), vector.zero, tint) \
             end",
        ),
    ];
    runtime
        .exec(
            "background = raster.rgb8(17, 20, 28) solid = raster.rgb8(40, 44, 58) \
             glass = soft.premultiply(raster.rgba8(90, 140, 220, 96)) tint = raster.rgb8(220, 224, 230) \
             output = buffer.create(640 * 480 * 4) target = buffer.create(258 * soft.VERTEX_BYTES) V = soft.vertices()",
        )
        .unwrap();
    let functions: Vec<(&str, u64, Function)> = cases
        .iter()
        .map(|(name, per, body)| {
            (
                *name,
                *per,
                runtime
                    .load_function(&format!(
                        "return function() local renderer, raster, soft, glyphs, texture = renderer, raster, soft, glyphs, texture \
                         local background, solid, glass, tint, output = background, solid, glass, tint, output \
                         local fanVertices, fanIndices, target, V = fanVertices, fanIndices, target, V {body} return 0 end"
                    ))
                    .unwrap(),
            )
        })
        .collect();
    let mut group = c.benchmark_group("soft_render_luau");
    for (name, per, function) in &functions {
        group.throughput(Throughput::Elements(*per));
        let stack = runtime.stack();
        group.bench_function(*name, |b| b.iter(|| function.invoke::<f64, _>(&stack, ()).unwrap()));
    }
    group.finish();
}

/// The lowered writer, inside natively compiled code.
#[cfg(feature = "jit")]
fn lowered(c: &mut Criterion) {
    use l3i::extension::NativeCodePolicy;
    use l3i::native_code::NativeCodeMode;
    use l3i::runtime::{CallContext, MemoryCategory};
    use l3i::sandbox::{InstanceSpec, SandboxOptions};

    let policy = RuntimePolicy::new()
        .compat_global("@dream/raster", "raster")
        .compat_global("@dream/soft-render", "soft")
        .native_code(NativeCodePolicy { mode: NativeCodeMode::Eager, ..NativeCodePolicy::default() });
    let plan = RuntimePlan::builder().policy(policy).extension(RasterExtension).extension(SoftRenderExtension).finalize().unwrap();
    let runtime = Runtime::from_plan(&plan).unwrap();
    if !runtime.native_code().is_some_and(l3i::native_code::NativeCodeGen::is_available) {
        return;
    }
    let sandbox = runtime
        .sandbox(|_| {}, SandboxOptions { compile_options: runtime.compile_options(), ..SandboxOptions::default() })
        .unwrap();
    let loader = runtime.load_function("return function(name) error('module ' .. name .. ' not found') end").unwrap();
    let instance = sandbox
        .new_instance(&runtime, &InstanceSpec { name: "v", packages: &[], hidden_data: None, loader: &loader })
        .unwrap();
    let template = sandbox
        .load_template(
            &runtime,
            "bench.lua",
            "--!native
             local V: dream_soft_render_Vertices = soft.vertices()
             local target = buffer.create(258 * soft.VERTEX_BYTES)
             local tint = raster.rgb8(220, 224, 230)
             return function()
                 local off = 0
                 for step = 0, 257 do
                     local angle = step / 256 * 2 * math.pi
                     off = V:write(target, off, vector.create(320 + 200 * math.cos(angle), 240 + 180 * math.sin(angle)), vector.zero, tint)
                 end
                 return 0
             end",
        )
        .unwrap();
    let results = sandbox.run(&runtime, &template, &instance, CallContext { id: 1, category: MemoryCategory(0) }).unwrap();
    let function = Function::from_value(results.into_iter().next().unwrap()).unwrap();
    let mut group = c.benchmark_group("soft_render_luau");
    group.throughput(Throughput::Elements(258));
    let stack = runtime.stack();
    group.bench_function("fan vertices: Vertices:write (native lowered)", |b| {
        b.iter(|| function.invoke::<f64, _>(&stack, ()).unwrap())
    });
    group.finish();
}

#[cfg(not(feature = "jit"))]
fn lowered(_: &mut Criterion) {}

fn configure() -> Criterion {
    Criterion::default().warm_up_time(Duration::from_secs(1)).measurement_time(Duration::from_secs(3))
}

criterion_group! { name = benches; config = configure(); targets = native, through_luau, lowered }
criterion_main!(benches);
