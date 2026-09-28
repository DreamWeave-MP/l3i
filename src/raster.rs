//! Raster scalars as packed Luau integers: the `dream.raster` extension.
//!
//! [`Color`] (kind 3) is an RGBA8 color. Its 32-bit form, red in bits 0..7 through alpha in
//! bits 24..31, is host-endian independent and is exactly the four bytes `[r, g, b, a]` a
//! vertex color field or a texture pixel holds, so one value serves script code, vertex
//! buffers, and pixel buffers with no conversion. Every `u32` is a color; the only runtime
//! check is the kind nibble, which rejects a plain integer or another packed kind. l3i keeps
//! channel *semantics* out of this module: whether a color is premultiplied is the renderer's
//! rule, and the renderer extension converts.
//!
//! [`ClipRect`] (kind 4) is an integer pixel rectangle, `min_x, min_y, max_x, max_y` in four
//! 14-bit fields, so a coordinate is at most [`ClipRect::MAX_COORD`] (16383). That limit is a
//! representation choice made here, on purpose and out loud: a renderer whose surfaces can
//! exceed it must not use this type. Construction and unpacking both require `min <= max` on
//! each axis; [`ClipRect::ALL`] has every field at the maximum, which a clamping renderer
//! treats as "no clip".
//!
//! Script surface, module `@dream/raster`:
//!
//! ```lua
//! local raster = require('@dream/raster')
//! local c = raster.rgba8(80, 160, 255, 192)       -- a Color, physically an integer
//! local r, g, b, a = raster.channels(c)
//! buffer.writeu32(vertices, 16, raster.packed(c))   -- the u32 form: the bytes r, g, b, a
//! local half = raster.lerp(raster.BLACK, c, 0.5)
//! local clip = raster.clip(0, 0, 640, 480)       -- a ClipRect
//! local x0, y0, x1, y1 = raster.clipBounds(clip)
//! ```
//!
//! Color arithmetic for GUI and shader-style code lives on the [`Math`] receiver, `raster.math()`,
//! whose methods lower to native code under `jit` when the script annotates it
//! (`local C: dream_raster_Math = raster.math()`): `rgba8`, `rgb8`, `red`/`green`/`blue`/`alpha`,
//! `channels`, `withAlpha`, `lerp`, `mul` (modulate), `add` (saturating), `scale` (the color
//! channels by a factor), and `premultiply`. Shader semantics throughout: inputs clamp to their
//! range, results round to nearest, and a NaN input yields channel 0. The module's `rgba8` and
//! `rgb8` are the strict constructors (an integer outside `0..=255` is an error); the receiver's
//! clamp, so both paths of a lowered call agree by construction.

use crate::error::{Error, Result};
use crate::extension::{Extension, ExtensionDescriptor, InstallContext};
use crate::packed::{BufferPack, Packed, PackedScalar};
use crate::source::CompileConstant;

/// An RGBA8 color. Channel meaning (straight or premultiplied) belongs to the consumer.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Color {
    pub r: u8,
    pub g: u8,
    pub b: u8,
    pub a: u8,
}

impl Color {
    pub const TRANSPARENT: Color = Color::rgba(0, 0, 0, 0);
    pub const BLACK: Color = Color::rgba(0, 0, 0, 255);
    pub const WHITE: Color = Color::rgba(255, 255, 255, 255);

    #[must_use]
    pub const fn rgba(r: u8, g: u8, b: u8, a: u8) -> Color {
        Color { r, g, b, a }
    }

    /// The fixed 32-bit layout: red in bits 0..7, green 8..15, blue 16..23, alpha 24..31.
    #[must_use]
    pub const fn packed(self) -> u32 {
        u32::from_le_bytes([self.r, self.g, self.b, self.a])
    }

    /// The color with the [`Color::packed`] layout `bits`. Every `u32` is a color.
    #[must_use]
    pub const fn from_packed(bits: u32) -> Color {
        let [r, g, b, a] = bits.to_le_bytes();
        Color { r, g, b, a }
    }

    /// The channels as `[r, g, b, a]`, the byte order of a pixel.
    #[must_use]
    pub const fn to_array(self) -> [u8; 4] {
        [self.r, self.g, self.b, self.a]
    }

