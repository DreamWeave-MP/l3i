//! The `dream.soft_render` extension (`soft-render` feature): dream-soft-render's CPU
//! rasterizer as a small software rendering device for scripts.
//!
//! Luau describes raster work in native-shaped values, and Rust does it: rectangles, textured
//! rectangles, and triangle meshes from Luau buffers, drawn immediately in call order into a
//! renderer-owned RGBA8 surface. Colors are [`crate::raster::Color`] integers, which the
//! renderer reads as premultiplied (its rule, see `dream_soft_render::Color`); clip rectangles
//! are [`crate::raster::ClipRect`] integers. Nothing here is per-pixel and nothing is a table:
//! vertex data is a buffer of 20-byte vertices (`x, y, u, v` as f32 and `[r, g, b, a]`) and an
//! index buffer of `u32`, borrowed for the duration of one draw call and never kept.
//!
//! ```lua
//! local soft = require('@dream/soft-render')
//! local raster = require('@dream/raster')
//! local renderer = soft.renderer()
//! local frame = renderer:beginFrame(640, 480)
//! frame:clear(raster.rgb8(17, 20, 28))
//! frame:rect(vector.create(8, 8), vector.create(312, 48), raster.rgba8(80, 160, 255, 192), raster.CLIP_ALL)
//! local icon = renderer:createTexture(2, 2, pixels)          -- a buffer or string, w * h * 4 bytes
//! frame:image(vector.create(16, 16), vector.create(40, 40), vector.zero, vector.one, icon, raster.WHITE, raster.CLIP_ALL)
//! frame:mesh(vertexBuffer, indexBuffer, nil, raster.CLIP_ALL)
//! frame:finish()
//! renderer:readInto(output)                                   -- width * height * 4 bytes
//! ```
//!
//! Coordinates, coverage, blending, and sampling are the renderer's rules and are not restated
//! here; l3i binds them without changing them, so a scene drawn from Rust and the same scene
//! drawn from Luau produce identical bytes (`tests/soft_render.rs` checks by hash). Malformed
//! input is an error, never a quietly clipped draw: a vertex buffer whose length is not a
//! multiple of 20, an index past the vertices, a NaN corner, a freed texture, a frame used after
//! `finish()` or after the next `beginFrame`.
//!
//! Resources belong to the runtime: a `Texture` frees its storage when collected or on
//! `free()`, a `Renderer` owns the surface and the texture store. A `Frame` is a token for one
//! frame; the renderer draws immediately, so the token holds no commands.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use dream_soft_render::{SoftwareRenderer, TextureId};

use crate::convert::{BufferView, BytesView, Vector3};
use crate::direct::field::{DirectField, FieldValue};
use crate::error::{Error, Result};
use crate::extension::{Extension, ExtensionDescriptor, InstallContext, TagPolicy};
use crate::packed::Packed;
use crate::raster::{ClipRect, Color};
use crate::source::CompileConstant;
use crate::userdata::{Owned, Userdata};

/// The extension id.
pub const EXTENSION_ID: &str = "dream.soft_render";
/// The module path.
pub const MODULE: &str = "@dream/soft-render";
/// The size of one vertex in a vertex buffer.
pub const VERTEX_BYTES: usize = std::mem::size_of::<dream_soft_render::Vertex>();
/// The size of one index in an index buffer.
pub const INDEX_BYTES: usize = std::mem::size_of::<u32>();

fn raster_error(error: dream_soft_render::Error) -> Error {
    Error::runtime(format!("dream.soft_render: {error}"))
}

fn color(c: Packed<Color>) -> dream_soft_render::Color {
    dream_soft_render::Color::from_packed(c.0.packed())
}

/// A packed clip rectangle as the renderer's: a max field at the packed limit means "to the
/// edge of the surface", whatever the surface size.
fn clip(c: Packed<ClipRect>) -> dream_soft_render::ClipRect {
    let c = c.0;
    let edge = |value: u32| if value == ClipRect::MAX_COORD { u32::MAX } else { value };
    dream_soft_render::ClipRect::new(c.min_x, c.min_y, edge(c.max_x), edge(c.max_y))
}

