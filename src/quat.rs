//! Rotations as packed Luau integers: the `dream.quat` extension.
//!
//! A unit quaternion is compressed smallest-three into the 56-bit payload of a
//! [`PackedScalar`] ([`Quaternion`], kind 1; [`AnimationKey`], kind 2, adds four flag bits for a
//! pose or frame key): two bits name the largest component, the other three are stored
//! as 18-bit lanes over `[-1/√2, 1/√2]`, and the omitted one is rebuilt from the unit norm. The
//! grid has an exact zero, so the identity and axis-aligned rotations round-trip exactly; a
//! random rotation comes back within 1.6e-5 rad (mean 5.7e-6). Scripts see one integer: no
//! allocation, no GC object, table keys and buffer fields for free, and a type error rather than
//! garbage when an integer of another kind is passed where a rotation is expected.
//!
//! The packed form is storage and transport, never the live accumulator: re-encoding after
//! every blend step random-walks (1.7e-3 rad over 100k slerp steps in the experiment), while
//! packing an f64 state each step stays within one quantisation step. Keep long-lived rotation
//! state as [`Quat`] on the host and pack what scripts, saves, and the wire see.
//!
//! Script surface, module `@dream/quat`:
//!
//! ```lua
//! local quat = require('@dream/quat')
//! local q = quat.axisAngle(vector.create(0, 0, 1), math.pi / 2)
//! local v = quat.rotate(q, vector.create(1, 0, 0))
//! local r = quat.mul(q, quat.inverse(q))      -- quat.IDENTITY, a compile-time constant
//! local x, y, z, w = quat.toXYZW(quat.slerp(q, r, 0.5))
//! local k = quat.key(q, 5)                      -- an AnimationKey: q plus flag bits 0101
//! assert(quat.keyFlags(k) == 5 and quat.angleTo(quat.keyRotation(k), q) < 1e-5)
//! quat.mul(q, k)                                 -- error: expected a packed Quaternion
//! ```
//!
//! With the `jit` feature, `quat.math()` returns a tagged receiver whose `rotate(q, v)`,
//! `mul(a, b)`, `slerp(a, b, t)`, `key(q, flags)`, `keyRotation(k)`, and `keyFlags(k)` are lowered to IR by
//! [`lowering::Lowering`] when the receiver's type is known to the compiler
//! (`local Q: dream_quat_Math = quat.math()` in a `--!native` script): no C call, the integer is
//! unpacked with shifts and masks, the arithmetic runs on doubles, and the result is stored as a
//! vector, a number, or a fresh packed integer. Measured per call inside native code: rotate
//! 21 ns and mul 45 ns, against 99 ns and 150 ns through the binder and 46 ns and 111 ns for an
//! f32 quaternion userdata (the latter allocating); `key` plus `keyRotation` together take 5 ns
//! against 220 ns through the binder.

use crate::convert::{Exact, Vector3};
use crate::error::Result;
use crate::extension::{Extension, ExtensionDescriptor};
use crate::packed::{Packed, PackedScalar};
use crate::source::CompileConstant;

/// A unit quaternion in f64: the reference representation and the host's accumulator.
#[derive(Clone, Copy, Debug, PartialEq)]
#[allow(clippy::many_single_char_names)]
pub struct Quat {
    pub x: f64,
    pub y: f64,
    pub z: f64,
    pub w: f64,
}

#[allow(clippy::many_single_char_names)]
impl Quat {
    pub const IDENTITY: Quat = Quat { x: 0.0, y: 0.0, z: 0.0, w: 1.0 };

    /// The rotation of `angle` radians about `axis` (any non-zero length).
    #[must_use]
    pub fn from_axis_angle(axis: [f64; 3], angle: f64) -> Quat {
        let len = (axis[0] * axis[0] + axis[1] * axis[1] + axis[2] * axis[2]).sqrt();
        let (s, c) = (angle / 2.0).sin_cos();
        Quat { x: axis[0] / len * s, y: axis[1] / len * s, z: axis[2] / len * s, w: c }
    }

    /// `from_axis_angle` for a finite, non-zero axis and a finite angle; `None` otherwise (the
    /// unchecked form divides by zero on a zero axis).
    #[must_use]
    pub fn try_from_axis_angle(axis: [f64; 3], angle: f64) -> Option<Quat> {
        let len2 = axis[0] * axis[0] + axis[1] * axis[1] + axis[2] * axis[2];
        (angle.is_finite() && len2.is_finite() && len2 > 0.0).then(|| Quat::from_axis_angle(axis, angle))
    }

    /// `normalize` for a finite, non-zero quaternion; `None` otherwise.
    #[must_use]
    pub fn try_normalize(self) -> Option<Quat> {
        let n2 = self.x * self.x + self.y * self.y + self.z * self.z + self.w * self.w;
        (n2.is_finite() && n2 > 0.0).then(|| self.normalize())
    }

    #[must_use]
    pub fn normalize(self) -> Quat {
        let n = (self.x * self.x + self.y * self.y + self.z * self.z + self.w * self.w).sqrt();
        let inv = 1.0 / n;
        Quat { x: self.x * inv, y: self.y * inv, z: self.z * inv, w: self.w * inv }
    }

    /// The inverse of a unit quaternion (its conjugate).
    #[must_use]
    pub fn inverse(self) -> Quat {
        Quat { x: -self.x, y: -self.y, z: -self.z, w: self.w }
    }

    #[must_use]
    pub fn dot(self, o: Quat) -> f64 {
        self.x * o.x + self.y * o.y + self.z * o.z + self.w * o.w
    }

    /// Spherical interpolation along the shorter arc, `t` clamped to `0..=1`; linear (then
    /// normalised) when the inputs are nearly parallel. The trigonometry is polynomial
    /// ([`acos_poly`], [`sin_poly`]) so the native lowering computes exactly this without a
    /// libm call; the angular error against exact slerp stays below 1e-7 rad, three orders of
    /// magnitude under the packed form's own quantisation.
    #[must_use]
    pub fn slerp(self, mut o: Quat, t: f64) -> Quat {
        let t = t.clamp(0.0, 1.0);
        let mut d = self.dot(o);
        if d < 0.0 {
            o = Quat { x: -o.x, y: -o.y, z: -o.z, w: -o.w };
            d = -d;
        }
        if d >= 0.9995 {
            return Quat {
                x: self.x + (o.x - self.x) * t,
                y: self.y + (o.y - self.y) * t,
                z: self.z + (o.z - self.z) * t,
                w: self.w + (o.w - self.w) * t,
            }
            .normalize();
        }
        let theta = acos_poly(d);
        let inv_sin_theta = 1.0 / (1.0 - d * d).sqrt();
        let (s0, s1) = (sin_poly((1.0 - t) * theta) * inv_sin_theta, sin_poly(t * theta) * inv_sin_theta);
        // The polynomials leave the norm off by up to 1e-8; renormalising costs one sqrt.
        Quat {
            x: s0 * self.x + s1 * o.x,
            y: s0 * self.y + s1 * o.y,
            z: s0 * self.z + s1 * o.z,
            w: s0 * self.w + s1 * o.w,
        }
        .normalize()
    }