    /// The Luau integer for this color.
    #[must_use]
    pub const fn pack(self) -> Packed<Color> {
        Packed(self)
    }

    /// A channel value from a number: clamped to `0..=255`, rounded to nearest, NaN to 0. The
    /// native lowering computes exactly this (`min(255, v)`, `max(0, v)`, `+ 0.5`, truncate).
    #[must_use]
    pub fn channel(value: f64) -> u8 {
        (value.clamp(0.0, 255.0) + 0.5) as u8
    }

    /// A color from four numbers, each through [`Color::channel`].
    #[must_use]
    pub fn from_numbers(r: f64, g: f64, b: f64, a: f64) -> Color {
        Color { r: Self::channel(r), g: Self::channel(g), b: Self::channel(b), a: Self::channel(a) }
    }

    fn map(self, other: Color, f: impl Fn(f64, f64) -> f64) -> Color {
        Color::from_numbers(
            f(f64::from(self.r), f64::from(other.r)),
            f(f64::from(self.g), f64::from(other.g)),
            f(f64::from(self.b), f64::from(other.b)),
            f(f64::from(self.a), f64::from(other.a)),
        )
    }

    #[must_use]
    pub fn with_alpha(self, a: f64) -> Color {
        Color { a: Self::channel(a), ..self }
    }

    /// Per-channel linear interpolation; `t` is clamped to `0..=1`.
    #[must_use]
    pub fn lerp(self, other: Color, t: f64) -> Color {
        let t = t.clamp(0.0, 1.0);
        self.map(other, |a, b| a + (b - a) * t)
    }

    /// Per-channel modulation, `a * b / 255`, as a tint multiplies a texel.
    #[must_use]
    pub fn modulate(self, other: Color) -> Color {
        self.map(other, |a, b| a * b / 255.0)
    }

    /// Per-channel saturating addition.
    #[must_use]
    pub fn saturating_add(self, other: Color) -> Color {
        self.map(other, |a, b| a + b)
    }

    /// The color channels scaled by `factor`, alpha unchanged.
    #[must_use]
    pub fn scale(self, factor: f64) -> Color {
        Color {
            r: Self::channel(f64::from(self.r) * factor),
            g: Self::channel(f64::from(self.g) * factor),
            b: Self::channel(f64::from(self.b) * factor),
            a: self.a,
        }
    }

    /// Straight alpha to premultiplied: each color channel becomes `(c * a + 127) / 255` in
    /// integer arithmetic, the rounding renderers use.
    #[must_use]
    pub fn premultiply(self) -> Color {
        let a = f64::from(self.a);
        let pre = |c: u8| ((f64::from(c) * a + 127.0) / 255.0).trunc() as u8;
        Color { r: pre(self.r), g: pre(self.g), b: pre(self.b), a: self.a }
    }
}

/// The color arithmetic receiver (`dream.raster.Math`, from `raster.math()`); see the module
/// docs for the method list and [`lowering::ColorMath`] for the native version.
#[derive(Clone, Copy, Debug, Default)]
pub struct Math;

// SAFETY: no payload, no Lua references.
unsafe impl crate::userdata::Userdata for Math {
    const NAME: &'static str = "dream.raster.Math";
}

impl PackedScalar for Color {
    const KIND: u8 = 3;
    const NAME: &'static str = "Color";
    fn pack(&self) -> (u64, u8) {
        (u64::from(self.packed()), 0)
    }
    fn unpack(payload: u64, _flags: u8) -> Result<Self> {
        let bits = u32::try_from(payload).map_err(|_| Error::runtime("Color payload has bits above 32"))?;
        Ok(Color::from_packed(bits))
    }
}

/// Four bytes `[r, g, b, a]`: the vertex color field and the texture pixel.
impl BufferPack for Color {
    const SIZE: usize = 4;
    fn read_from(bytes: &[u8]) -> Result<Self> {
        u32::read_from(bytes).map(Color::from_packed)
    }
    fn write_to(&self, bytes: &mut [u8]) -> Result<()> {
        self.packed().write_to(bytes)
    }
}

