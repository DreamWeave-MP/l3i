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

use crate::convert::{Exact, BufferView, BytesView, Vector3};
use crate::direct::field::{DirectField, FieldValue};
use crate::error::{Error, Result};
use crate::extension::{Extension, ExtensionDescriptor, TagPolicy};
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

fn dimension(name: &str, value: Exact<i64>) -> Result<usize> {
    usize::try_from(value.0).map_err(|_| Error::runtime(format!("dream.soft_render: {name} {} is negative", value.0)))
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
    /// Textures whose handles were collected while the renderer was borrowed; freed on the
    /// next borrow, so a transient conflict never leaks a texture.
    pending_frees: RefCell<Vec<TextureId>>,
}

impl State {
    fn borrow_mut(&self) -> Result<std::cell::RefMut<'_, SoftwareRenderer>> {
        let mut renderer = self
            .renderer
            .try_borrow_mut()
            .map_err(|_| Error::runtime("dream.soft_render: the renderer is already in use by this call"))?;
        for id in self.pending_frees.borrow_mut().drain(..) {
            let _ = renderer.free_texture(id);
        }
        Ok(renderer)
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
        Renderer {
            state: Rc::new(State {
                renderer: RefCell::new(SoftwareRenderer::default()),
                generation: Cell::new(0),
                pending_frees: RefCell::new(Vec::new()),
            }),
        }
    }

    /// Runs `body` on the underlying renderer, for host code (reading the surface natively,
    /// registering textures from Rust). Fails if a script call on this renderer is in progress.
    pub fn with<R>(&self, body: impl FnOnce(&mut SoftwareRenderer) -> R) -> Result<R> {
        let mut renderer = self.state.borrow_mut()?;
        Ok(body(&mut renderer))
    }

    fn begin_frame(&self, width: Exact<i64>, height: Exact<i64>) -> Result<Owned<Frame>> {
        let (width, height) = (dimension("width", width)?, dimension("height", height)?);
        self.state.borrow_mut()?.begin_frame(width, height).map_err(raster_error)?;
        let generation = self
            .state
            .generation
            .get()
            .checked_add(1)
            .ok_or_else(|| Error::runtime("dream.soft_render: frame generations exhausted"))?;
        self.state.generation.set(generation);
        Ok(Owned(Frame { state: Rc::clone(&self.state), generation, width, height }))
    }

    fn create_texture(&self, width: Exact<i64>, height: Exact<i64>, pixels: BytesView<'_>) -> Result<Owned<Texture>> {
        let (width, height) = (dimension("width", width)?, dimension("height", height)?);
        let mut renderer = self.state.borrow_mut()?;
        // SAFETY: the renderer is a pure Rust crate holding no Lua handle, so nothing writes the
        // buffer while it reads the slice; the slice ends with this statement.
        let bytes = unsafe { pixels.bytes_unchecked() };
        let id = renderer.create_texture(width, height, bytes).map_err(raster_error)?;
        drop(renderer);
        Ok(Owned(Texture { state: Rc::clone(&self.state), id, width, height, freed: Cell::new(false) }))
    }

    /// Copies the surface's pixels into `buffer` at `offset` (`width * height * 4` bytes).
    fn read_into(&self, buffer: BufferView<'_>, offset: Option<Exact<i64>>) -> Result<f64> {
        let offset = dimension("offset", offset.unwrap_or(Exact(0)))?;
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
        // SAFETY: both slices are read-only and live only through `draw`, which hands them to
        // the renderer, a pure Rust crate holding no Lua handle: nothing writes either buffer
        // meanwhile, and the same buffer in both parameters is two shared slices.
        let (vertex_bytes, index_bytes) = unsafe { (vertices.bytes_unchecked(), indices.bytes_unchecked()) };
        let mesh = dream_soft_render::Mesh {
            vertices: self::vertices(vertex_bytes)?,
            indices: self::indices(index_bytes)?,
            texture,
        };
        self.draw(|frame| frame.mesh(mesh, self::clip(clip)).map_err(raster_error))
    }

    fn finish(&self) -> Result<()> {
        self.live()?;
        self.state.generation.set(self.generation.saturating_add(1));
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

    fn update(&self, x: Exact<i64>, y: Exact<i64>, width: Exact<i64>, height: Exact<i64>, pixels: BytesView<'_>) -> Result<()> {
        let id = self.id()?;
        let (x, y) = (dimension("x", x)?, dimension("y", y)?);
        let (width, height) = (dimension("width", width)?, dimension("height", height)?);
        let mut renderer = self.state.borrow_mut()?;
        // SAFETY: as `create_texture`: the renderer holds no Lua handle and the slice ends here.
        let bytes = unsafe { pixels.bytes_unchecked() };
        renderer.update_texture(id, x, y, width, height, bytes).map_err(raster_error)
    }

    /// Frees the texture; the handle is marked freed only once the renderer has let go, so a
    /// failed attempt (the renderer busy in this call) can be retried and `Drop` still cleans up.
    fn free(&self) -> Result<()> {
        let id = self.id()?;
        self.state.borrow_mut()?.free_texture(id).map_err(raster_error)?;
        self.freed.set(true);
        Ok(())
    }
}