    /// Rotates `v`.
    #[must_use]
    pub fn rotate(self, v: [f64; 3]) -> [f64; 3] {
        // v' = v + w t + q × t with t = 2 (q × v): 18 multiplies, no second product.
        let (qx, qy, qz) = (self.x, self.y, self.z);
        let t = [2.0 * (qy * v[2] - qz * v[1]), 2.0 * (qz * v[0] - qx * v[2]), 2.0 * (qx * v[1] - qy * v[0])];
        [
            v[0] + self.w * t[0] + (qy * t[2] - qz * t[1]),
            v[1] + self.w * t[1] + (qz * t[0] - qx * t[2]),
            v[2] + self.w * t[2] + (qx * t[1] - qy * t[0]),
        ]
    }

    /// The rotation angle between two unit quaternions, in radians.
    #[must_use]
    pub fn angle_to(self, o: Quat) -> f64 {
        2.0 * self.dot(o).abs().min(1.0).acos()
    }
}

/// `acos(x)` for `0 <= x <= 1` as `sqrt(1 - x)` times a degree-7 polynomial (Abramowitz and
/// Stegun 4.4.46), absolute error below 2e-8. Shared with the native lowering.
pub const ACOS_COEFFICIENTS: [f64; 8] = [
    1.570_796_305_0,
    -0.214_598_801_6,
    0.088_978_987_4,
    -0.050_174_304_6,
    0.030_891_881_0,
    -0.017_088_125_6,
    0.006_670_090_1,
    -0.001_262_491_1,
];

/// `sin(x)` for `0 <= x <= pi/2` as the odd Taylor series through `x^11`, absolute error below
/// 6e-8 on that range (the packed form quantises at 4e-6). Shared with the native lowering.
pub const SIN_COEFFICIENTS: [f64; 6] =
    [1.0, -1.0 / 6.0, 1.0 / 120.0, -1.0 / 5040.0, 1.0 / 362_880.0, -1.0 / 39_916_800.0];

#[must_use]
pub fn acos_poly(x: f64) -> f64 {
    let mut poly = ACOS_COEFFICIENTS[7];
    for c in ACOS_COEFFICIENTS[..7].iter().rev() {
        poly = poly * x + c;
    }
    (1.0 - x).sqrt() * poly
}

#[must_use]
pub fn sin_poly(x: f64) -> f64 {
    let x2 = x * x;
    let mut poly = SIN_COEFFICIENTS[5];
    for c in SIN_COEFFICIENTS[..5].iter().rev() {
        poly = poly * x2 + c;
    }
    x * poly
}

/// The Hamilton product `a * b`: apply `b`, then `a`.
impl std::ops::Mul for Quat {
    type Output = Quat;
    fn mul(self, o: Quat) -> Quat {
        Quat {
            x: self.w * o.x + self.x * o.w + self.y * o.z - self.z * o.y,
            y: self.w * o.y - self.x * o.z + self.y * o.w + self.z * o.x,
            z: self.w * o.z + self.x * o.y - self.y * o.x + self.z * o.w,
            w: self.w * o.w - self.x * o.x - self.y * o.y - self.z * o.z,
        }
    }
}

// ---- smallest-three packing ------------------------------------------------------------------

/// Bits per stored component.
pub const COMPONENT_BITS: u32 = 18;
/// One fewer level than the bit width allows, so the grid has an exact zero.
pub const COMPONENT_MAX: f64 = ((1u64 << COMPONENT_BITS) - 2) as f64;
/// The magnitude bound of a non-largest component of a unit quaternion.
pub const RANGE: f64 = std::f64::consts::FRAC_1_SQRT_2;

/// A rotation packed into 56 bits: 2 bits for the omitted (largest) component, 3 × 18 bits.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct PackedRotation(pub u64);

impl PackedRotation {
    /// Packs `q`, normalising first only when it is not already unit.
    #[must_use]
    pub fn encode(q: Quat) -> PackedRotation {
        // Measured: a branch-free four-lane variant and an f32 variant were both slower (the
        // latter through a software fma), so this stays scalar; the floor is the three
        // float-to-integer conversions.
        let norm2 = q.x * q.x + q.y * q.y + q.z * q.z + q.w * q.w;
        let q = if (norm2 - 1.0).abs() < 1e-9 { q } else { q.normalize() };
        let c = [q.x, q.y, q.z, q.w];
        let mut largest = 0;
        for i in 1..4 {
            if c[i].abs() > c[largest].abs() {
                largest = i;
            }
        }
        let sign = if c[largest] < 0.0 { -1.0 } else { 1.0 };
        let mut bits = largest as u64;
        for (i, component) in c.iter().enumerate() {
            if i == largest {
                continue;
            }
            let normalized = (component * sign / RANGE).clamp(-1.0, 1.0).midpoint(1.0);
            // `normalized` is in [0, 1]: adding one half and truncating rounds to nearest.
            let quantized = (normalized * COMPONENT_MAX + 0.5) as u64;
            bits = (bits << COMPONENT_BITS) | quantized;
        }
        PackedRotation(bits)
    }

    #[must_use]
    pub fn decode(self) -> Quat {
        let mut bits = self.0;
        let mut values = [0.0f64; 3];
        for slot in (0..3).rev() {
            let quantized = (bits & ((1u64 << COMPONENT_BITS) - 1)) as f64;
            values[slot] = (quantized / COMPONENT_MAX * 2.0 - 1.0) * RANGE;
            bits >>= COMPONENT_BITS;
        }
        let largest = (bits & 3) as usize;
        let mut c = [0.0f64; 4];
        let mut slot = 0;
        for (i, component) in c.iter_mut().enumerate() {
            if i == largest {
                continue;
            }
            *component = values[slot];
            slot += 1;
        }
        c[largest] = (1.0 - values.iter().map(|v| v * v).sum::<f64>()).max(0.0).sqrt();
        Quat { x: c[0], y: c[1], z: c[2], w: c[3] }
    }
}

/// The packed scalar kind of a rotation (kind 1, flags always zero).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Quaternion(pub Quat);

impl PackedScalar for Quaternion {
    const KIND: u8 = 1;
    const NAME: &'static str = "Quaternion";
    fn pack(&self) -> (u64, u8) {
        (PackedRotation::encode(self.0).0, 0)
    }
    fn unpack(payload: u64, _flags: u8) -> Result<Self> {
        Ok(Quaternion(PackedRotation(payload).decode()))
    }
}

impl Quaternion {
    /// The Luau integer for `q`.
    #[must_use]
    pub fn pack(q: Quat) -> Packed<Quaternion> {
        Packed(Quaternion(q))
    }
}

/// The packed scalar kind of a rotation plus four bits of side data (kind 2): a pose key, an
/// animation frame, a network snapshot, all in one integer. l3i keeps the flag nibble opaque; the
/// system that produces the keys defines the bits.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct AnimationKey {
    pub rotation: Quat,
    /// Four bits; higher bits are dropped when packing.
    pub flags: u8,
}

