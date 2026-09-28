//! The comparison baseline for the packed quaternion (`l3i::quat`): an f32 quaternion userdata
//! with the same operations. Shared by `tests/quat.rs` and `benches/packed_quat.rs`; not
//! public API.

#![allow(dead_code, clippy::cast_possible_truncation, clippy::cast_precision_loss, clippy::cast_sign_loss)]

use std::cell::Cell;

use l3i::Result;
use l3i::convert::Vector3;
use l3i::extension::{Extension, ExtensionDescriptor, TagPolicy};
use l3i::quat::Quat;
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
/// `a:rotate(v)`, `a:angleTo(b)`).
pub struct QuatBaseline;

impl Extension for QuatBaseline {
    fn id(&self) -> &'static str {
        "dream.quat.baseline"
    }

    fn describe(&self, d: &mut ExtensionDescriptor) -> Result<()> {
        let mut ud = d.userdata::<QuatUserdata>("dream.quat.Quat");
        ud.tag(TagPolicy::Required);
        ud.method("mul", |a: &QuatUserdata, b: &QuatUserdata| Owned(QuatUserdata::from(a.get() * b.get())));
        ud.method("slerp", |a: &QuatUserdata, b: &QuatUserdata, t: f64| {
            Owned(QuatUserdata::from(a.get().slerp(b.get(), t)))
        });
        ud.method("rotate", |a: &QuatUserdata, v: Vector3| to_vec3(a.get().rotate(from_vec3(v))));
        ud.method("angleTo", |a: &QuatUserdata, b: &QuatUserdata| a.get().angle_to(b.get()));
        d.module("@dream/quatud").function("axisAngle", |axis: Vector3, angle: f64| {
            Owned(QuatUserdata::from(Quat::from_axis_angle(from_vec3(axis), angle)))
        });
        Ok(())
    }
}