/// A 2D point from the native vector: `x` and `y`, `z` ignored.
fn xy(v: Vector3) -> [f32; 2] {
    [v.x, v.y]
}

fn rect(min: Vector3, max: Vector3) -> dream_soft_render::Rect {
    dream_soft_render::Rect::from_min_max(xy(min), xy(max))
}

fn dimension(name: &str, value: i64) -> Result<usize> {
    usize::try_from(value).map_err(|_| Error::runtime(format!("dream.soft_render: {name} {value} is negative")))
}

/// Reinterprets a buffer's bytes as vertices: the length must be a multiple of
/// [`VERTEX_BYTES`] and the storage 4-aligned (Luau buffers are 8-aligned; this is checked
/// anyway). Every bit pattern is a vertex, so no value is inspected here; the renderer rejects
/// non-finite coordinates itself.
fn vertices(bytes: &[u8]) -> Result<&[dream_soft_render::Vertex]> {
    if !bytes.len().is_multiple_of(VERTEX_BYTES) {
        return Err(Error::runtime(format!(
            "dream.soft_render: vertex buffer length {} is not a multiple of {VERTEX_BYTES} bytes",
            bytes.len()
        )));
    }
    if bytes.as_ptr().align_offset(std::mem::align_of::<dream_soft_render::Vertex>()) != 0 {
        return Err(Error::runtime("dream.soft_render: vertex buffer storage is not 4-byte aligned"));
    }
    // SAFETY: `Vertex` is `#[repr(C)]` of four f32 and four u8 with no padding (size asserted
    // by the crate), every bit pattern is valid, the length is a multiple of its size, and the
    // alignment was checked above. The slice borrows `bytes` and lives no longer.
    #[allow(clippy::cast_ptr_alignment)]
    let vertices = unsafe {
        std::slice::from_raw_parts(bytes.as_ptr().cast::<dream_soft_render::Vertex>(), bytes.len() / VERTEX_BYTES)
    };
    Ok(vertices)
}

/// Reinterprets a buffer's bytes as `u32` indices (native byte order).
fn indices(bytes: &[u8]) -> Result<&[u32]> {
    if !bytes.len().is_multiple_of(INDEX_BYTES) {
        return Err(Error::runtime(format!(
            "dream.soft_render: index buffer length {} is not a multiple of {INDEX_BYTES} bytes",
            bytes.len()
        )));
    }
    if bytes.as_ptr().align_offset(std::mem::align_of::<u32>()) != 0 {
        return Err(Error::runtime("dream.soft_render: index buffer storage is not 4-byte aligned"));
    }
    // SAFETY: as `vertices`; every bit pattern is a u32.
    #[allow(clippy::cast_ptr_alignment)]
    let indices = unsafe { std::slice::from_raw_parts(bytes.as_ptr().cast::<u32>(), bytes.len() / INDEX_BYTES) };
    Ok(indices)
}

/// The renderer shared by a `Renderer` handle, its frames, and its textures.
struct State {
    renderer: RefCell<SoftwareRenderer>,
    /// Bumped by `beginFrame` and `finish`; a `Frame` is live while it matches.
    generation: Cell<u64>,
}

impl State {
    fn borrow_mut(&self) -> Result<std::cell::RefMut<'_, SoftwareRenderer>> {
        self.renderer
            .try_borrow_mut()
            .map_err(|_| Error::runtime("dream.soft_render: the renderer is already in use by this call"))
    }
}

/// A software renderer as scripts see it (`dream.soft_render.Renderer`).
pub struct Renderer {
    state: Rc<State>,
}

// SAFETY: plain Rust state; dropping it frees the surface and textures without the Lua API.
unsafe impl Userdata for Renderer {
    const NAME: &'static str = "dream.soft_render.Renderer";
}

impl Default for Renderer {
    fn default() -> Self {
        Self::new()
    }
}

impl Renderer {
    /// A renderer with an empty surface and no textures.
    pub fn new() -> Renderer {
        Renderer { state: Rc::new(State { renderer: RefCell::new(SoftwareRenderer::default()), generation: Cell::new(0) }) }
    }