impl PackedScalar for AnimationKey {
    const KIND: u8 = 2;
    const NAME: &'static str = "AnimationKey";
    fn pack(&self) -> (u64, u8) {
        (PackedRotation::encode(self.rotation).0, self.flags & 0xF)
    }
    fn unpack(payload: u64, flags: u8) -> Result<Self> {
        Ok(AnimationKey { rotation: PackedRotation(payload).decode(), flags })
    }
}

impl AnimationKey {
    /// The Luau integer for `rotation` with `flags` (low four bits).
    #[must_use]
    pub fn pack(rotation: Quat, flags: u8) -> Packed<AnimationKey> {
        Packed(AnimationKey { rotation, flags: flags & 0xF })
    }
}

/// `slerp`'s weight must be finite: a NaN or infinite `t` would flow through the
/// interpolation into a valid-looking packed integer. Finite values outside `[0, 1]` clamp.
fn finite_weight(t: f64) -> Result<()> {
    if t.is_finite() {
        Ok(())
    } else {
        Err(crate::error::Error::runtime("quat.slerp: t must be finite (values outside 0..1 clamp)"))
    }
}

/// An animation key's flags: the low four bits of an exact integer, by contract (a fraction
/// is refused by `Exact`; higher bits are dropped, on the lowered path too).
fn key_flags(flags: Exact<i64>) -> u8 {
    (flags.0 & 0xF) as u8
}

fn to_vec3(v: [f64; 3]) -> Vector3 {
    Vector3 { x: v[0] as f32, y: v[1] as f32, z: v[2] as f32 }
}

fn from_vec3(v: Vector3) -> [f64; 3] {
    [f64::from(v.x), f64::from(v.y), f64::from(v.z)]
}

/// The `dream.quat` extension: module `@dream/quat` and, with `jit`, the `dream.quat.Math`
/// receiver whose operations lower to native code.
pub struct QuatExtension;

impl Extension for QuatExtension {
    fn id(&self) -> &'static str {
        "dream.quat"
    }

    fn describe(&self, d: &mut ExtensionDescriptor) -> Result<()> {
        let module = d.module("@dream/quat");
        module
            .doc("Rotations as packed integers.")
            .constant("IDENTITY", CompileConstant::Integer(Quaternion::pack(Quat::IDENTITY).bits()?))
            .function("axisAngle", |axis: Vector3, angle: f64| -> Result<Packed<Quaternion>> {
                Quat::try_from_axis_angle(from_vec3(axis), angle).map(Quaternion::pack).ok_or_else(|| {
                    crate::error::Error::runtime("quat.axisAngle: the axis must be finite and non-zero and the angle finite")
                })
            }).signature("(axis: vector, angle: number) -> integer")
            .function("fromXYZW", |x: f64, y: f64, z: f64, w: f64| -> Result<Packed<Quaternion>> {
                Quat { x, y, z, w }.try_normalize().map(Quaternion::pack).ok_or_else(|| {
                    crate::error::Error::runtime("quat.fromXYZW: the components must be finite and not all zero")
                })
            }).signature("(x: number, y: number, z: number, w: number) -> integer")
            .function("toXYZW", |q: Packed<Quaternion>| (q.0.0.x, q.0.0.y, q.0.0.z, q.0.0.w)).signature("(q: integer) -> (number, number, number, number)")
            .function("mul", |a: Packed<Quaternion>, b: Packed<Quaternion>| Quaternion::pack(a.0.0 * b.0.0)).signature("(a: integer, b: integer) -> integer")
            .function("inverse", |q: Packed<Quaternion>| Quaternion::pack(q.0.0.inverse())).signature("(q: integer) -> integer")
            .function("slerp", |a: Packed<Quaternion>, b: Packed<Quaternion>, t: f64| -> Result<Packed<Quaternion>> {
                finite_weight(t)?;
                Ok(Quaternion::pack(a.0.0.slerp(b.0.0, t)))
            }).signature("(a: integer, b: integer, t: number) -> integer")
            .function("rotate", |q: Packed<Quaternion>, v: Vector3| to_vec3(q.0.0.rotate(from_vec3(v)))).signature("(q: integer, v: vector) -> vector")
            .function("angleTo", |a: Packed<Quaternion>, b: Packed<Quaternion>| a.0.0.angle_to(b.0.0)).signature("(a: integer, b: integer) -> number")
            .function("key", |q: Packed<Quaternion>, flags: Exact<i64>| AnimationKey::pack(q.0.0, key_flags(flags))).signature("(q: integer, flags: number) -> integer")
            .function("keyRotation", |k: Packed<AnimationKey>| Quaternion::pack(k.0.rotation)).signature("(k: integer) -> integer")
            .function("keyFlags", |k: Packed<AnimationKey>| i64::from(k.0.flags)).signature("(k: integer) -> number");
        #[cfg(feature = "jit")]
        {
            module.function("math", || crate::userdata::Owned(lowering::Math)).signature("() -> dream_quat_Math");
            let mut receiver = d.userdata::<lowering::Math>("dream.quat.Math");
            receiver
                .tag(crate::extension::TagPolicy::Required)
                .compiler_type(crate::extension::CompilerTypePolicy::Required)
                .doc("Natively lowered rotation operations.");
            receiver
                .method("rotate", |_: &lowering::Math, q: Packed<Quaternion>, v: Vector3| lowering::rotate(q, v)).signature("(self, q: integer, v: vector): vector");
            receiver
                .method("mul", |_: &lowering::Math, a: Packed<Quaternion>, b: Packed<Quaternion>| lowering::mul(a, b)).signature("(self, a: integer, b: integer): integer");
            receiver
                .method("slerp", |_: &lowering::Math, a: Packed<Quaternion>, b: Packed<Quaternion>, t: f64| {
                    lowering::slerp(a, b, t)
                }).signature("(self, a: integer, b: integer, t: number): integer");
            receiver
                .method("key", |_: &lowering::Math, q: Packed<Quaternion>, flags: Exact<i64>| {
                    AnimationKey::pack(q.0.0, key_flags(flags))
                }).signature("(self, q: integer, flags: number): integer");
            receiver
                .method("keyRotation", |_: &lowering::Math, k: Packed<AnimationKey>| Quaternion::pack(k.0.rotation)).signature("(self, k: integer): integer");
            receiver
                .method("keyFlags", |_: &lowering::Math, k: Packed<AnimationKey>| i64::from(k.0.flags)).signature("(self, k: integer): number");
            d.native_hooks(lowering::Lowering);
        }
        Ok(())
    }
}