/// An integer pixel rectangle: `min` inclusive, `max` as the consumer defines it (renderers
/// here use half-open). Fields are 14 bits each.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct ClipRect {
    pub min_x: u32,
    pub min_y: u32,
    pub max_x: u32,
    pub max_y: u32,
}

impl ClipRect {
    /// Bits per field.
    pub const FIELD_BITS: u32 = 14;
    /// The largest coordinate a packed clip rectangle can hold.
    pub const MAX_COORD: u32 = (1 << Self::FIELD_BITS) - 1;
    /// From the origin to the addressable limit: no clipping for any surface this type can
    /// address.
    pub const ALL: ClipRect = ClipRect { min_x: 0, min_y: 0, max_x: Self::MAX_COORD, max_y: Self::MAX_COORD };

    /// A rectangle from its bounds; fails when a value exceeds [`Self::MAX_COORD`] or
    /// `min > max` on an axis.
    pub fn new(min_x: u32, min_y: u32, max_x: u32, max_y: u32) -> Result<ClipRect> {
        for (name, value) in [("minX", min_x), ("minY", min_y), ("maxX", max_x), ("maxY", max_y)] {
            if value > Self::MAX_COORD {
                return Err(Error::runtime(format!("ClipRect {name} {value} exceeds the maximum {}", Self::MAX_COORD)));
            }
        }
        if min_x > max_x || min_y > max_y {
            return Err(Error::runtime(format!(
                "ClipRect min ({min_x}, {min_y}) exceeds max ({max_x}, {max_y})"
            )));
        }
        Ok(ClipRect { min_x, min_y, max_x, max_y })
    }

    /// The Luau integer for this rectangle.
    #[must_use]
    pub const fn pack(self) -> Packed<ClipRect> {
        Packed(self)
    }

    /// True when both max fields sit at [`Self::MAX_COORD`]: the rectangle reaches the
    /// addressable limit, which a clamping consumer reads as "clip to the surface only".
    #[must_use]
    pub const fn reaches_limit(self) -> bool {
        self.max_x == Self::MAX_COORD && self.max_y == Self::MAX_COORD
    }
}

impl PackedScalar for ClipRect {
    const KIND: u8 = 4;
    const NAME: &'static str = "ClipRect";
    fn pack(&self) -> (u64, u8) {
        let bits = Self::FIELD_BITS;
        let payload = u64::from(self.min_x)
            | (u64::from(self.min_y) << bits)
            | (u64::from(self.max_x) << (2 * bits))
            | (u64::from(self.max_y) << (3 * bits));
        (payload, 0)
    }
    fn unpack(payload: u64, _flags: u8) -> Result<Self> {
        let bits = Self::FIELD_BITS;
        let mask = u64::from(Self::MAX_COORD);
        let field = |shift: u32| (payload >> shift) & mask;
        ClipRect::new(field(0) as u32, field(bits) as u32, field(2 * bits) as u32, field(3 * bits) as u32)
    }
}

fn channel(name: &str, value: i64) -> Result<u8> {
    u8::try_from(value).map_err(|_| Error::runtime(format!("{name} {value} is outside 0..=255")))
}

fn coordinate(name: &str, value: i64) -> Result<u32> {
    u32::try_from(value)
        .ok()
        .filter(|v| *v <= ClipRect::MAX_COORD)
        .ok_or_else(|| Error::runtime(format!("{name} {value} is outside 0..={}", ClipRect::MAX_COORD)))
}

/// The `dream.raster` extension: module `@dream/raster`.
pub struct RasterExtension;

/// The extension id.
pub const EXTENSION_ID: &str = "dream.raster";
/// The module path.
pub const MODULE: &str = "@dream/raster";

