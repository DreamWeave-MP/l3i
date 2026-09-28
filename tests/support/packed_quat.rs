//! The packed-quaternion experiment (architecture §28.1): smallest-three compression into the
//! 56-bit payload of an l3i packed scalar, beside an f32 quaternion userdata for comparison.
//! Shared by `tests/packed_quat.rs` and `benches/packed_quat.rs`; not public API.

#![allow(dead_code, clippy::cast_possible_truncation, clippy::cast_precision_loss, clippy::cast_sign_loss)]

use std::cell::Cell;

use l3i::Result;
use l3i::bind::Call;
use l3i::convert::Vector3;
use l3i::extension::{Extension, ExtensionDescriptor, InstallContext, TagPolicy};
use l3i::packed::{Packed, PackedScalar};
use l3i::userdata::{Owned, Userdata};

/// A unit quaternion in f64, the reference representation.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Quat {
    pub x: f64,
    pub y: f64,
    pub z: f64,
    pub w: f64,
}

impl Quat {
    pub const IDENTITY: Quat = Quat { x: 0.0, y: 0.0, z: 0.0, w: 1.0 };

    pub fn from_axis_angle(axis: [f64; 3], angle: f64) -> Quat {
        let len = (axis[0] * axis[0] + axis[1] * axis[1] + axis[2] * axis[2]).sqrt();
        let (s, c) = (angle / 2.0).sin_cos();
        Quat { x: axis[0] / len * s, y: axis[1] / len * s, z: axis[2] / len * s, w: c }
    }

    pub fn normalize(self) -> Quat {
        let n = (self.x * self.x + self.y * self.y + self.z * self.z + self.w * self.w).sqrt();
        Quat { x: self.x / n, y: self.y / n, z: self.z / n, w: self.w / n }
    }

    pub fn mul(self, o: Quat) -> Quat {
        Quat {
            x: self.w * o.x + self.x * o.w + self.y * o.z - self.z * o.y,
            y: self.w * o.y - self.x * o.z + self.y * o.w + self.z * o.x,
            z: self.w * o.z + self.x * o.y - self.y * o.x + self.z * o.w,
            w: self.w * o.w - self.x * o.x - self.y * o.y - self.z * o.z,
        }
    }

    pub fn dot(self, o: Quat) -> f64 {
        self.x * o.x + self.y * o.y + self.z * o.z + self.w * o.w
    }

    pub fn slerp(self, mut o: Quat, t: f64) -> Quat {
        let mut d = self.dot(o);
        if d < 0.0 {
            o = Quat { x: -o.x, y: -o.y, z: -o.z, w: -o.w };
            d = -d;
        }
        if d > 0.9995 {
            return Quat {
                x: self.x + (o.x - self.x) * t,
                y: self.y + (o.y - self.y) * t,
                z: self.z + (o.z - self.z) * t,
                w: self.w + (o.w - self.w) * t,
            }
            .normalize();
        }
        let theta = d.acos();
        let (s0, s1) = (((1.0 - t) * theta).sin() / theta.sin(), (t * theta).sin() / theta.sin());
        Quat {
            x: s0 * self.x + s1 * o.x,
            y: s0 * self.y + s1 * o.y,
            z: s0 * self.z + s1 * o.z,
            w: s0 * self.w + s1 * o.w,
        }
    }

    pub fn rotate(self, v: [f64; 3]) -> [f64; 3] {
        let q = Quat { x: v[0], y: v[1], z: v[2], w: 0.0 };
        let inv = Quat { x: -self.x, y: -self.y, z: -self.z, w: self.w };
        let r = self.mul(q).mul(inv);
        [r.x, r.y, r.z]
    }

    /// The rotation angle between two unit quaternions, in radians.
    pub fn angle_to(self, o: Quat) -> f64 {
        2.0 * self.dot(o).abs().min(1.0).acos()
    }
}

/// A deterministic random unit quaternion.
pub fn random_quat(rng: &mut u64) -> Quat {
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

// ---- smallest-three packing ------------------------------------------------------------------

const COMPONENT_BITS: u32 = 18;
/// One fewer level than the bit width allows, so the grid has an exact zero (identity and
/// axis-aligned rotations round-trip exactly).
const COMPONENT_MAX: f64 = ((1u64 << COMPONENT_BITS) - 2) as f64;
const RANGE: f64 = std::f64::consts::FRAC_1_SQRT_2;

/// A rotation packed into 56 bits: 2 bits for the omitted (largest) component, 3 × 18 bits.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PackedRotation(pub u64);

impl PackedRotation {
    pub fn encode(q: Quat) -> PackedRotation {
        // Skip the normalisation when the input is already unit (the common case); the sqrt and
        // four divisions were a third of the encode cost. A branch-free four-lane variant and an
        // f32 variant were both measured slower (the latter through a software fma), so this
        // stays scalar: the floor is the three float-to-integer conversions.
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
            // `normalized` is in [0, 1]: adding one half and truncating rounds to nearest without
            // the `round` library call.
            let quantized = (normalized * COMPONENT_MAX + 0.5) as u64;
            bits = (bits << COMPONENT_BITS) | quantized;
        }
        PackedRotation(bits)
    }

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

/// The packed scalar kind `Quaternion`: rotation only, flags always zero.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Quaternion(pub Quat);

impl PackedScalar for Quaternion {
    const KIND: u8 = 7;
    const NAME: &'static str = "Quaternion";
    fn pack(&self) -> (u64, u8) {
        (PackedRotation::encode(self.0).0, 0)
    }
    fn unpack(payload: u64, _flags: u8) -> Result<Self> {
        Ok(Quaternion(PackedRotation(payload).decode()))
    }
}

/// The packed scalar kind `AnimationKey`: the same rotation plus four animation flag bits.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct AnimationKey {
    pub rotation: Quat,
    pub flags: u8,
}