/// Native lowering of the packed operations (`jit`).
///
/// [`Math`] is a payload-free tagged receiver: `Q:rotate(q, v)`, `Q:mul(a, b)`, `Q:key(q, flags)`,
/// `Q:keyRotation(k)`, and `Q:keyFlags(k)` are ordinary bound methods on the interpreter path
/// and lowered through
/// [`NativeCodeHooks::userdata_namecall`] when the compiler knows the receiver's type. The
/// lowering checks the receiver's tag, the operands' integer tags and packed kinds (a mismatch
/// exits to the interpreter, whose C method raises the type error), and writes a vector or a
/// packed integer straight into the result register. It matches the interpreter path to the
/// `acos` noise floor. Only single-result, fixed-arity call sites lower: `return Q:mul(a, b)`
/// and `Q:keyRotation(Q:key(q, 3))` (a call nested as the last argument is a multiple-results
/// call, which gives the outer call a dynamic argument count) run through the bound method
/// instead. Bind the inner result to a local first.
#[cfg(feature = "jit")]
#[allow(clippy::many_single_char_names)]
pub mod lowering {
    use std::ffi::c_int;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::{AnimationKey, COMPONENT_BITS, COMPONENT_MAX, Quaternion, RANGE};
    use crate::convert::Vector3;
    use crate::native_code::hooks::{NamecallSite, NativeCodeHooks, NativeContext};
    use crate::native_code::ir::{IrBlockKind, IrBuilder, IrCmd, IrCondition, IrOp, bytecode_type};
    use crate::packed::{Packed, PackedScalar};
    use crate::raw::ffi::{LUA_TINTEGER, LUA_TNUMBER, LUA_TVECTOR};
    use crate::userdata::Userdata;

    /// The receiver `quat.math()` returns (`dream.quat.Math`, class `dream_quat_Math` in annotations).
    #[derive(Clone, Copy, Debug, Default)]
    pub struct Math;

    // SAFETY: no payload, no Lua references.
    unsafe impl Userdata for Math {
        const NAME: &'static str = "dream.quat.Math";
    }

    static LOWERED: AtomicUsize = AtomicUsize::new(0);

    /// How many call sites the hook has lowered in this process (a diagnostic for tests).
    #[doc(hidden)]
    pub fn lowered_sites() -> usize {
        LOWERED.load(Ordering::Relaxed)
    }

    /// The interpreter path of `Math:rotate`, the oracle the lowering must match.
    pub fn rotate(q: Packed<Quaternion>, v: Vector3) -> Vector3 {
        let r = q.0.0.rotate([f64::from(v.x), f64::from(v.y), f64::from(v.z)]);
        Vector3 { x: r[0] as f32, y: r[1] as f32, z: r[2] as f32 }
    }

    /// The interpreter path of `Math:mul`.
    pub fn mul(a: Packed<Quaternion>, b: Packed<Quaternion>) -> Packed<Quaternion> {
        Packed(Quaternion(a.0.0 * b.0.0))
    }

    /// The interpreter path of `Math:slerp`; a non-finite `t` is an error, as on the module.
    pub fn slerp(a: Packed<Quaternion>, b: Packed<Quaternion>, t: f64) -> crate::error::Result<Packed<Quaternion>> {
        super::finite_weight(t)?;
        Ok(Packed(Quaternion(a.0.0.slerp(b.0.0, t))))
    }

    /// The hook set; [`super::QuatExtension`] registers it.
    pub struct Lowering;

    const KIND: i64 = <Quaternion as PackedScalar>::KIND as i64;
    const KEY_KIND: i64 = <AnimationKey as PackedScalar>::KIND as i64;
    const LANE_MASK: i64 = (1 << COMPONENT_BITS) - 1;
    const PAYLOAD_MASK: i64 = crate::packed::PAYLOAD_MASK as i64;
    const FLAG_SHIFT: i64 = crate::packed::PAYLOAD_BITS as i64;

    /// A decoded rotation as four double IR values.
    struct Decoded {
        x: IrOp,
        y: IrOp,
        z: IrOp,
        w: IrOp,
    }

    fn num(build: &mut IrBuilder<'_>, value: f64) -> IrOp {
        build.const_double(value)
    }

    fn i64c(build: &mut IrBuilder<'_>, value: i64) -> IrOp {
        build.const_int64(value)
    }

    fn op(build: &mut IrBuilder<'_>, cmd: IrCmd, a: IrOp, b: IrOp) -> IrOp {
        build.inst(cmd, &[a, b])
    }

    /// `if l == which then then_value else else_value` on doubles (`SELECT_NUM` compares equal).
    fn select(build: &mut IrBuilder<'_>, l: IrOp, which: f64, then_value: IrOp, else_value: IrOp) -> IrOp {
        let which = num(build, which);
        build.inst(IrCmd::SELECT_NUM, &[else_value, then_value, l, which])
    }

    /// Checks the register holds an integer of packed kind `expected_kind` and loads its bits;
    /// anything else exits to the interpreter.
    fn checked_bits(build: &mut IrBuilder<'_>, reg: IrOp, expected_kind: i64, exit: IrOp) -> IrOp {
        build.load_and_check_tag(reg, LUA_TINTEGER as u8, exit);
        let bits = build.inst(IrCmd::LOAD_INT64, &[reg]);
        let sixty = i64c(build, 60);
        let fifteen = i64c(build, 15);
        let kind = op(build, IrCmd::BITRSHIFT_INT64, bits, sixty);
        let kind = op(build, IrCmd::BITAND_INT64, kind, fifteen);
        let expected = i64c(build, expected_kind);
        let equal = build.cond(IrCondition::Equal);
        build.inst(IrCmd::CHECK_CMP_INT64, &[kind, expected, equal, exit]);
        bits
    }

    fn store_integer(build: &mut IrBuilder<'_>, result: IrOp, bits: IrOp) {
        build.inst(IrCmd::STORE_INT64, &[result, bits]);
        let integer_tag = build.const_tag(LUA_TINTEGER as u8);
        build.inst(IrCmd::STORE_TAG, &[result, integer_tag]);
    }

    /// Checks the register holds a packed `Quaternion` and unpacks it: three 18-bit lanes and
    /// the omitted component rebuilt from the unit norm.
    fn decode(build: &mut IrBuilder<'_>, reg: IrOp, exit: IrOp) -> Decoded {
        let bits = checked_bits(build, reg, KIND, exit);
        decode_bits(build, bits)
    }