impl Extension for RasterExtension {
    fn id(&self) -> &'static str {
        EXTENSION_ID
    }

    fn describe(&self, d: &mut ExtensionDescriptor) -> Result<()> {
        let math = d.userdata::<Math>("dream.raster.Math");
        math.tag(crate::extension::TagPolicy::Required).doc("Color arithmetic; natively lowered under jit.");
        math.method("rgba8").signature("(self, r: number, g: number, b: number, a: number): number");
        math.method("rgb8").signature("(self, r: number, g: number, b: number): number");
        for channel in ["red", "green", "blue", "alpha"] {
            math.method(channel).signature("(self, color: number): number");
        }
        math.method("channels").signature("(self, color: number): (number, number, number, number)");
        math.method("withAlpha").signature("(self, color: number, a: number): number");
        math.method("lerp").signature("(self, a: number, b: number, t: number): number");
        math.method("mul").signature("(self, a: number, b: number): number");
        math.method("add").signature("(self, a: number, b: number): number");
        math.method("scale").signature("(self, color: number, factor: number): number");
        math.method("premultiply").signature("(self, color: number): number");
        #[cfg(feature = "jit")]
        d.native_hooks(lowering::ColorMath);
        d.module(MODULE).doc("Colors and clip rectangles as packed integers.");
        Ok(())
    }

    fn install(&self, cx: &mut InstallContext<'_>) -> Result<()> {
        type C = Packed<Color>;
        cx.userdata::<Math>("dream.raster.Math")?
            .method("rgba8", |_: &Math, r: f64, g: f64, b: f64, a: f64| Color::from_numbers(r, g, b, a).pack())?
            .method("rgb8", |_: &Math, r: f64, g: f64, b: f64| Color::from_numbers(r, g, b, 255.0).pack())?
            .method("red", |_: &Math, c: C| f64::from(c.0.r))?
            .method("green", |_: &Math, c: C| f64::from(c.0.g))?
            .method("blue", |_: &Math, c: C| f64::from(c.0.b))?
            .method("alpha", |_: &Math, c: C| f64::from(c.0.a))?
            .method("channels", |_: &Math, c: C| (f64::from(c.0.r), f64::from(c.0.g), f64::from(c.0.b), f64::from(c.0.a)))?
            .method("withAlpha", |_: &Math, c: C, a: f64| c.0.with_alpha(a).pack())?
            .method("lerp", |_: &Math, a: C, b: C, t: f64| a.0.lerp(b.0, t).pack())?
            .method("mul", |_: &Math, a: C, b: C| a.0.modulate(b.0).pack())?
            .method("add", |_: &Math, a: C, b: C| a.0.saturating_add(b.0).pack())?
            .method("scale", |_: &Math, c: C, factor: f64| c.0.scale(factor).pack())?
            .method("premultiply", |_: &Math, c: C| c.0.premultiply().pack())?;
        let mut module = cx.module(MODULE)?;
        module
            .constant("TRANSPARENT", CompileConstant::Integer(Color::TRANSPARENT.pack().bits()))?
            .constant("BLACK", CompileConstant::Integer(Color::BLACK.pack().bits()))?
            .constant("WHITE", CompileConstant::Integer(Color::WHITE.pack().bits()))?
            .constant("CLIP_ALL", CompileConstant::Integer(ClipRect::ALL.pack().bits()))?
            .constant("CLIP_MAX_COORD", CompileConstant::Number(f64::from(ClipRect::MAX_COORD)))?
            .function("rgba8", |r: i64, g: i64, b: i64, a: i64| -> Result<Packed<Color>> {
                Ok(Color::rgba(channel("rgba8 red", r)?, channel("rgba8 green", g)?, channel("rgba8 blue", b)?, channel("rgba8 alpha", a)?).pack())
            })?
            .function("rgb8", |r: i64, g: i64, b: i64| -> Result<Packed<Color>> {
                Ok(Color::rgba(channel("rgb8 red", r)?, channel("rgb8 green", g)?, channel("rgb8 blue", b)?, 255).pack())
            })?
            .function("packed", |c: Packed<Color>| f64::from(c.0.packed()))?
            .function("channels", |c: Packed<Color>| {
                (f64::from(c.0.r), f64::from(c.0.g), f64::from(c.0.b), f64::from(c.0.a))
            })?
            .function("withAlpha", |c: Packed<Color>, a: f64| c.0.with_alpha(a).pack())?
            .function("lerp", |a: Packed<Color>, b: Packed<Color>, t: f64| a.0.lerp(b.0, t).pack())?
            .function("mul", |a: Packed<Color>, b: Packed<Color>| a.0.modulate(b.0).pack())?
            .function("add", |a: Packed<Color>, b: Packed<Color>| a.0.saturating_add(b.0).pack())?
            .function("scale", |c: Packed<Color>, factor: f64| c.0.scale(factor).pack())?
            .function("premultiply", |c: Packed<Color>| c.0.premultiply().pack())?
            .function("math", || crate::userdata::Owned(Math))?
            .function("clip", |min_x: i64, min_y: i64, max_x: i64, max_y: i64| -> Result<Packed<ClipRect>> {
                Ok(ClipRect::new(
                    coordinate("clip minX", min_x)?,
                    coordinate("clip minY", min_y)?,
                    coordinate("clip maxX", max_x)?,
                    coordinate("clip maxY", max_y)?,
                )?
                .pack())
            })?
            .function("clipBounds", |c: Packed<ClipRect>| {
                (f64::from(c.0.min_x), f64::from(c.0.min_y), f64::from(c.0.max_x), f64::from(c.0.max_y))
            })?;
        module.finish()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn color_layout_is_red_in_the_low_byte_and_round_trips() {
        let c = Color::rgba(0x11, 0x22, 0x33, 0x44);
        assert_eq!(c.packed(), 0x4433_2211);
        assert_eq!(Color::from_packed(0x4433_2211), c);
        let mut bytes = [0u8; 4];
        c.write_to(&mut bytes).unwrap();
        assert_eq!(bytes, [0x11, 0x22, 0x33, 0x44]);
        assert_eq!(Color::read_from(&bytes).unwrap(), c);
        let bits = c.pack().bits();
        assert_eq!(Packed::<Color>::from_bits(bits).unwrap().0, c);
        assert!(Packed::<ClipRect>::from_bits(bits).is_err(), "a color is not a clip rectangle");
        assert!(Packed::<Color>::from_bits(crate::packed::encode(3, 0, 1 << 40)).is_err(), "high payload bits");
        assert_eq!(Color::BLACK.lerp(Color::WHITE, 0.5), Color::rgba(128, 128, 128, 255));
        assert_eq!(Color::BLACK.lerp(Color::WHITE, 7.0), Color::WHITE);
        assert_eq!(Color::BLACK.lerp(Color::WHITE, f64::NAN), Color::TRANSPARENT, "NaN yields zero channels");
        assert_eq!(Color::rgba(200, 100, 50, 255).modulate(Color::rgba(128, 255, 0, 255)), Color::rgba(100, 100, 0, 255));
        assert_eq!(Color::rgba(200, 100, 50, 10).saturating_add(Color::rgba(100, 100, 100, 250)), Color::rgba(255, 200, 150, 255));
        assert_eq!(Color::rgba(200, 100, 50, 7).scale(0.5), Color::rgba(100, 50, 25, 7));
        assert_eq!(Color::from_numbers(-4.0, 255.4, 254.5, f64::INFINITY), Color::rgba(0, 255, 255, 255));
    }

    #[test]
    fn premultiply_matches_integer_rounding_everywhere() {
        for c in 0..=255u16 {
            for a in 0..=255u16 {
                let expected = ((c * a + 127) / 255) as u8;
                let color = Color::rgba(c as u8, 0, 0, a as u8).premultiply();
                assert_eq!(color.r, expected, "c {c} a {a}");
            }
        }
    }

    #[test]
    fn clip_rect_packs_four_fields_and_validates() {
        let clip = ClipRect::new(1, 2, 16383, 40).unwrap();
        let back = Packed::<ClipRect>::from_bits(clip.pack().bits()).unwrap().0;
        assert_eq!(back, clip);
        assert!(ClipRect::new(5, 0, 4, 0).is_err(), "min above max");
        assert!(ClipRect::new(0, 0, 16384, 0).is_err(), "beyond the field");
        assert!(ClipRect::ALL.reaches_limit());
        // A forged payload with min > max fails on unpack, not just on construction.
        let forged = crate::packed::encode(4, 0, 9 | (3 << 28));
        assert!(Packed::<ClipRect>::from_bits(forged).is_err());
    }
}