impl Drop for Texture {
    fn drop(&mut self) {
        if self.freed.get() {
            return;
        }
        match self.state.renderer.try_borrow_mut() {
            // A handle the renderer no longer knows is already gone; nothing to report from a
            // destructor.
            Ok(mut renderer) => {
                let _ = renderer.free_texture(self.id);
            }
            // Collected while the renderer is borrowed: freed on its next use.
            Err(_) => self.state.pending_frees.borrow_mut().push(self.id),
        }
    }
}

/// The vertex writer (`dream.soft_render.Vertices`, from `soft.vertices()`): one call packs
/// a vertex into a buffer with a single bounds check, `V:write(buffer, offset, pos, uv,
/// color)`, and returns the next offset. With `jit` the call lowers to native stores
/// ([`lowering::VertexWriter`]).
#[derive(Clone, Copy, Debug, Default)]
pub struct Vertices;

// SAFETY: no payload, no Lua references.
unsafe impl Userdata for Vertices {
    const NAME: &'static str = "dream.soft_render.Vertices";
}

/// The interpreter path of `Vertices:write`, and the oracle its lowering must match. The
/// offset truncates toward zero like the buffer library's; anything outside the buffer is the
/// library's "buffer access out of bounds".
pub fn write_vertex(buffer: BufferView<'_>, offset: f64, pos: Vector3, uv: Vector3, color: Packed<Color>) -> Result<f64> {
    let offset = if offset.is_finite() && offset >= 0.0 && offset < f64::from(i32::MAX) {
        offset.trunc() as usize
    } else {
        return Err(Error::runtime("buffer access out of bounds"));
    };
    let mut bytes = [0u8; VERTEX_BYTES];
    bytes[0..4].copy_from_slice(&pos.x.to_ne_bytes());
    bytes[4..8].copy_from_slice(&pos.y.to_ne_bytes());
    bytes[8..12].copy_from_slice(&uv.x.to_ne_bytes());
    bytes[12..16].copy_from_slice(&uv.y.to_ne_bytes());
    bytes[16..20].copy_from_slice(&color.0.to_array());
    buffer.write(offset, &bytes)?;
    Ok((offset + VERTEX_BYTES) as f64)
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
        let mut renderer = d.userdata::<Renderer>("dream.soft_render.Renderer");
        renderer.tag(TagPolicy::Preferred).doc("A CPU rasterizer: one surface, a texture store.");
        renderer
            .method("beginFrame", |r: &Renderer, width: Exact<i64>, height: Exact<i64>| r.begin_frame(width, height))
            .signature("(self, width: number, height: number): dream_soft_render_Frame");
        renderer
            .method("createTexture", |r: &Renderer, width: Exact<i64>, height: Exact<i64>, pixels: BytesView| {
                r.create_texture(width, height, pixels)
            })
            .signature("(self, width: number, height: number, pixels: buffer | string): dream_soft_render_Texture");
        renderer
            .method("readInto", |r: &Renderer, buffer: BufferView, offset: Option<Exact<i64>>| r.read_into(buffer, offset))
            .signature("(self, buffer: buffer, offset: number?): number");
        renderer.field::<RendererWidth>("width").signature("number");
        renderer.field::<RendererHeight>("height").signature("number");

        let mut frame = d.userdata::<Frame>("dream.soft_render.Frame");
        frame.tag(TagPolicy::Required).doc("One frame of immediate drawing; stale after finish().");
        frame.method("clear", |f: &Frame, color: Packed<Color>| f.clear(color)).signature("(self, color: integer)");
        frame
            .method("rect", |f: &Frame, min: Vector3, max: Vector3, color: Packed<Color>, clip: Packed<ClipRect>| {
                f.rect(min, max, color, clip)
            })
            .signature("(self, min: vector, max: vector, color: integer, clip: integer)");
        frame
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
            )
            .signature(
                "(self, min: vector, max: vector, uvMin: vector, uvMax: vector, texture: dream_soft_render_Texture, tint: integer, clip: integer)",
            );
        frame
            .method(
                "mesh",
                |f: &Frame, vertices: BufferView, indices: BufferView, texture: Option<&Texture>, clip: Packed<ClipRect>| {
                    f.mesh(vertices, indices, texture, clip)
                },
            )
            .signature("(self, vertices: buffer, indices: buffer, texture: dream_soft_render_Texture?, clip: integer)");
        frame.method("finish", |f: &Frame| f.finish()).signature("(self)");
        frame.field::<FrameWidth>("width").signature("number");
        frame.field::<FrameHeight>("height").signature("number");

        let mut texture = d.userdata::<Texture>("dream.soft_render.Texture");
        texture.tag(TagPolicy::Preferred).doc("Premultiplied RGBA8 pixels in the renderer's store.");
        texture
            .method("update", |t: &Texture, x: Exact<i64>, y: Exact<i64>, width: Exact<i64>, height: Exact<i64>, pixels: BytesView| {
                t.update(x, y, width, height, pixels)
            })
            .signature("(self, x: number, y: number, width: number, height: number, pixels: buffer | string)");
        texture.method("free", |t: &Texture| t.free()).signature("(self)");
        texture.field::<TextureWidth>("width").signature("number");
        texture.field::<TextureHeight>("height").signature("number");

        let mut vertices = d.userdata::<Vertices>("dream.soft_render.Vertices");
        vertices
            .tag(TagPolicy::Required)
            .compiler_type(crate::extension::CompilerTypePolicy::Required)
            .doc("Packs vertices into buffers; natively lowered under jit.");
        vertices
            .method(
                "write",
                |_: &Vertices, buffer: BufferView, offset: f64, pos: Vector3, uv: Vector3, color: Packed<Color>| {
                    write_vertex(buffer, offset, pos, uv, color)
                },
            )
            .signature("(self, buffer: buffer, offset: number, pos: vector, uv: vector, color: integer): number");
        #[cfg(feature = "jit")]
        d.native_hooks(lowering::VertexWriter);

        d.module(MODULE)
            .doc("A software rendering device.")
            .function("renderer", || Owned(Renderer::new())).signature("() -> dream_soft_render_Renderer")
            .function("vertices", || Owned(Vertices)).signature("() -> dream_soft_render_Vertices")
            .function("premultiply", |c: Packed<Color>| {
                let c = c.0;
                Color::from_packed(dream_soft_render::Color::from_rgba_unmultiplied(c.r, c.g, c.b, c.a).to_packed())
                    .pack()
            }).signature("(color: integer) -> integer")
            .constant("MAX_SURFACE_PIXELS", CompileConstant::Number(dream_soft_render::MAX_SURFACE_PIXELS as f64))
            .constant("MAX_TEXTURE_BYTES", CompileConstant::Number(dream_soft_render::MAX_TEXTURE_BYTES as f64))
            .constant("VERTEX_BYTES", CompileConstant::Number(VERTEX_BYTES as f64));
        d.memory_category(EXTENSION_ID);
        Ok(())
    }
}

