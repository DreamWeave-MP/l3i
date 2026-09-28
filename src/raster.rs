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

    /// Per-channel linear interpolation, rounded to nearest; `t` is clamped to `0..=1`.
    #[must_use]
    pub fn lerp(self, other: Color, t: f64) -> Color {
        let t = if t.is_nan() { 0.0 } else { t.clamp(0.0, 1.0) };
        let mix = |a: u8, b: u8| (f64::from(a) + (f64::from(b) - f64::from(a)) * t).round() as u8;
        Color { r: mix(self.r, other.r), g: mix(self.g, other.g), b: mix(self.b, other.b), a: mix(self.a, other.a) }
    }
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
        d.module(MODULE).doc("Colors and clip rectangles as packed integers.");
        Ok(())
    }

    fn install(&self, cx: &mut InstallContext<'_>) -> Result<()> {
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
            .function("withAlpha", |c: Packed<Color>, a: i64| -> Result<Packed<Color>> {
                Ok(Color { a: channel("withAlpha alpha", a)?, ..c.0 }.pack())
            })?
            .function("lerp", |a: Packed<Color>, b: Packed<Color>, t: f64| a.0.lerp(b.0, t).pack())?
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