/// Native lowering of the [`Math`] receiver (`jit`): a color is unpacked with shifts and masks
/// into four doubles, the arithmetic runs on doubles, and the result is clamped, rounded, and
/// packed into one integer store. Every operand is tag- and kind-checked; a mismatch exits to
/// the interpreter, whose bound method raises the type error. `min(255, v)` and `max(0, v)` are
/// emitted with the constant first so a NaN comes through as NaN on every target and converts
/// to channel 0, as [`Color::channel`] does.
#[cfg(feature = "jit")]
pub mod lowering {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::{Color, Math};
    use crate::native_code::hooks::{NamecallSite, NativeCodeHooks, NativeContext};
    use crate::native_code::ir::{IrBuilder, IrCmd, IrCondition, IrOp, bytecode_type};
    use crate::packed::PackedScalar;
    use crate::raw::ffi::{LUA_TINTEGER, LUA_TNUMBER};

    static LOWERED: AtomicUsize = AtomicUsize::new(0);

    /// How many call sites the hook has lowered in this process (a diagnostic for tests).
    #[doc(hidden)]
    pub fn lowered_sites() -> usize {
        LOWERED.load(Ordering::Relaxed)
    }

    /// The hook set; [`super::RasterExtension`] registers it.
    pub struct ColorMath;