    /// Unpacks already-checked bits.
    fn decode_bits(build: &mut IrBuilder<'_>, bits: IrOp) -> Decoded {
        let scale = num(build, 2.0 * RANGE / COMPONENT_MAX);
        let offset = num(build, -RANGE);
        let mask = i64c(build, LANE_MASK);
        let mut lanes = [bits; 3];
        for (slot, shift) in [(0usize, 2 * COMPONENT_BITS), (1, COMPONENT_BITS), (2, 0)] {
            let shifted = if shift == 0 {
                bits
            } else {
                let amount = i64c(build, i64::from(shift));
                op(build, IrCmd::BITRSHIFT_INT64, bits, amount)
            };
            let lane = op(build, IrCmd::BITAND_INT64, shifted, mask);
            let lane = build.inst(IrCmd::INT64_TO_NUM, &[lane]);
            let lane = op(build, IrCmd::MUL_NUM, lane, scale);
            lanes[slot] = op(build, IrCmd::ADD_NUM, lane, offset);
        }
        let [a, b, c] = lanes;
        let largest_shift = i64c(build, i64::from(3 * COMPONENT_BITS));
        let three = i64c(build, 3);
        let largest = op(build, IrCmd::BITRSHIFT_INT64, bits, largest_shift);
        let largest = op(build, IrCmd::BITAND_INT64, largest, three);
        let l = build.inst(IrCmd::INT64_TO_NUM, &[largest]);

        let aa = op(build, IrCmd::MUL_NUM, a, a);
        let bb = op(build, IrCmd::MUL_NUM, b, b);
        let cc = op(build, IrCmd::MUL_NUM, c, c);
        let sum = op(build, IrCmd::ADD_NUM, aa, bb);
        let sum = op(build, IrCmd::ADD_NUM, sum, cc);
        let one = num(build, 1.0);
        let zero = num(build, 0.0);
        let rest = op(build, IrCmd::SUB_NUM, one, sum);
        let rest = op(build, IrCmd::MAX_NUM, zero, rest);
        let big = build.inst(IrCmd::SQRT_NUM, &[rest]);

        // Place the three lanes around the omitted component by its index.
        let x = select(build, l, 0.0, big, a);
        let y_else = select(build, l, 1.0, big, b);
        let y = select(build, l, 0.0, a, y_else);
        let z_else = select(build, l, 2.0, big, b);
        let z = select(build, l, 3.0, c, z_else);
        let w = select(build, l, 3.0, big, c);
        Decoded { x, y, z, w }
    }

    /// Packs four doubles (a unit quaternion) into the integer bit pattern, branch-free.
    fn encode(build: &mut IrBuilder<'_>, q: &Decoded) -> IrOp {
        let c = [q.x, q.y, q.z, q.w];
        let abs: Vec<IrOp> = c.iter().map(|v| build.inst(IrCmd::ABS_NUM, &[*v])).collect();
        let m01 = op(build, IrCmd::MAX_NUM, abs[0], abs[1]);
        let m23 = op(build, IrCmd::MAX_NUM, abs[2], abs[3]);
        let m = op(build, IrCmd::MAX_NUM, m01, m23);
        // Index of the largest magnitude (ties to the lowest index), as a double for selects.
        let three = num(build, 3.0);
        let two = num(build, 2.0);
        let one = num(build, 1.0);
        let zero = num(build, 0.0);
        let l = build.inst(IrCmd::SELECT_NUM, &[three, two, abs[2], m]);
        let l = build.inst(IrCmd::SELECT_NUM, &[l, one, abs[1], m]);
        let l = build.inst(IrCmd::SELECT_NUM, &[l, zero, abs[0], m]);
        // Sign: the largest component is negative exactly when min(c) == -m.
        let n01 = op(build, IrCmd::MIN_NUM, c[0], c[1]);
        let n23 = op(build, IrCmd::MIN_NUM, c[2], c[3]);
        let n = op(build, IrCmd::MIN_NUM, n01, n23);
        let neg_m = op(build, IrCmd::SUB_NUM, zero, m);
        let minus_one = num(build, -1.0);
        let sign = build.inst(IrCmd::SELECT_NUM, &[one, minus_one, n, neg_m]);
        // Quantise every lane: q = trunc(clamp01((c * sign / RANGE + 1) / 2) * MAX + 0.5).
        let scale = num(build, 0.5 / RANGE);
        let half = num(build, 0.5);
        let max = num(build, COMPONENT_MAX);
        let mut lanes = [zero; 4];
        for (i, component) in c.iter().enumerate() {
            let v = op(build, IrCmd::MUL_NUM, *component, sign);
            let v = op(build, IrCmd::MUL_NUM, v, scale);
            let v = op(build, IrCmd::ADD_NUM, v, half);
            let v = op(build, IrCmd::MAX_NUM, zero, v);
            let v = op(build, IrCmd::MIN_NUM, one, v);
            let v = op(build, IrCmd::MUL_NUM, v, max);
            let v = op(build, IrCmd::ADD_NUM, v, half);
            lanes[i] = build.inst(IrCmd::NUM_TO_INT64, &[v]);
        }
        // Kept lanes in index order, skipping the largest.
        let l64 = build.inst(IrCmd::NUM_TO_INT64, &[l]);
        let equal = build.cond(IrCondition::Equal);
        let k0 = i64c(build, 0);
        let k1 = i64c(build, 1);
        let k3 = i64c(build, 3);
        let lane_a = build.inst(IrCmd::SELECT_INT64, &[lanes[0], lanes[1], l64, k0, equal]);
        let lane_b = build.inst(IrCmd::SELECT_INT64, &[lanes[1], lanes[2], l64, k0, equal]);
        let lane_b = build.inst(IrCmd::SELECT_INT64, &[lane_b, lanes[2], l64, k1, equal]);
        let lane_c = build.inst(IrCmd::SELECT_INT64, &[lanes[3], lanes[2], l64, k3, equal]);
        let s54 = i64c(build, i64::from(3 * COMPONENT_BITS));
        let s36 = i64c(build, i64::from(2 * COMPONENT_BITS));
        let s18 = i64c(build, i64::from(COMPONENT_BITS));
        let head = op(build, IrCmd::BITLSHIFT_INT64, l64, s54);
        let a = op(build, IrCmd::BITLSHIFT_INT64, lane_a, s36);
        let b = op(build, IrCmd::BITLSHIFT_INT64, lane_b, s18);
        let bits = op(build, IrCmd::BITOR_INT64, head, a);
        let bits = op(build, IrCmd::BITOR_INT64, bits, b);
        let bits = op(build, IrCmd::BITOR_INT64, bits, lane_c);
        let kind = i64c(build, KIND << 60);
        op(build, IrCmd::BITOR_INT64, bits, kind)
    }

    fn cross(build: &mut IrBuilder<'_>, a: [IrOp; 3], b: [IrOp; 3]) -> [IrOp; 3] {
        let mut out = [a[0]; 3];
        for (i, slot) in out.iter_mut().enumerate() {
            let (j, k) = ((i + 1) % 3, (i + 2) % 3);
            let p = op(build, IrCmd::MUL_NUM, a[j], b[k]);
            let q = op(build, IrCmd::MUL_NUM, a[k], b[j]);
            *slot = op(build, IrCmd::SUB_NUM, p, q);
        }
        out
    }

    /// Horner evaluation of `coefficients` (lowest degree first) at `x`.
    fn horner(build: &mut IrBuilder<'_>, coefficients: &[f64], x: IrOp) -> IrOp {
        let mut acc = num(build, coefficients[coefficients.len() - 1]);
        for c in coefficients[..coefficients.len() - 1].iter().rev() {
            let c = num(build, *c);
            let scaled = op(build, IrCmd::MUL_NUM, acc, x);
            acc = op(build, IrCmd::ADD_NUM, scaled, c);
        }
        acc
    }