    /// Runs `body` on the underlying renderer, for host code (reading the surface natively,
    /// registering textures from Rust). Fails if a script call on this renderer is in progress.
    pub fn with<R>(&self, body: impl FnOnce(&mut SoftwareRenderer) -> R) -> Result<R> {
        let mut renderer = self.state.borrow_mut()?;
        Ok(body(&mut renderer))
    }

    fn begin_frame(&self, width: i64, height: i64) -> Result<Owned<Frame>> {
        let (width, height) = (dimension("width", width)?, dimension("height", height)?);
        self.state.borrow_mut()?.begin_frame(width, height).map_err(raster_error)?;
        let generation = self.state.generation.get() + 1;
        self.state.generation.set(generation);
        Ok(Owned(Frame { state: Rc::clone(&self.state), generation, width, height }))
    }

    fn create_texture(&self, width: i64, height: i64, pixels: BytesView<'_>) -> Result<Owned<Texture>> {
        let (width, height) = (dimension("width", width)?, dimension("height", height)?);
        let mut renderer = self.state.borrow_mut()?;
        let id = pixels.with_bytes(|bytes| renderer.create_texture(width, height, bytes)).map_err(raster_error)?;
        drop(renderer);
        Ok(Owned(Texture { state: Rc::clone(&self.state), id, width, height, freed: Cell::new(false) }))
    }

    /// Copies the surface's pixels into `buffer` at `offset` (`width * height * 4` bytes).
    fn read_into(&self, buffer: BufferView<'_>, offset: Option<i64>) -> Result<f64> {
        let offset = dimension("offset", offset.unwrap_or(0))?;
        let renderer = self.state.borrow_mut()?;
        let pixels = &renderer.surface().pixels;
        buffer.write(offset, pixels)?;
        Ok(pixels.len() as f64)
    }

    fn surface_size(&self) -> (usize, usize) {
        match self.state.renderer.try_borrow() {
            Ok(renderer) => (renderer.surface().width, renderer.surface().height),
            Err(_) => (0, 0),
        }
    }
}

/// One frame of drawing (`dream.soft_render.Frame`), from `Renderer:beginFrame`. Draw calls
/// rasterize immediately; the frame is a token that goes stale on `finish()` or the next
/// `beginFrame`.
pub struct Frame {
    state: Rc<State>,
    generation: u64,
    width: usize,
    height: usize,
}

// SAFETY: as `Renderer`.
unsafe impl Userdata for Frame {
    const NAME: &'static str = "dream.soft_render.Frame";
}

impl Frame {
    fn live(&self) -> Result<()> {
        if self.generation != self.state.generation.get() {
            return Err(Error::runtime("dream.soft_render: this frame is finished; call beginFrame for a new one"));
        }
        Ok(())
    }

    /// Re-enters the renderer's current surface for one draw. `begin_frame` at the size this
    /// frame opened with compares sizes and does nothing else, so no pixel is touched.
    fn draw<R>(&self, body: impl FnOnce(&mut dream_soft_render::Frame<'_>) -> Result<R>) -> Result<R> {
        self.live()?;
        let mut renderer = self.state.borrow_mut()?;
        let mut frame = renderer.begin_frame(self.width, self.height).map_err(raster_error)?;
        body(&mut frame)
    }

    fn clear(&self, color: Packed<Color>) -> Result<()> {
        self.draw(|frame| {
            frame.clear(self::color(color));
            Ok(())
        })
    }

    fn rect(&self, min: Vector3, max: Vector3, color: Packed<Color>, clip: Packed<ClipRect>) -> Result<()> {
        self.draw(|frame| frame.fill_rect(rect(min, max), self::color(color), self::clip(clip)).map_err(raster_error))
    }

    #[allow(clippy::too_many_arguments)]
    fn image(
        &self,
        min: Vector3,
        max: Vector3,
        uv_min: Vector3,
        uv_max: Vector3,
        texture: &Texture,
        tint: Packed<Color>,
        clip: Packed<ClipRect>,
    ) -> Result<()> {
        let id = texture.id()?;
        self.draw(|frame| {
            frame
                .textured_rect(rect(min, max), rect(uv_min, uv_max), id, self::color(tint), self::clip(clip))
                .map_err(raster_error)
        })
    }

    fn mesh(
        &self,
        vertices: BufferView<'_>,
        indices: BufferView<'_>,
        texture: Option<&Texture>,
        clip: Packed<ClipRect>,
    ) -> Result<()> {
        let texture = texture.map(Texture::id).transpose()?;
        vertices.with_bytes(|vertex_bytes| {
            indices.with_bytes(|index_bytes| {
                let mesh = dream_soft_render::Mesh {
                    vertices: self::vertices(vertex_bytes)?,
                    indices: self::indices(index_bytes)?,
                    texture,
                };
                self.draw(|frame| frame.mesh(mesh, self::clip(clip)).map_err(raster_error))
            })
        })
    }

    fn finish(&self) -> Result<()> {
        self.live()?;
        self.state.generation.set(self.generation + 1);
        Ok(())
    }
}

/// A texture in a renderer's store (`dream.soft_render.Texture`). Freed on `free()` or when
/// collected.
pub struct Texture {
    state: Rc<State>,
    id: TextureId,
    width: usize,
    height: usize,
    freed: Cell<bool>,
}

// SAFETY: the destructor frees the texture through the Rust renderer, never the Lua API.
unsafe impl Userdata for Texture {
    const NAME: &'static str = "dream.soft_render.Texture";
}

impl Texture {
    fn id(&self) -> Result<TextureId> {
        if self.freed.get() {
            return Err(Error::runtime("dream.soft_render: the texture was freed"));
        }
        Ok(self.id)
    }

