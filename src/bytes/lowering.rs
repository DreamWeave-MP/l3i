//! Native lowering of the [`Math`](super::Math) receiver's integer methods (feature `jit`).
//!
//! A lowered call is a tag check on the receiver and the arguments, Luau's own buffer bounds
//! check, one load or store, and a byte swap: the same instructions `buffer.readu32` compiles
//! to, plus the swap. The float forms (`readf32be`, `readf64be`, `readf16`, and their writes)
//! stay on the binder path, since the IR has no bit cast between integers and floats; a script
//! that needs them in native code reads the integer form and moves it through a scratch buffer
//! with `buffer.writeu32` and `buffer.readf32`, both of which Luau lowers.

use std::sync::atomic::{AtomicUsize, Ordering};

use super::Math;
use crate::native_code::hooks::{NamecallSite, NativeCodeHooks, NativeContext};
use crate::native_code::ir::{IrBuilder, IrCmd, IrOp, bytecode_type};
use crate::raw::ffi::{LUA_TBUFFER, LUA_TINTEGER, LUA_TNUMBER};

static LOWERED: AtomicUsize = AtomicUsize::new(0);

/// How many call sites the hook has lowered in this process (a diagnostic for tests).
#[doc(hidden)]
pub fn lowered_sites() -> usize {
    LOWERED.load(Ordering::Relaxed)
}

/// The hook set; [`super::BytesExtension`] registers it.
pub struct ByteMath;

/// What a member does: its byte width, the load or store, and how the bits become a value.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Op {
    /// Read 16 bits big-endian; signed or not.
    Read16 {
        signed: bool,
    },
    /// Read 24 bits; big-endian or little; signed or not.
    Read24 {
        big: bool,
        signed: bool,
    },
    /// Read 32 bits big-endian; signed or not.
    Read32 {
        signed: bool,
    },
    /// Read a 64-bit big-endian integer.
    Read64,
    Write16,
    Write24 {
        big: bool,
    },
    Write32,
    Write64,
}

impl Op {
    fn of(member: &str) -> Option<Op> {
        Some(match member {
            "readu16be" => Op::Read16 { signed: false },
            "readi16be" => Op::Read16 { signed: true },
            "readu24" => Op::Read24 { big: false, signed: false },
            "readi24" => Op::Read24 { big: false, signed: true },
            "readu24be" => Op::Read24 { big: true, signed: false },
            "readi24be" => Op::Read24 { big: true, signed: true },
            "readu32be" => Op::Read32 { signed: false },
            "readi32be" => Op::Read32 { signed: true },
            "readi64be" => Op::Read64,
            "writeu16be" | "writei16be" => Op::Write16,
            "writeu24" | "writei24" => Op::Write24 { big: false },
            "writeu24be" | "writei24be" => Op::Write24 { big: true },
            "writeu32be" | "writei32be" => Op::Write32,
            "writei64be" => Op::Write64,
            _ => return None,
        })
    }

    fn width(self) -> i32 {
        match self {
            Op::Read16 { .. } | Op::Write16 => 2,
            Op::Read24 { .. } | Op::Write24 { .. } => 3,
            Op::Read32 { .. } | Op::Write32 => 4,
            Op::Read64 | Op::Write64 => 8,
        }
    }

    fn is_read(self) -> bool {
        matches!(self, Op::Read16 { .. } | Op::Read24 { .. } | Op::Read32 { .. } | Op::Read64)
    }

    fn result_type(self) -> u8 {
        match self {
            Op::Read64 => bytecode_type::INTEGER,
            op if op.is_read() => bytecode_type::NUMBER,
            _ => bytecode_type::ANY,
        }
    }
}

/// `(value ^ sign) - sign`: sign-extends an unsigned `bits`-wide int. Luau's uint commands
/// take their constants as ints, as its own bit32 lowering does.
fn sign_extend(build: &mut IrBuilder<'_>, value: IrOp, bits: u32) -> IrOp {
    let sign = build.const_int(1 << (bits - 1));
    let flipped = build.inst(IrCmd::BITXOR_UINT, &[value, sign]);
    build.inst(IrCmd::SUB_INT, &[flipped, sign])
}