    /// [`super::acos_poly`] in IR.
    fn acos_poly(build: &mut IrBuilder<'_>, x: IrOp) -> IrOp {
        let poly = horner(build, &super::ACOS_COEFFICIENTS, x);
        let one = num(build, 1.0);
        let rest = op(build, IrCmd::SUB_NUM, one, x);
        let root = build.inst(IrCmd::SQRT_NUM, &[rest]);
        op(build, IrCmd::MUL_NUM, root, poly)
    }

    /// [`super::sin_poly`] in IR.
    fn sin_poly(build: &mut IrBuilder<'_>, x: IrOp) -> IrOp {
        let x2 = op(build, IrCmd::MUL_NUM, x, x);
        let poly = horner(build, &super::SIN_COEFFICIENTS, x2);
        op(build, IrCmd::MUL_NUM, x, poly)
    }

    /// `Q:slerp(a, b, t)` in one block: both weight sets are computed and the linear pair is
    /// selected when `max(dot, 0.9995) == dot`, so nothing branches, nothing is decoded twice,
    /// and no value stays live across a block boundary. The spherical weights are NaN when the
    /// inputs coincide (sin(theta) is zero) but are never selected then. Same formulas as the
    /// interpreter's [`super::Quat::slerp`], polynomial trigonometry included.
    fn lower_slerp(build: &mut IrBuilder<'_>, result: IrOp, a_reg: IrOp, b_reg: IrOp, t_reg: c_int, exit: IrOp) {
        let t_reg = build.vm_reg(t_reg);
        // A non-finite weight exits to the binder, which raises: `t - t` is zero exactly for
        // finite `t` and NaN for NaN or an infinity, and `NotEqual` is true for NaN. A jump
        // needs blocks for both targets; the bail block is one jump to the VM exit.
        build.load_and_check_tag(t_reg, LUA_TNUMBER as u8, exit);
        let weight = build.inst(IrCmd::LOAD_DOUBLE, &[t_reg]);
        let difference = op(build, IrCmd::SUB_NUM, weight, weight);
        let zero = num(build, 0.0);
        let not_equal = build.cond(IrCondition::NotEqual);
        let bail = build.block(IrBlockKind::Internal);
        let finite = build.block(IrBlockKind::Internal);
        build.inst(IrCmd::JUMP_CMP_NUM, &[difference, zero, not_equal, bail, finite]);
        build.begin_block(bail);
        build.inst(IrCmd::JUMP, &[exit]);
        build.begin_block(finite);
        let a = decode(build, a_reg, exit);
        let b = decode(build, b_reg, exit);
        let t = build.inst(IrCmd::LOAD_DOUBLE, &[t_reg]);
        let one = num(build, 1.0);
        let zero = num(build, 0.0);
        let t = build.inst(IrCmd::MIN_NUM, &[one, t]);
        let t = build.inst(IrCmd::MAX_NUM, &[zero, t]);
        let d = product_sum(build, &a, &b);
        // Take the shorter arc: flip `b` when the dot is negative.
        let minus_one = num(build, -1.0);
        let raw_sign = build.inst(IrCmd::SIGN_NUM, &[d]);
        let sign = build.inst(IrCmd::SELECT_NUM, &[one, minus_one, raw_sign, minus_one]);
        let dot = build.inst(IrCmd::ABS_NUM, &[d]);
        // Spherical weights.
        let theta = acos_poly(build, dot);
        let dd = op(build, IrCmd::MUL_NUM, dot, dot);
        let rest = op(build, IrCmd::SUB_NUM, one, dd);
        let sin_theta = build.inst(IrCmd::SQRT_NUM, &[rest]);
        let one_minus_t = op(build, IrCmd::SUB_NUM, one, t);
        let angle0 = op(build, IrCmd::MUL_NUM, one_minus_t, theta);
        let angle1 = op(build, IrCmd::MUL_NUM, t, theta);
        let sin0 = sin_poly(build, angle0);
        let sin1 = sin_poly(build, angle1);
        let inv_sin_theta = op(build, IrCmd::DIV_NUM, one, sin_theta);
        let spherical0 = op(build, IrCmd::MUL_NUM, sin0, inv_sin_theta);
        let spherical1 = op(build, IrCmd::MUL_NUM, sin1, inv_sin_theta);
        // Nearly parallel: linear weights, selected when max(dot, threshold) == dot.
        let threshold = num(build, 0.9995);
        let clamped = build.inst(IrCmd::MAX_NUM, &[dot, threshold]);
        let s0 = build.inst(IrCmd::SELECT_NUM, &[spherical0, one_minus_t, clamped, dot]);
        let s1 = build.inst(IrCmd::SELECT_NUM, &[spherical1, t, clamped, dot]);
        let s1 = op(build, IrCmd::MUL_NUM, s1, sign);
        let mut r = [a.x, a.y, a.z, a.w];
        for (slot, (from, to)) in r.iter_mut().zip([(a.x, b.x), (a.y, b.y), (a.z, b.z), (a.w, b.w)]) {
            let weighted_a = op(build, IrCmd::MUL_NUM, s0, from);
            let weighted_b = op(build, IrCmd::MUL_NUM, s1, to);
            *slot = op(build, IrCmd::ADD_NUM, weighted_a, weighted_b);
        }
        let unit = normalized(build, r);
        let bits = encode(build, &unit);
        store_integer(build, result, bits);
    }

    /// Four components divided by their norm.
    fn normalized(build: &mut IrBuilder<'_>, mut r: [IrOp; 4]) -> Decoded {
        let squares: Vec<IrOp> = r.iter().map(|v| op(build, IrCmd::MUL_NUM, *v, *v)).collect();
        let sum = op(build, IrCmd::ADD_NUM, squares[0], squares[1]);
        let sum = op(build, IrCmd::ADD_NUM, sum, squares[2]);
        let sum = op(build, IrCmd::ADD_NUM, sum, squares[3]);
        let norm = build.inst(IrCmd::SQRT_NUM, &[sum]);
        let one = num(build, 1.0);
        let inv = op(build, IrCmd::DIV_NUM, one, norm);
        for slot in &mut r {
            *slot = op(build, IrCmd::MUL_NUM, *slot, inv);
        }
        Decoded { x: r[0], y: r[1], z: r[2], w: r[3] }
    }

    /// The dot product of two decoded rotations.
    fn product_sum(build: &mut IrBuilder<'_>, a: &Decoded, b: &Decoded) -> IrOp {
        let xx = op(build, IrCmd::MUL_NUM, a.x, b.x);
        let yy = op(build, IrCmd::MUL_NUM, a.y, b.y);
        let zz = op(build, IrCmd::MUL_NUM, a.z, b.z);
        let ww = op(build, IrCmd::MUL_NUM, a.w, b.w);
        let sum = op(build, IrCmd::ADD_NUM, xx, yy);
        let sum = op(build, IrCmd::ADD_NUM, sum, zz);
        op(build, IrCmd::ADD_NUM, sum, ww)
    }