impl PackedScalar for AnimationKey {
    const KIND: u8 = 8;
    const NAME: &'static str = "AnimationKey";
    fn pack(&self) -> (u64, u8) {
        (PackedRotation::encode(self.rotation).0, self.flags & 0xF)
    }
    fn unpack(payload: u64, flags: u8) -> Result<Self> {
        Ok(AnimationKey { rotation: PackedRotation(payload).decode(), flags })
    }
}

// ---- the userdata baseline ------------------------------------------------------------------

/// An f32 quaternion as inline tagged userdata: the representation the packed one competes with.
pub struct QuatUserdata(pub Cell<[f32; 4]>);

unsafe impl Userdata for QuatUserdata {
    const NAME: &'static str = "dream.quat.Quat";
}

impl QuatUserdata {
    pub fn get(&self) -> Quat {
        let c = self.0.get();
        Quat { x: f64::from(c[0]), y: f64::from(c[1]), z: f64::from(c[2]), w: f64::from(c[3]) }
    }
    pub fn from(q: Quat) -> QuatUserdata {
        QuatUserdata(Cell::new([q.x as f32, q.y as f32, q.z as f32, q.w as f32]))
    }
}

fn to_vec3(v: [f64; 3]) -> Vector3 {
    Vector3 { x: v[0] as f32, y: v[1] as f32, z: v[2] as f32 }
}

fn from_vec3(v: Vector3) -> [f64; 3] {
    [f64::from(v.x), f64::from(v.y), f64::from(v.z)]
}

/// `@dream/quat`: the same operations over both representations, so a script can call
/// `quat.mul(a, b)` on packed integers and `a:mul(b)` on userdata.
pub struct QuatExtension;

impl Extension for QuatExtension {
    fn id(&self) -> &'static str {
        "dream.quat"
    }

    fn describe(&self, d: &mut ExtensionDescriptor) -> Result<()> {
        let ud = d.userdata::<QuatUserdata>("dream.quat.Quat");
        ud.tag(TagPolicy::Required);
        ud.method("mul").direct();
        ud.method("slerp").direct();
        ud.method("rotate").direct();
        ud.method("angleTo");
        d.module("@dream/quat");
        Ok(())
    }

    fn install(&self, cx: &mut InstallContext<'_>) -> Result<()> {
        cx.userdata::<QuatUserdata>("dream.quat.Quat")?
            .method("mul", |a: &QuatUserdata, b: &QuatUserdata| Owned(QuatUserdata::from(a.get().mul(b.get()))))?
            .method("slerp", |a: &QuatUserdata, b: &QuatUserdata, t: f64| {
                Owned(QuatUserdata::from(a.get().slerp(b.get(), t)))
            })?
            .method("rotate", |a: &QuatUserdata, v: Vector3| to_vec3(a.get().rotate(from_vec3(v))))?
            .method("angleTo", |a: &QuatUserdata, b: &QuatUserdata| a.get().angle_to(b.get()))?;
        let mut module = cx.module("@dream/quat")?;
        module
            .function("axisAngle", |axis: Vector3, angle: f64| {
                Packed(Quaternion(Quat::from_axis_angle(from_vec3(axis), angle)))
            })?
            .function("mul", |a: Packed<Quaternion>, b: Packed<Quaternion>| Packed(Quaternion(a.0.0.mul(b.0.0))))?
            .function("slerp", |a: Packed<Quaternion>, b: Packed<Quaternion>, t: f64| {
                Packed(Quaternion(a.0.0.slerp(b.0.0, t)))
            })?
            .function("rotate", |q: Packed<Quaternion>, v: Vector3| to_vec3(q.0.0.rotate(from_vec3(v))))?
            .function("angleTo", |a: Packed<Quaternion>, b: Packed<Quaternion>| a.0.0.angle_to(b.0.0))?
            .function("key", |q: Packed<Quaternion>, flags: i64| {
                Packed(AnimationKey { rotation: q.0.0, flags: flags as u8 })
            })?
            .function("keyRotation", |k: Packed<AnimationKey>| Packed(Quaternion(k.0.rotation)))?
            .function("keyFlags", |k: Packed<AnimationKey>| i64::from(k.0.flags))?
            .function("udAxisAngle", |axis: Vector3, angle: f64| {
                Owned(QuatUserdata::from(Quat::from_axis_angle(from_vec3(axis), angle)))
            })?
            .function("udMul", |a: &QuatUserdata, b: &QuatUserdata| Owned(QuatUserdata::from(a.get().mul(b.get()))))?
            .function("udSlerp", |a: &QuatUserdata, b: &QuatUserdata, t: f64| {
                Owned(QuatUserdata::from(a.get().slerp(b.get(), t)))
            })?
            .function("udRotate", |a: &QuatUserdata, v: Vector3| to_vec3(a.get().rotate(from_vec3(v))))?;
        module.finish()?;
        Ok(())
    }
}

/// A `Call`-free helper for tests: the identity of the experiment's tag.
pub fn _unused(_: &Call<'_>) {}