    const KIND: i64 = <Color as PackedScalar>::KIND as i64;
    const SHIFTS: [i64; 4] = [0, 8, 16, 24];

    /// Checks the register holds a packed `Color` and returns its bits.
    fn checked_color(build: &mut IrBuilder<'_>, reg: IrOp, exit: IrOp) -> IrOp {
        build.load_and_check_tag(reg, LUA_TINTEGER as u8, exit);
        let bits = build.inst(IrCmd::LOAD_INT64, &[reg]);
        let sixty = build.const_int64(60);
        let fifteen = build.const_int64(15);
        let kind = build.inst(IrCmd::BITRSHIFT_INT64, &[bits, sixty]);
        let kind = build.inst(IrCmd::BITAND_INT64, &[kind, fifteen]);
        let expected = build.const_int64(KIND);
        let equal = build.cond(IrCondition::Equal);
        build.inst(IrCmd::CHECK_CMP_INT64, &[kind, expected, equal, exit]);
        bits
    }

    fn number(build: &mut IrBuilder<'_>, reg: IrOp, exit: IrOp) -> IrOp {
        build.load_and_check_tag(reg, LUA_TNUMBER as u8, exit);
        build.inst(IrCmd::LOAD_DOUBLE, &[reg])
    }

    /// One channel of `bits` as a double. Every value a lowering defines must be consumed: an
    /// unused value stays live and trips Luau's no-spills assertion at the next call, so
    /// callers unpack only the lanes they use.
    fn lane(build: &mut IrBuilder<'_>, bits: IrOp, shift: i64) -> IrOp {
        let mask = build.const_int64(255);
        let lane = if shift == 0 {
            bits
        } else {
            let amount = build.const_int64(shift);
            build.inst(IrCmd::BITRSHIFT_INT64, &[bits, amount])
        };
        let lane = build.inst(IrCmd::BITAND_INT64, &[lane, mask]);
        build.inst(IrCmd::INT64_TO_NUM, &[lane])
    }

    /// The four channels of `bits` as doubles.
    fn unpack(build: &mut IrBuilder<'_>, bits: IrOp) -> [IrOp; 4] {
        SHIFTS.map(|shift| lane(build, bits, shift))
    }

    /// One channel from a double: clamp, round (or truncate), convert, mask.
    fn channel(build: &mut IrBuilder<'_>, value: IrOp, round: bool) -> IrOp {
        let top = build.const_double(255.0);
        let zero = build.const_double(0.0);
        let value = build.inst(IrCmd::MIN_NUM, &[top, value]);
        let value = build.inst(IrCmd::MAX_NUM, &[zero, value]);
        let value = if round {
            let half = build.const_double(0.5);
            build.inst(IrCmd::ADD_NUM, &[value, half])
        } else {
            value
        };
        let value = build.inst(IrCmd::NUM_TO_INT64, &[value]);
        let mask = build.const_int64(255);
        build.inst(IrCmd::BITAND_INT64, &[value, mask])
    }