/// A 16-bit value's bytes swapped: swap the 32-bit form and take its top half.
fn swap16(build: &mut IrBuilder<'_>, value: IrOp) -> IrOp {
    let swapped = build.inst(IrCmd::BYTESWAP_UINT, &[value]);
    let sixteen = build.const_int(16);
    build.inst(IrCmd::BITRSHIFT_UINT, &[swapped, sixteen])
}

fn store_number(build: &mut IrBuilder<'_>, result: IrOp, value: IrOp) {
    build.inst(IrCmd::STORE_DOUBLE, &[result, value]);
    let tag = build.const_tag(LUA_TNUMBER as u8);
    build.inst(IrCmd::STORE_TAG, &[result, tag]);
}

impl NativeCodeHooks for ByteMath {
    fn userdata_namecall_type(&self, context: &NativeContext<'_>, userdata_type: u8, member: &str) -> u8 {
        if context.userdata_type_of::<Math>() != Some(userdata_type) {
            return bytecode_type::ANY;
        }
        Op::of(member).map_or(bytecode_type::ANY, Op::result_type)
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
        let Some(op) = Op::of(member) else { return false };
        let read = op.is_read();
        if (read && (site.params != 3 || site.results != 1)) || (!read && (site.params != 4 || site.results != 0)) {
            return false;
        }
        let Some(tag) = context.tag_of::<Math>() else { return false };
        let exit = build.vm_exit(site.pcpos);
        let receiver = build.vm_reg(site.source_reg);
        let pointer = build.inst(IrCmd::LOAD_POINTER, &[receiver]);
        let tag = build.const_int(i32::from(tag));
        build.inst(IrCmd::CHECK_USERDATA_TAG, &[pointer, tag, exit]);

        let buffer_reg = build.vm_reg(site.arg_res_reg + 2);
        let offset_reg = build.vm_reg(site.arg_res_reg + 3);
        build.load_and_check_tag(buffer_reg, LUA_TBUFFER as u8, exit);
        build.load_and_check_tag(offset_reg, LUA_TNUMBER as u8, exit);
        let buffer = build.inst(IrCmd::LOAD_POINTER, &[buffer_reg]);
        let offset_number = build.inst(IrCmd::LOAD_DOUBLE, &[offset_reg]);
        let offset = build.inst(IrCmd::NUM_TO_INT, &[offset_number]);
        let zero = build.const_int(0);
        let width = build.const_int(op.width());
        build.inst(IrCmd::CHECK_BUFFER_LEN, &[buffer, offset, zero, width, offset_number, exit]);
        let buffer_tag = build.const_tag(LUA_TBUFFER as u8);
        let result = build.vm_reg(site.arg_res_reg);

        match op {
            Op::Read16 { signed } => {
                let raw = build.inst(IrCmd::BUFFER_READU16, &[buffer, offset, buffer_tag]);
                let value = swap16(build, raw);
                let value = if signed {
                    let value = sign_extend(build, value, 16);
                    build.inst(IrCmd::INT_TO_NUM, &[value])
                } else {
                    build.inst(IrCmd::UINT_TO_NUM, &[value])
                };
                store_number(build, result, value);
            }
            Op::Read24 { big, signed } => {
                let two = build.const_int(2);
                let third = build.inst(IrCmd::ADD_INT, &[offset, two]);
                let value = if big {
                    // The first two bytes are the high 16 bits, the third the low 8.
                    let high = build.inst(IrCmd::BUFFER_READU16, &[buffer, offset, buffer_tag]);
                    let high = swap16(build, high);
                    let eight = build.const_int(8);
                    let high = build.inst(IrCmd::BITLSHIFT_UINT, &[high, eight]);
                    let low = build.inst(IrCmd::BUFFER_READU8, &[buffer, third, buffer_tag]);
                    build.inst(IrCmd::BITOR_UINT, &[high, low])
                } else {
                    let low = build.inst(IrCmd::BUFFER_READU16, &[buffer, offset, buffer_tag]);
                    let high = build.inst(IrCmd::BUFFER_READU8, &[buffer, third, buffer_tag]);
                    let sixteen = build.const_int(16);
                    let high = build.inst(IrCmd::BITLSHIFT_UINT, &[high, sixteen]);
                    build.inst(IrCmd::BITOR_UINT, &[low, high])
                };
                let value = if signed {
                    let value = sign_extend(build, value, 24);
                    build.inst(IrCmd::INT_TO_NUM, &[value])
                } else {
                    build.inst(IrCmd::UINT_TO_NUM, &[value])
                };
                store_number(build, result, value);
            }
            Op::Read32 { signed } => {
                let raw = build.inst(IrCmd::BUFFER_READI32, &[buffer, offset, buffer_tag]);
                let value = build.inst(IrCmd::BYTESWAP_UINT, &[raw]);
                let value = if signed {
                    build.inst(IrCmd::INT_TO_NUM, &[value])
                } else {
                    build.inst(IrCmd::UINT_TO_NUM, &[value])
                };
                store_number(build, result, value);
            }
            Op::Read64 => {
                let raw = build.inst(IrCmd::BUFFER_READI64, &[buffer, offset, buffer_tag]);
                let value = build.inst(IrCmd::BYTESWAP_INT64, &[raw]);
                build.inst(IrCmd::STORE_INT64, &[result, value]);
                let tag = build.const_tag(LUA_TINTEGER as u8);
                build.inst(IrCmd::STORE_TAG, &[result, tag]);
            }
            Op::Write16 | Op::Write24 { .. } | Op::Write32 => {
                let value_reg = build.vm_reg(site.arg_res_reg + 4);
                build.load_and_check_tag(value_reg, LUA_TNUMBER as u8, exit);
                let number = build.inst(IrCmd::LOAD_DOUBLE, &[value_reg]);
                // `(long long)double`, then the low 32 bits: what the binder's `as i64 as u32`
                // computes, and what Luau's own unsigned buffer writes do.
                let value = build.inst(IrCmd::NUM_TO_UINT, &[number]);
                match op {
                    Op::Write16 => {
                        let swapped = swap16(build, value);
                        build.inst(IrCmd::BUFFER_WRITEI16, &[buffer, offset, swapped, buffer_tag]);
                    }
                    Op::Write24 { big: true } => {
                        for (index, shift) in [(0, 16), (1, 8), (2, 0)] {
                            let at = build.const_int(index);
                            let at = build.inst(IrCmd::ADD_INT, &[offset, at]);
                            let byte = if shift == 0 {
                                value
                            } else {
                                let shift = build.const_int(shift);
                                build.inst(IrCmd::BITRSHIFT_UINT, &[value, shift])
                            };
                            build.inst(IrCmd::BUFFER_WRITEI8, &[buffer, at, byte, buffer_tag]);
                        }
                    }
                    Op::Write24 { big: false } => {
                        build.inst(IrCmd::BUFFER_WRITEI16, &[buffer, offset, value, buffer_tag]);
                        let two = build.const_int(2);
                        let third = build.inst(IrCmd::ADD_INT, &[offset, two]);
                        let sixteen = build.const_int(16);
                        let high = build.inst(IrCmd::BITRSHIFT_UINT, &[value, sixteen]);
                        build.inst(IrCmd::BUFFER_WRITEI8, &[buffer, third, high, buffer_tag]);
                    }
                    _ => {
                        let swapped = build.inst(IrCmd::BYTESWAP_UINT, &[value]);
                        build.inst(IrCmd::BUFFER_WRITEI32, &[buffer, offset, swapped, buffer_tag]);
                    }
                }
            }
            Op::Write64 => {
                let value_reg = build.vm_reg(site.arg_res_reg + 4);
                build.load_and_check_tag(value_reg, LUA_TINTEGER as u8, exit);
                let bits = build.inst(IrCmd::LOAD_INT64, &[value_reg]);
                let swapped = build.inst(IrCmd::BYTESWAP_INT64, &[bits]);
                build.inst(IrCmd::BUFFER_WRITEI64, &[buffer, offset, swapped, buffer_tag]);
            }
        }
        LOWERED.fetch_add(1, Ordering::Relaxed);
        true
    }
}
