//! Packed scalars in lowering hooks: the IR that loads, checks and stores one, for the carrier
//! this build uses (`crate::packed::carrier`).
//!
//! The check is the binder's, in IR: the Luau type tag, then the kind nibble in the bits. A wide
//! kind has no nibble and is checked by type tag alone, in both paths. The light userdata tag is
//! written on every store, for `typeof`, equality and table keys, and is never read: an exact tag
//! check costs lowered code more than the nibble check, and a binder that checked one thing and a
//! lowering that checked another would accept different operands. Every hook goes through these
//! three functions; none loads or stores a packed scalar by hand.
//!
//! On the light carrier the bits come out of `LOAD_POINTER` and go back through `STORE_POINTER`,
//! with the `*_INT64` commands in between: Luau's backends spill and lower a pointer and an int64
//! the same way and check neither operand's kind. Never answer `NUMBER` for a packed result, or
//! store one under a number tag, to dodge the store-forwarding stall of a reassigned result: that
//! would put a light userdata's bits where the optimizer believes a double lives.

use super::ir::{IrBuilder, IrCmd, IrCondition, IrOp, bytecode_type};
use crate::packed::carrier::LightTag;
use crate::raw::ffi;

/// The bytecode type a `*_type` hook answers for an operation whose result is a packed scalar.
/// Luau has no bytecode type for a light userdata, so it is `ANY`; the optimizer still knows the
/// tag of a value it has just seen stored.
#[cfg(l3i_carrier = "light")]
pub const RESULT_TYPE: u8 = bytecode_type::ANY;
/// The bytecode type a `*_type` hook answers for an operation whose result is a packed scalar.
#[cfg(l3i_carrier = "integer")]
pub const RESULT_TYPE: u8 = bytecode_type::INTEGER;

/// The Luau type tag of a packed scalar on this carrier.
#[cfg(l3i_carrier = "light")]
const TYPE_TAG: u8 = ffi::LUA_TLIGHTUSERDATA as u8;
#[cfg(l3i_carrier = "integer")]
const TYPE_TAG: u8 = ffi::LUA_TINTEGER as u8;

#[cfg(l3i_carrier = "light")]
const LOAD: IrCmd = IrCmd::LOAD_POINTER;
#[cfg(l3i_carrier = "integer")]
const LOAD: IrCmd = IrCmd::LOAD_INT64;

/// Guards that `reg` holds a packed scalar of nibble kind `kind` and returns its 64 bits; anything
/// else exits to `exit`.
pub fn load_checked(build: &mut IrBuilder<'_>, reg: IrOp, kind: u8, exit: IrOp) -> IrOp {
    let bits = load_wide(build, reg, exit);
    let sixty = build.const_int64(60);
    let fifteen = build.const_int64(15);
    let actual = build.inst(IrCmd::BITRSHIFT_INT64, &[bits, sixty]);
    let actual = build.inst(IrCmd::BITAND_INT64, &[actual, fifteen]);
    let expected = build.const_int64(i64::from(kind));
    let equal = build.cond(IrCondition::Equal);
    build.inst(IrCmd::CHECK_CMP_INT64, &[actual, expected, equal, exit]);
    bits
}

/// Guards that `reg` holds a value on the carrier (a wide kind: the type tag alone) and returns
/// its 64 bits.
pub fn load_wide(build: &mut IrBuilder<'_>, reg: IrOp, exit: IrOp) -> IrOp {
    build.load_and_check_tag(reg, TYPE_TAG, exit);
    build.inst(LOAD, &[reg])
}

/// Writes `bits` to `result` as a packed scalar under light tag `tag`: the value, the tag in
/// `extra`, then the type tag.
pub fn store(build: &mut IrBuilder<'_>, result: IrOp, bits: IrOp, tag: LightTag) {
    #[cfg(l3i_carrier = "light")]
    {
        build.inst(IrCmd::STORE_POINTER, &[result, bits]);
        let extra = build.const_int(std::ffi::c_int::from(tag));
        build.inst(IrCmd::STORE_EXTRA, &[result, extra]);
    }
    #[cfg(l3i_carrier = "integer")]
    {
        let _ = tag;
        build.inst(IrCmd::STORE_INT64, &[result, bits]);
    }
    let type_tag = build.const_tag(TYPE_TAG);
    build.inst(IrCmd::STORE_TAG, &[result, type_tag]);
}