    fn update(&self, x: i64, y: i64, width: i64, height: i64, pixels: BytesView<'_>) -> Result<()> {
        let id = self.id()?;
        let (x, y) = (dimension("x", x)?, dimension("y", y)?);
        let (width, height) = (dimension("width", width)?, dimension("height", height)?);
        let mut renderer = self.state.borrow_mut()?;
        pixels.with_bytes(|bytes| renderer.update_texture(id, x, y, width, height, bytes)).map_err(raster_error)
    }

    fn free(&self) -> Result<()> {
        let id = self.id()?;
        self.freed.set(true);
        self.state.borrow_mut()?.free_texture(id).map_err(raster_error)
    }
}

impl Drop for Texture {
    fn drop(&mut self) {
        if !self.freed.get()
            && let Ok(mut renderer) = self.state.renderer.try_borrow_mut()
        {
            // A handle the renderer no longer knows is already gone; nothing to report from a
            // destructor.
            let _ = renderer.free_texture(self.id);
        }
    }
}

macro_rules! size_field {
    ($name:ident, $ty:ty, $get:expr) => {
        struct $name;
        impl DirectField<$ty> for $name {
            fn get(value: &$ty) -> FieldValue {
                FieldValue::Number($get(value) as f64)
            }
        }
    };
}

size_field!(RendererWidth, Renderer, |r: &Renderer| r.surface_size().0);
size_field!(RendererHeight, Renderer, |r: &Renderer| r.surface_size().1);
size_field!(FrameWidth, Frame, |f: &Frame| f.width);
size_field!(FrameHeight, Frame, |f: &Frame| f.height);
size_field!(TextureWidth, Texture, |t: &Texture| t.width);
size_field!(TextureHeight, Texture, |t: &Texture| t.height);

/// The `dream.soft_render` extension; requires `dream.raster` in the same plan.
pub struct SoftRenderExtension;

impl Extension for SoftRenderExtension {
    fn id(&self) -> &'static str {
        EXTENSION_ID
    }