    /// Hamilton product of two decoded rotations.
    fn product(build: &mut IrBuilder<'_>, a: &Decoded, b: &Decoded) -> Decoded {
        let term = |build: &mut IrBuilder<'_>, terms: [(IrOp, IrOp, bool); 4]| {
            let mut acc: Option<IrOp> = None;
            for (l, r, negative) in terms {
                let product = op(build, IrCmd::MUL_NUM, l, r);
                acc = Some(match acc {
                    None => product,
                    Some(acc) => op(build, if negative { IrCmd::SUB_NUM } else { IrCmd::ADD_NUM }, acc, product),
                });
            }
            acc.expect("four terms")
        };
        Decoded {
            x: term(build, [(a.w, b.x, false), (a.x, b.w, false), (a.y, b.z, false), (a.z, b.y, true)]),
            y: term(build, [(a.w, b.y, false), (a.x, b.z, true), (a.y, b.w, false), (a.z, b.x, false)]),
            z: term(build, [(a.w, b.z, false), (a.x, b.y, false), (a.y, b.x, true), (a.z, b.w, false)]),
            w: term(build, [(a.w, b.w, false), (a.x, b.x, true), (a.y, b.y, true), (a.z, b.z, true)]),
        }
    }

    impl NativeCodeHooks for Lowering {
        fn userdata_namecall_type(&self, context: &NativeContext<'_>, userdata_type: u8, member: &str) -> u8 {
            if context.userdata_type_of::<Math>() != Some(userdata_type) {
                return bytecode_type::ANY;
            }
            match member {
                "rotate" => bytecode_type::VECTOR,
                "mul" | "slerp" | "key" | "keyRotation" => bytecode_type::INTEGER,
                "keyFlags" => bytecode_type::NUMBER,
                _ => bytecode_type::ANY,
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
            // Single-result sites only: `return Q:mul(...)` asks for LUA_MULTRET.
            if context.userdata_type_of::<Math>() != Some(userdata_type) || site.results != 1 {
                return false;
            }
            let Some(tag) = context.tag_of::<Math>() else { return false };
            let exit = build.vm_exit(site.pcpos);
            let receiver = build.vm_reg(site.source_reg);
            let pointer = build.inst(IrCmd::LOAD_POINTER, &[receiver]);
            let tag = build.const_int(i32::from(tag));
            build.inst(IrCmd::CHECK_USERDATA_TAG, &[pointer, tag, exit]);
            // ra is the function slot, ra + 1 the receiver copy the skipped NAMECALL would have
            // made; arguments start at ra + 2. `params` counts the receiver.
            let result = build.vm_reg(site.arg_res_reg);
            let first = build.vm_reg(site.arg_res_reg + 2);
            let second = build.vm_reg(site.arg_res_reg + 3);
            match (member, site.params) {
                ("rotate", 3) => {
                    let q = decode(build, first, exit);
                    build.load_and_check_tag(second, LUA_TVECTOR as u8, exit);
                    let vector = build.inst(IrCmd::LOAD_TVALUE, &[second]);
                    let mut v = [q.x; 3];
                    for (i, slot) in v.iter_mut().enumerate() {
                        let index = build.const_int(i32::try_from(i).expect("three lanes"));
                        let component = build.inst(IrCmd::EXTRACT_VEC, &[vector, index]);
                        *slot = build.inst(IrCmd::FLOAT_TO_NUM, &[component]);
                    }
                    // v' = v + w t + q × t, with t = 2 (q × v).
                    let qv = [q.x, q.y, q.z];
                    let t = cross(build, qv, v);
                    let two = num(build, 2.0);
                    let t = [
                        op(build, IrCmd::MUL_NUM, t[0], two),
                        op(build, IrCmd::MUL_NUM, t[1], two),
                        op(build, IrCmd::MUL_NUM, t[2], two),
                    ];
                    let qt = cross(build, qv, t);
                    let mut out = [q.x; 3];
                    for i in 0..3 {
                        let wt = op(build, IrCmd::MUL_NUM, q.w, t[i]);
                        let sum = op(build, IrCmd::ADD_NUM, v[i], wt);
                        let sum = op(build, IrCmd::ADD_NUM, sum, qt[i]);
                        out[i] = build.inst(IrCmd::NUM_TO_FLOAT, &[sum]);
                    }
                    let vector_tag = build.const_tag(LUA_TVECTOR as u8);
                    build.inst(IrCmd::STORE_VECTOR, &[result, out[0], out[1], out[2], vector_tag]);
                }
                ("mul", 3) => {
                    let a = decode(build, first, exit);
                    let b = decode(build, second, exit);
                    let product = product(build, &a, &b);
                    let bits = encode(build, &product);
                    store_integer(build, result, bits);
                }
                ("slerp", 4) => lower_slerp(build, result, first, second, site.arg_res_reg + 4, exit),
                ("key", 3) => {
                    // `flags` must be an exact integer, as the binder's `Exact` demands: a number
                    // that does not survive the round trip through int64 (a fraction, NaN, out
                    // of range) exits so the binder raises the same error on both paths.
                    build.load_and_check_tag(second, LUA_TNUMBER as u8, exit);
                    let number = build.inst(IrCmd::LOAD_DOUBLE, &[second]);
                    let truncated = build.inst(IrCmd::NUM_TO_INT64, &[number]);
                    let back = build.inst(IrCmd::INT64_TO_NUM, &[truncated]);
                    // `NotEqual` is true for NaN as well, which is the exit we want (`Equal` is
                    // the one condition the number compare does not implement). A jump needs
                    // blocks for both targets; the bail block is one jump to the VM exit.
                    let not_equal = build.cond(IrCondition::NotEqual);
                    let bail = build.block(IrBlockKind::Internal);
                    let exact = build.block(IrBlockKind::Internal);
                    build.inst(IrCmd::JUMP_CMP_NUM, &[number, back, not_equal, bail, exact]);
                    build.begin_block(bail);
                    build.inst(IrCmd::JUMP, &[exit]);
                    build.begin_block(exact);
                    // The rotation payload with the low four bits of `flags` and kind 2.
                    let q = checked_bits(build, first, KIND, exit);
                    let flags = build.inst(IrCmd::LOAD_DOUBLE, &[second]);
                    let flags = build.inst(IrCmd::NUM_TO_INT64, &[flags]);
                    let fifteen = i64c(build, 15);
                    let flags = op(build, IrCmd::BITAND_INT64, flags, fifteen);
                    let shift = i64c(build, FLAG_SHIFT);
                    let flags = op(build, IrCmd::BITLSHIFT_INT64, flags, shift);
                    let payload_mask = i64c(build, PAYLOAD_MASK);
                    let payload = op(build, IrCmd::BITAND_INT64, q, payload_mask);
                    let bits = op(build, IrCmd::BITOR_INT64, payload, flags);
                    let kind = i64c(build, KEY_KIND << 60);
                    let bits = op(build, IrCmd::BITOR_INT64, bits, kind);
                    store_integer(build, result, bits);
                }
                ("keyRotation", 2) => {
                    let k = checked_bits(build, first, KEY_KIND, exit);
                    let payload_mask = i64c(build, PAYLOAD_MASK);
                    let payload = op(build, IrCmd::BITAND_INT64, k, payload_mask);
                    let kind = i64c(build, KIND << 60);
                    let bits = op(build, IrCmd::BITOR_INT64, payload, kind);
                    store_integer(build, result, bits);
                }
                ("keyFlags", 2) => {
                    let k = checked_bits(build, first, KEY_KIND, exit);
                    let shift = i64c(build, FLAG_SHIFT);
                    let flags = op(build, IrCmd::BITRSHIFT_INT64, k, shift);
                    let fifteen = i64c(build, 15);
                    let flags = op(build, IrCmd::BITAND_INT64, flags, fifteen);
                    let flags = build.inst(IrCmd::INT64_TO_NUM, &[flags]);
                    build.inst(IrCmd::STORE_DOUBLE, &[result, flags]);
                    let number_tag = build.const_tag(LUA_TNUMBER as u8);
                    build.inst(IrCmd::STORE_TAG, &[result, number_tag]);
                }
                _ => return false,
            }
            LOWERED.fetch_add(1, Ordering::Relaxed);
            true
        }
    }
}

