//! Comparison baselines for the packed quaternion (`l3i::quat`): an f32 quaternion userdata
//! with the same operations, and a second packed kind (`AnimationKey`) to prove kinds are
//! distinct. Shared by `tests/quat.rs` and `benches/packed_quat.rs`; not public API.

#![allow(dead_code, clippy::cast_possible_truncation, clippy::cast_precision_loss, clippy::cast_sign_loss)]

use std::cell::Cell;

use l3i::Result;
use l3i::convert::Vector3;
use l3i::extension::{Extension, ExtensionDescriptor, InstallContext, TagPolicy};
use l3i::packed::{Packed, PackedScalar};
use l3i::quat::{PackedRotation, Quat, Quaternion};
use l3i::userdata::{Owned, Userdata};

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

/// A second packed kind: the same rotation plus four animation flag bits.
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

/// `@dream/quatud`: the userdata baseline (`axisAngle`, `a:mul(b)`, `a:slerp(b, t)`,
/// `a:rotate(v)`, `a:angleTo(b)`) and the `AnimationKey` kind (`key`, `keyRotation`, `keyFlags`).
pub struct QuatBaseline;

impl Extension for QuatBaseline {
    fn id(&self) -> &'static str {
        "dream.quat.baseline"
    }

    fn describe(&self, d: &mut ExtensionDescriptor) -> Result<()> {
        let ud = d.userdata::<QuatUserdata>("dream.quat.Quat");
        ud.tag(TagPolicy::Required);
        ud.method("mul");
        ud.method("slerp");
        ud.method("rotate");
        ud.method("angleTo");
        d.module("@dream/quatud");
        Ok(())
    }

    fn install(&self, cx: &mut InstallContext<'_>) -> Result<()> {
        cx.userdata::<QuatUserdata>("dream.quat.Quat")?
            .method("mul", |a: &QuatUserdata, b: &QuatUserdata| Owned(QuatUserdata::from(a.get() * b.get())))?
            .method("slerp", |a: &QuatUserdata, b: &QuatUserdata, t: f64| {
                Owned(QuatUserdata::from(a.get().slerp(b.get(), t)))
            })?
            .method("rotate", |a: &QuatUserdata, v: Vector3| to_vec3(a.get().rotate(from_vec3(v))))?
            .method("angleTo", |a: &QuatUserdata, b: &QuatUserdata| a.get().angle_to(b.get()))?;
        let mut module = cx.module("@dream/quatud")?;
        module
            .function("axisAngle", |axis: Vector3, angle: f64| {
                Owned(QuatUserdata::from(Quat::from_axis_angle(from_vec3(axis), angle)))
            })?
            .function("key", |q: Packed<Quaternion>, flags: i64| {
                Packed(AnimationKey { rotation: q.0.0, flags: flags as u8 })
            })?
            .function("keyRotation", |k: Packed<AnimationKey>| Packed(Quaternion(k.0.rotation)))?
            .function("keyFlags", |k: Packed<AnimationKey>| i64::from(k.0.flags))?;
        module.finish()?;
        Ok(())
    }
}