    fn describe(&self, d: &mut ExtensionDescriptor) -> Result<()> {
        d.requires(crate::raster::EXTENSION_ID);
        let renderer = d.userdata::<Renderer>("dream.soft_render.Renderer");
        renderer.tag(TagPolicy::Preferred).doc("A CPU rasterizer: one surface, a texture store.");
        renderer.method("beginFrame").signature("(self, width: number, height: number): dream_soft_render_Frame");
        renderer
            .method("createTexture")
            .signature("(self, width: number, height: number, pixels: buffer | string): dream_soft_render_Texture");
        renderer.method("readInto").signature("(self, buffer: buffer, offset: number?): number");
        renderer.field("width").signature("number");
        renderer.field("height").signature("number");

        let frame = d.userdata::<Frame>("dream.soft_render.Frame");
        frame.tag(TagPolicy::Required).doc("One frame of immediate drawing; stale after finish().");
        frame.method("clear").signature("(self, color: integer)");
        frame.method("rect").signature("(self, min: vector, max: vector, color: integer, clip: integer)");
        frame.method("image").signature(
            "(self, min: vector, max: vector, uvMin: vector, uvMax: vector, texture: dream_soft_render_Texture, tint: integer, clip: integer)",
        );
        frame
            .method("mesh")
            .signature("(self, vertices: buffer, indices: buffer, texture: dream_soft_render_Texture?, clip: integer)");
        frame.method("finish").signature("(self)");
        frame.field("width").signature("number");
        frame.field("height").signature("number");

        let texture = d.userdata::<Texture>("dream.soft_render.Texture");
        texture.tag(TagPolicy::Preferred).doc("Premultiplied RGBA8 pixels in the renderer's store.");
        texture
            .method("update")
            .signature("(self, x: number, y: number, width: number, height: number, pixels: buffer | string)");
        texture.method("free").signature("(self)");
        texture.field("width").signature("number");
        texture.field("height").signature("number");

        d.module(MODULE).doc("A software rendering device.");
        d.memory_category(EXTENSION_ID);
        Ok(())
    }

    fn install(&self, cx: &mut InstallContext<'_>) -> Result<()> {
        cx.userdata::<Renderer>("dream.soft_render.Renderer")?
            .method("beginFrame", |r: &Renderer, width: i64, height: i64| r.begin_frame(width, height))?
            .method("createTexture", |r: &Renderer, width: i64, height: i64, pixels: BytesView| {
                r.create_texture(width, height, pixels)
            })?
            .method("readInto", |r: &Renderer, buffer: BufferView, offset: Option<i64>| r.read_into(buffer, offset))?
            .field::<RendererWidth>("width")?
            .field::<RendererHeight>("height")?;
        cx.userdata::<Frame>("dream.soft_render.Frame")?
            .method("clear", |f: &Frame, color: Packed<Color>| f.clear(color))?
            .method("rect", |f: &Frame, min: Vector3, max: Vector3, color: Packed<Color>, clip: Packed<ClipRect>| {
                f.rect(min, max, color, clip)
            })?
            .method(
                "image",
                |f: &Frame,
                 min: Vector3,
                 max: Vector3,
                 uv_min: Vector3,
                 uv_max: Vector3,
                 texture: &Texture,
                 tint: Packed<Color>,
                 clip: Packed<ClipRect>| f.image(min, max, uv_min, uv_max, texture, tint, clip),
            )?
            .method(
                "mesh",
                |f: &Frame, vertices: BufferView, indices: BufferView, texture: Option<&Texture>, clip: Packed<ClipRect>| {
                    f.mesh(vertices, indices, texture, clip)
                },
            )?
            .method("finish", |f: &Frame| f.finish())?
            .field::<FrameWidth>("width")?
            .field::<FrameHeight>("height")?;
        cx.userdata::<Texture>("dream.soft_render.Texture")?
            .method("update", |t: &Texture, x: i64, y: i64, width: i64, height: i64, pixels: BytesView| {
                t.update(x, y, width, height, pixels)
            })?
            .method("free", |t: &Texture| t.free())?
            .field::<TextureWidth>("width")?
            .field::<TextureHeight>("height")?;
        let mut module = cx.module(MODULE)?;
        module
            .function("renderer", || Owned(Renderer::new()))?
            .function("premultiply", |c: Packed<Color>| {
                let c = c.0;
                Color::from_packed(dream_soft_render::Color::from_rgba_unmultiplied(c.r, c.g, c.b, c.a).to_packed())
                    .pack()
            })?
            .constant("MAX_SURFACE_PIXELS", CompileConstant::Number(dream_soft_render::MAX_SURFACE_PIXELS as f64))?
            .constant("MAX_TEXTURE_BYTES", CompileConstant::Number(dream_soft_render::MAX_TEXTURE_BYTES as f64))?
            .constant("VERTEX_BYTES", CompileConstant::Number(VERTEX_BYTES as f64))?;
        module.finish()?;
        Ok(())
    }
}