#[cfg(test)]
#[allow(clippy::many_single_char_names)]
mod tests {
    use super::*;

    fn random_quat(rng: &mut u64) -> Quat {
        let mut next = || {
            *rng ^= *rng << 13;
            *rng ^= *rng >> 7;
            *rng ^= *rng << 17;
            (*rng >> 11) as f64 / (1u64 << 53) as f64
        };
        let (u1, u2, u3) = (next(), next(), next());
        let (a, b) = ((1.0 - u1).sqrt(), u1.sqrt());
        Quat {
            x: a * (2.0 * std::f64::consts::PI * u2).sin(),
            y: a * (2.0 * std::f64::consts::PI * u2).cos(),
            z: b * (2.0 * std::f64::consts::PI * u3).sin(),
            w: b * (2.0 * std::f64::consts::PI * u3).cos(),
        }
    }

    #[test]
    fn smallest_three_precision_over_random_rotations() {
        let mut rng = 0x9E37_79B9_7F4A_7C15u64;
        let (mut max_error, mut total) = (0.0f64, 0.0f64);
        const N: usize = 100_000;
        for _ in 0..N {
            let q = random_quat(&mut rng);
            let back = PackedRotation::encode(q).decode();
            let error = q.angle_to(back);
            max_error = max_error.max(error);
            total += error;
        }
        assert!(max_error < 2.0e-5, "max error {max_error}");
        let mean = total / N as f64;
        assert!(mean < 1.0e-5, "mean error {mean}");
        assert!(PackedRotation::encode(Quat::IDENTITY).0 < (1 << 56));
        assert!(Quat::IDENTITY.angle_to(PackedRotation::encode(Quat::IDENTITY).decode()) < 1e-9);
    }

    #[test]
    fn polynomial_trigonometry_is_accurate_on_its_domain() {
        let mut worst_acos: f64 = 0.0;
        let mut worst_sin: f64 = 0.0;
        for i in 0..=100_000 {
            let x = f64::from(i) / 100_000.0;
            worst_acos = worst_acos.max((acos_poly(x) - x.acos()).abs());
            let angle = x * std::f64::consts::FRAC_PI_2;
            worst_sin = worst_sin.max((sin_poly(angle) - angle.sin()).abs());
        }
        assert!(worst_acos < 3e-8, "acos error {worst_acos:e}");
        assert!(worst_sin < 6e-8, "sin error {worst_sin:e}");
        // Slerp against the exact formula.
        let mut rng = 11u64;
        let mut worst: f64 = 0.0;
        for i in 0..2000 {
            let (a, b) = (random_quat(&mut rng), random_quat(&mut rng));
            let t = f64::from(i % 101) / 100.0;
            let exact = {
                let mut o = b;
                let mut d = a.dot(o);
                if d < 0.0 {
                    o = Quat { x: -o.x, y: -o.y, z: -o.z, w: -o.w };
                    d = -d;
                }
                if d >= 0.9995 {
                    a.slerp(b, t)
                } else {
                    let theta = d.acos();
                    let (s0, s1) = (((1.0 - t) * theta).sin() / theta.sin(), (t * theta).sin() / theta.sin());
                    Quat { x: s0 * a.x + s1 * o.x, y: s0 * a.y + s1 * o.y, z: s0 * a.z + s1 * o.z, w: s0 * a.w + s1 * o.w }
                }
            };
            worst = worst.max(a.slerp(b, t).angle_to(exact));
        }
        assert!(worst < 5e-7, "slerp error {worst:e}");
    }

    #[test]
    fn rotate_matches_the_double_product() {
        let mut rng = 7u64;
        for _ in 0..1000 {
            let q = random_quat(&mut rng);
            let v = [1.0, -2.0, 0.5];
            let p = Quat { x: v[0], y: v[1], z: v[2], w: 0.0 };
            let r = q * p * q.inverse();
            let fast = q.rotate(v);
            for i in 0..3 {
                assert!((fast[i] - [r.x, r.y, r.z][i]).abs() < 1e-12);
            }
        }
    }

    #[test]
    fn animation_keys_are_a_distinct_kind() {
        let key = AnimationKey::pack(Quat::IDENTITY, 0x19);
        let back = Packed::<AnimationKey>::from_bits(key.bits().unwrap()).unwrap();
        assert_eq!(back.0.flags, 9, "four flag bits");
        assert_eq!(back.0.rotation, Quat::IDENTITY);
        assert!(Packed::<Quaternion>::from_bits(key.bits().unwrap()).is_err());
        assert!(Packed::<AnimationKey>::from_bits(Quaternion::pack(Quat::IDENTITY).bits().unwrap()).is_err());
    }

    #[test]
    fn packed_form_is_storage_not_an_accumulator() {
        let mut rng = 0xDEAD_BEEF_CAFE_F00Du64;
        let target = random_quat(&mut rng);
        let mut reference = Quat::IDENTITY;
        let mut chained = PackedRotation::encode(Quat::IDENTITY);
        let (mut worst_chained, mut worst_snapshot) = (0.0f64, 0.0f64);
        for step in 0..20_000 {
            let t = 0.001 + 0.004 * ((step % 100) as f64 / 100.0);
            reference = reference.slerp(target, t).normalize();
            chained = PackedRotation::encode(chained.decode().slerp(target, t));
            worst_chained = worst_chained.max(reference.angle_to(chained.decode()));
            worst_snapshot = worst_snapshot.max(reference.angle_to(PackedRotation::encode(reference).decode()));
        }
        assert!(worst_snapshot < 2.0e-5, "snapshot error {worst_snapshot}");
        assert!(worst_chained > worst_snapshot * 10.0, "expected the chained form to accumulate; got {worst_chained}");
    }
}
