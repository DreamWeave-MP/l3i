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

pub const COMPONENT_BITS: u32 = 18;
/// One fewer level than the bit width allows, so the grid has an exact zero (identity and
/// axis-aligned rotations round-trip exactly).
pub const COMPONENT_MAX: f64 = ((1u64 << COMPONENT_BITS) - 2) as f64;
pub const RANGE: f64 = std::f64::consts::FRAC_1_SQRT_2;

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
        #[cfg(feature = "jit")]
        {
            let receiver = d.userdata::<lowering::QuatMath>("dream.quat.Math");
            receiver.tag(TagPolicy::Required);
            receiver.method("rotate").direct();
            receiver.method("mul").direct();
            d.native_hooks(lowering::PackedQuatLowering);
        }
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
        #[cfg(feature = "jit")]
        cx.userdata::<lowering::QuatMath>("dream.quat.Math")?
            .method("rotate", |_: &lowering::QuatMath, q: Packed<Quaternion>, v: Vector3| lowering::rotate(q, v))?
            .method("mul", |_: &lowering::QuatMath, a: Packed<Quaternion>, b: Packed<Quaternion>| lowering::mul(a, b))?;
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
        #[cfg(feature = "jit")]
        module.function("math", || Owned(lowering::QuatMath))?;
        module.finish()?;
        Ok(())
    }
}