/// Native lowering of `Vertices:write` (`jit`): one buffer bounds check, four f32 stores from
/// the two vectors, and the color's four bytes as two 16-bit stores (the IR has no 64-to-32-bit
/// integer narrowing, and a 32-bit store through a double would lose bit 31). A wrong tag, a
/// color of another packed kind, or an offset outside the buffer exits to the interpreter,
/// whose bound method reports the error.
#[cfg(feature = "jit")]
pub mod lowering {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::{VERTEX_BYTES, Vertices};
    use crate::native_code::hooks::{NamecallSite, NativeCodeHooks, NativeContext};
    use crate::native_code::ir::{IrBuilder, IrCmd, IrCondition, IrOp, bytecode_type};
    use crate::packed::PackedScalar;
    use crate::raw::ffi::{LUA_TBUFFER, LUA_TINTEGER, LUA_TNUMBER, LUA_TVECTOR};
    use crate::raster::Color;

    static LOWERED: AtomicUsize = AtomicUsize::new(0);

    /// How many call sites the hook has lowered in this process (a diagnostic for tests).
    #[doc(hidden)]
    pub fn lowered_sites() -> usize {
        LOWERED.load(Ordering::Relaxed)
    }

    /// The hook set; [`super::SoftRenderExtension`] registers it.
    pub struct VertexWriter;

    fn write_f32_pair(build: &mut IrBuilder<'_>, buffer: IrOp, offset: IrOp, at: i32, reg: IrOp, tag: IrOp) {
        let vector = build.inst(IrCmd::LOAD_TVALUE, &[reg]);
        for lane in 0..2i32 {
            let index = build.const_int(lane);
            let value = build.inst(IrCmd::EXTRACT_VEC, &[vector, index]);
            let step = build.const_int(at + lane * 4);
            let destination = build.inst(IrCmd::ADD_INT, &[offset, step]);
            build.inst(IrCmd::BUFFER_WRITEF32, &[buffer, destination, value, tag]);
        }
    }