    /// Packs four channel doubles into a `Color` integer.
    fn pack(build: &mut IrBuilder<'_>, channels: [IrOp; 4], round: bool) -> IrOp {
        let mut bits: Option<IrOp> = None;
        for (value, shift) in channels.into_iter().zip(SHIFTS) {
            let lane = channel(build, value, round);
            let lane = if shift == 0 {
                lane
            } else {
                let amount = build.const_int64(shift);
                build.inst(IrCmd::BITLSHIFT_INT64, &[lane, amount])
            };
            bits = Some(match bits {
                None => lane,
                Some(bits) => build.inst(IrCmd::BITOR_INT64, &[bits, lane]),
            });
        }
        let kind = build.const_int64(KIND << 60);
        let bits = bits.expect("four channels");
        build.inst(IrCmd::BITOR_INT64, &[bits, kind])
    }

    fn store_integer(build: &mut IrBuilder<'_>, result: IrOp, bits: IrOp) {
        build.inst(IrCmd::STORE_INT64, &[result, bits]);
        let tag = build.const_tag(LUA_TINTEGER as u8);
        build.inst(IrCmd::STORE_TAG, &[result, tag]);
    }

    fn store_number(build: &mut IrBuilder<'_>, result: IrOp, value: IrOp) {
        build.inst(IrCmd::STORE_DOUBLE, &[result, value]);
        let tag = build.const_tag(LUA_TNUMBER as u8);
        build.inst(IrCmd::STORE_TAG, &[result, tag]);
    }

    /// The result count each method produces.
    fn results_of(member: &str) -> Option<i32> {
        Some(match member {
            "channels" => 4,
            "rgba8" | "rgb8" | "red" | "green" | "blue" | "alpha" | "withAlpha" | "lerp" | "mul" | "add" | "scale"
            | "premultiply" => 1,
            _ => return None,
        })
    }

    impl NativeCodeHooks for ColorMath {
        fn userdata_namecall_type(&self, context: &NativeContext<'_>, userdata_type: u8, member: &str) -> u8 {
            if context.userdata_type_of::<Math>() != Some(userdata_type) {
                return bytecode_type::ANY;
            }
            match member {
                "red" | "green" | "blue" | "alpha" => bytecode_type::NUMBER,
                "channels" => bytecode_type::ANY,
                _ if results_of(member).is_some() => bytecode_type::INTEGER,
                _ => bytecode_type::ANY,
            }
        }