/// A `Call`-free helper for tests: the identity of the experiment's tag.
pub fn _unused(_: &Call<'_>) {}

// ---- native lowering (jit) -------------------------------------------------------------------

/// The lowering half of the experiment: a tagged receiver whose methods take packed integers,
/// so `Q:rotate(q, v)` and `Q:mul(a, b)` can be lowered through the userdata namecall hook into
/// pure IR (integer unpacking, double math, vector or integer store) with no C call.
#[cfg(feature = "jit")]
// Quaternion algebra reads best with the conventional single letters.
#[allow(clippy::many_single_char_names)]
pub mod lowering {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use l3i::convert::Vector3;
    use l3i::ffi::{LUA_TINTEGER, LUA_TVECTOR};
    use l3i::native_code::hooks::{NamecallSite, NativeCodeHooks, NativeContext};
    use l3i::native_code::ir::{IrBuilder, IrCmd, IrCondition, IrOp, bytecode_type};
    use l3i::packed::Packed;
    use l3i::userdata::Userdata;

    use super::{COMPONENT_BITS, COMPONENT_MAX, Quat, Quaternion, RANGE};

    /// The receiver: a payload-free tagged userdata (`dream.quat.Math`, class `dream_quat_Math`).
    pub struct QuatMath;

    // SAFETY: no payload, no Lua references.
    unsafe impl Userdata for QuatMath {
        const NAME: &'static str = "dream.quat.Math";
    }

    /// How many call sites the hook lowered.
    pub static LOWERED: AtomicUsize = AtomicUsize::new(0);

    /// The interpreter fallbacks (the semantic oracle) the lowering must match.
    pub fn rotate(q: Packed<Quaternion>, v: Vector3) -> Vector3 {
        let r = q.0.0.rotate([f64::from(v.x), f64::from(v.y), f64::from(v.z)]);
        Vector3 { x: r[0] as f32, y: r[1] as f32, z: r[2] as f32 }
    }

    pub fn mul(a: Packed<Quaternion>, b: Packed<Quaternion>) -> Packed<Quaternion> {
        Packed(Quaternion(a.0.0.mul(b.0.0)))
    }

    pub struct PackedQuatLowering;

    const KIND: i64 = <Quaternion as l3i::packed::PackedScalar>::KIND as i64;
    const LANE_MASK: i64 = (1 << COMPONENT_BITS) - 1;

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

    /// Checks the register holds a packed `Quaternion` and unpacks it: three 18-bit lanes and
    /// the omitted component rebuilt from the unit norm.
    fn decode(build: &mut IrBuilder<'_>, reg: IrOp, exit: IrOp) -> Decoded {
        build.load_and_check_tag(reg, LUA_TINTEGER as u8, exit);
        let bits = build.inst(IrCmd::LOAD_INT64, &[reg]);
        let sixty = i64c(build, 60);
        let fifteen = i64c(build, 15);
        let kind = op(build, IrCmd::BITRSHIFT_INT64, bits, sixty);
        let kind = op(build, IrCmd::BITAND_INT64, kind, fifteen);
        let expected = i64c(build, KIND);
        let equal = build.cond(IrCondition::Equal);
        build.inst(IrCmd::CHECK_CMP_INT64, &[kind, expected, equal, exit]);

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

    impl NativeCodeHooks for PackedQuatLowering {
        fn userdata_namecall_type(&self, userdata_type: u8, member: &str) -> u8 {
            if userdata_type != bytecode_type::TAGGED_USERDATA_BASE {
                return bytecode_type::ANY;
            }
            match member {
                "rotate" => bytecode_type::VECTOR,
                "mul" => bytecode_type::INTEGER,
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
            // The experiment plans `dream.quat.Math` as the first tagged type (index 0).
            if userdata_type != bytecode_type::TAGGED_USERDATA_BASE || site.results != 1 {
                return false;
            }
            let Some(tag) = context.tag_of::<QuatMath>() else { return false };
            let exit = build.vm_exit(site.pcpos);
            let receiver = build.vm_reg(site.source_reg);
            let pointer = build.inst(IrCmd::LOAD_POINTER, &[receiver]);
            let tag = build.const_int(i32::from(tag));
            build.inst(IrCmd::CHECK_USERDATA_TAG, &[pointer, tag, exit]);
            let result = build.vm_reg(site.arg_res_reg);
            match (member, site.params) {
                ("rotate", 3) => {
                    // ra is the function slot, ra + 1 the receiver copy the skipped NAMECALL
                    // would have made; arguments start at ra + 2.
                    let q_reg = build.vm_reg(site.arg_res_reg + 2);
                    let v_reg = build.vm_reg(site.arg_res_reg + 3);
                    let q = decode(build, q_reg, exit);
                    build.load_and_check_tag(v_reg, LUA_TVECTOR as u8, exit);
                    let vector = build.inst(IrCmd::LOAD_TVALUE, &[v_reg]);
                    let mut v = [q.x; 3];
                    for (i, slot) in v.iter_mut().enumerate() {
                        let index = build.const_int(i32::try_from(i).expect("three lanes"));
                        let component = build.inst(IrCmd::EXTRACT_VEC, &[vector, index]);
                        *slot = build.inst(IrCmd::FLOAT_TO_NUM, &[component]);
                    }
                    // v' = v + w * t + qv x t, with t = 2 (qv x v).
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
                    let a_reg = build.vm_reg(site.arg_res_reg + 2);
                    let b_reg = build.vm_reg(site.arg_res_reg + 3);
                    let a = decode(build, a_reg, exit);
                    let b = decode(build, b_reg, exit);
                    let product = |build: &mut IrBuilder<'_>, terms: [(IrOp, IrOp, bool); 4]| {
                        let mut acc: Option<IrOp> = None;
                        for (l, r, negative) in terms {
                            let term = op(build, IrCmd::MUL_NUM, l, r);
                            acc = Some(match acc {
                                None => term,
                                Some(acc) => op(build, if negative { IrCmd::SUB_NUM } else { IrCmd::ADD_NUM }, acc, term),
                            });
                        }
                        acc.expect("four terms")
                    };
                    let x = product(build, [(a.w, b.x, false), (a.x, b.w, false), (a.y, b.z, false), (a.z, b.y, true)]);
                    let y = product(build, [(a.w, b.y, false), (a.x, b.z, true), (a.y, b.w, false), (a.z, b.x, false)]);
                    let z = product(build, [(a.w, b.z, false), (a.x, b.y, false), (a.y, b.x, true), (a.z, b.w, false)]);
                    let w = product(build, [(a.w, b.w, false), (a.x, b.x, true), (a.y, b.y, true), (a.z, b.z, true)]);
                    let bits = encode(build, &Decoded { x, y, z, w });
                    build.inst(IrCmd::STORE_INT64, &[result, bits]);
                    let integer_tag = build.const_tag(LUA_TINTEGER as u8);
                    build.inst(IrCmd::STORE_TAG, &[result, integer_tag]);
                }
                _ => return false,
            }
            LOWERED.fetch_add(1, Ordering::Relaxed);
            true
        }
    }

    /// Reference decode of the lowering's own lane layout, for tests.
    pub fn reference_decode(bits: i64) -> Quat {
        super::PackedRotation((bits as u64) & l3i::packed::PAYLOAD_MASK).decode()
    }
}