    impl NativeCodeHooks for VertexWriter {
        fn userdata_namecall_type(&self, context: &NativeContext<'_>, userdata_type: u8, member: &str) -> u8 {
            if context.userdata_type_of::<Vertices>() == Some(userdata_type) && member == "write" {
                bytecode_type::NUMBER
            } else {
                bytecode_type::ANY
            }
        }

        fn userdata_namecall(
            &self,
            context: &NativeContext<'_>,
            build: &mut IrBuilder<'_>,
            userdata_type: u8,
            member: &str,
            site: NamecallSite,
        ) -> bool {
            if context.userdata_type_of::<Vertices>() != Some(userdata_type)
                || member != "write"
                || site.params != 6
                || !matches!(site.results, 0 | 1)
            {
                return false;
            }
            let Some(tag) = context.tag_of::<Vertices>() else { return false };
            let exit = build.vm_exit(site.pcpos);
            let receiver = build.vm_reg(site.source_reg);
            let pointer = build.inst(IrCmd::LOAD_POINTER, &[receiver]);
            let tag = build.const_int(i32::from(tag));
            build.inst(IrCmd::CHECK_USERDATA_TAG, &[pointer, tag, exit]);
            // ra + 2 onward: buffer, offset, pos, uv, color.
            let buffer_reg = build.vm_reg(site.arg_res_reg + 2);
            let offset_reg = build.vm_reg(site.arg_res_reg + 3);
            let pos_reg = build.vm_reg(site.arg_res_reg + 4);
            let uv_reg = build.vm_reg(site.arg_res_reg + 5);
            let color_reg = build.vm_reg(site.arg_res_reg + 6);
            build.load_and_check_tag(buffer_reg, LUA_TBUFFER as u8, exit);
            build.load_and_check_tag(offset_reg, LUA_TNUMBER as u8, exit);
            build.load_and_check_tag(pos_reg, LUA_TVECTOR as u8, exit);
            build.load_and_check_tag(uv_reg, LUA_TVECTOR as u8, exit);
            build.load_and_check_tag(color_reg, LUA_TINTEGER as u8, exit);

            let buffer = build.inst(IrCmd::LOAD_POINTER, &[buffer_reg]);
            let offset_number = build.inst(IrCmd::LOAD_DOUBLE, &[offset_reg]);
            let offset = build.inst(IrCmd::NUM_TO_INT, &[offset_number]);
            let zero = build.const_int(0);
            let size = build.const_int(VERTEX_BYTES as i32);
            build.inst(IrCmd::CHECK_BUFFER_LEN, &[buffer, offset, zero, size, offset_number, exit]);

            let bits = build.inst(IrCmd::LOAD_INT64, &[color_reg]);
            let sixty = build.const_int64(60);
            let fifteen = build.const_int64(15);
            let kind = build.inst(IrCmd::BITRSHIFT_INT64, &[bits, sixty]);
            let kind = build.inst(IrCmd::BITAND_INT64, &[kind, fifteen]);
            let expected = build.const_int64(i64::from(<Color as PackedScalar>::KIND));
            let equal = build.cond(IrCondition::Equal);
            build.inst(IrCmd::CHECK_CMP_INT64, &[kind, expected, equal, exit]);

            let buffer_tag = build.const_tag(LUA_TBUFFER as u8);
            write_f32_pair(build, buffer, offset, 0, pos_reg, buffer_tag);
            write_f32_pair(build, buffer, offset, 8, uv_reg, buffer_tag);
            let mask = build.const_int64(0xFFFF);
            let sixteen = build.const_int64(16);
            let low = build.inst(IrCmd::BITAND_INT64, &[bits, mask]);
            let high = build.inst(IrCmd::BITRSHIFT_INT64, &[bits, sixteen]);
            let high = build.inst(IrCmd::BITAND_INT64, &[high, mask]);
            for (half, at) in [(low, 16), (high, 18)] {
                let number = build.inst(IrCmd::INT64_TO_NUM, &[half]);
                let value = build.inst(IrCmd::NUM_TO_INT, &[number]);
                let step = build.const_int(at);
                let destination = build.inst(IrCmd::ADD_INT, &[offset, step]);
                build.inst(IrCmd::BUFFER_WRITEI16, &[buffer, destination, value, buffer_tag]);
            }
            if site.results == 1 {
                let stride = build.const_double(VERTEX_BYTES as f64);
                let next = build.inst(IrCmd::ADD_NUM, &[offset_number, stride]);
                let result = build.vm_reg(site.arg_res_reg);
                build.inst(IrCmd::STORE_DOUBLE, &[result, next]);
                let number_tag = build.const_tag(LUA_TNUMBER as u8);
                build.inst(IrCmd::STORE_TAG, &[result, number_tag]);
            }
            LOWERED.fetch_add(1, Ordering::Relaxed);
            true
        }
    }
}