        #[allow(clippy::too_many_lines)]
        fn userdata_namecall(
            &self,
            context: &NativeContext<'_>,
            build: &mut IrBuilder<'_>,
            userdata_type: u8,
            member: &str,
            site: NamecallSite,
        ) -> bool {
            if context.userdata_type_of::<Math>() != Some(userdata_type) {
                return false;
            }
            let Some(results) = results_of(member) else { return false };
            if site.results != results {
                return false;
            }
            let Some(tag) = context.tag_of::<Math>() else { return false };
            let exit = build.vm_exit(site.pcpos);
            let receiver = build.vm_reg(site.source_reg);
            let pointer = build.inst(IrCmd::LOAD_POINTER, &[receiver]);
            let tag = build.const_int(i32::from(tag));
            build.inst(IrCmd::CHECK_USERDATA_TAG, &[pointer, tag, exit]);
            let result = build.vm_reg(site.arg_res_reg);
            let arg = |build: &mut IrBuilder<'_>, index: i32| build.vm_reg(site.arg_res_reg + 2 + index);
            match (member, site.params) {
                ("rgba8", 5) | ("rgb8", 4) => {
                    let mut channels = [result; 4];
                    for (i, slot) in channels.iter_mut().enumerate().take(site.params as usize - 1) {
                        let reg = arg(build, i as i32);
                        *slot = number(build, reg, exit);
                    }
                    if member == "rgb8" {
                        channels[3] = build.const_double(255.0);
                    }
                    let bits = pack(build, channels, true);
                    store_integer(build, result, bits);
                }
                ("red" | "green" | "blue" | "alpha", 2) => {
                    let reg = arg(build, 0);
                    let bits = checked_color(build, reg, exit);
                    let index = ["red", "green", "blue", "alpha"].iter().position(|m| *m == member).expect("matched");
                    let value = lane(build, bits, SHIFTS[index]);
                    store_number(build, result, value);
                }
                ("channels", 2) => {
                    let reg = arg(build, 0);
                    let bits = checked_color(build, reg, exit);
                    let channels = unpack(build, bits);
                    for (i, value) in channels.into_iter().enumerate() {
                        let slot = build.vm_reg(site.arg_res_reg + i as i32);
                        store_number(build, slot, value);
                    }
                }
                ("withAlpha", 3) => {
                    let (c_reg, a_reg) = (arg(build, 0), arg(build, 1));
                    let bits = checked_color(build, c_reg, exit);
                    let alpha = number(build, a_reg, exit);
                    let channels = [lane(build, bits, 0), lane(build, bits, 8), lane(build, bits, 16), alpha];
                    let bits = pack(build, channels, true);
                    store_integer(build, result, bits);
                }
                ("lerp", 4) => {
                    let (a_reg, b_reg, t_reg) = (arg(build, 0), arg(build, 1), arg(build, 2));
                    let a = checked_color(build, a_reg, exit);
                    let b = checked_color(build, b_reg, exit);
                    let t = number(build, t_reg, exit);
                    let one = build.const_double(1.0);
                    let zero = build.const_double(0.0);
                    let t = build.inst(IrCmd::MIN_NUM, &[one, t]);
                    let t = build.inst(IrCmd::MAX_NUM, &[zero, t]);
                    let (a, b) = (unpack(build, a), unpack(build, b));
                    let mut out = [result; 4];
                    for i in 0..4 {
                        let delta = build.inst(IrCmd::SUB_NUM, &[b[i], a[i]]);
                        let step = build.inst(IrCmd::MUL_NUM, &[delta, t]);
                        out[i] = build.inst(IrCmd::ADD_NUM, &[a[i], step]);
                    }
                    let bits = pack(build, out, true);
                    store_integer(build, result, bits);
                }
                ("mul" | "add", 3) => {
                    let (a_reg, b_reg) = (arg(build, 0), arg(build, 1));
                    let a = checked_color(build, a_reg, exit);
                    let b = checked_color(build, b_reg, exit);
                    let (a, b) = (unpack(build, a), unpack(build, b));
                    let mut out = [result; 4];
                    let top = build.const_double(255.0);
                    for i in 0..4 {
                        out[i] = if member == "mul" {
                            // `a * b / 255`, the division as the interpreter computes it.
                            let product = build.inst(IrCmd::MUL_NUM, &[a[i], b[i]]);
                            build.inst(IrCmd::DIV_NUM, &[product, top])
                        } else {
                            build.inst(IrCmd::ADD_NUM, &[a[i], b[i]])
                        };
                    }
                    let bits = pack(build, out, true);
                    store_integer(build, result, bits);
                }
                ("scale", 3) => {
                    let (c_reg, f_reg) = (arg(build, 0), arg(build, 1));
                    let bits = checked_color(build, c_reg, exit);
                    let factor = number(build, f_reg, exit);
                    let channels = unpack(build, bits);
                    let mut out = channels;
                    for slot in out.iter_mut().take(3) {
                        *slot = build.inst(IrCmd::MUL_NUM, &[*slot, factor]);
                    }
                    // Alpha passes through: it is already an exact channel, so rounding is a no-op.
                    let bits = pack(build, out, true);
                    store_integer(build, result, bits);
                }
                ("premultiply", 2) => {
                    let reg = arg(build, 0);
                    let bits = checked_color(build, reg, exit);
                    let channels = unpack(build, bits);
                    let alpha = channels[3];
                    let bias = build.const_double(127.0);
                    let top = build.const_double(255.0);
                    let mut out = channels;
                    for slot in out.iter_mut().take(3) {
                        let scaled = build.inst(IrCmd::MUL_NUM, &[*slot, alpha]);
                        let biased = build.inst(IrCmd::ADD_NUM, &[scaled, bias]);
                        *slot = build.inst(IrCmd::DIV_NUM, &[biased, top]);
                    }
                    // Truncating conversion: `(c * a + 127) / 255` in integer arithmetic.
                    let bits = pack(build, out, false);
                    store_integer(build, result, bits);
                }
                _ => return false,
            }
            LOWERED.fetch_add(1, Ordering::Relaxed);
            true
        }
    }
}
