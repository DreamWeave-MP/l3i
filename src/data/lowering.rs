//! Native lowering of the [`KindReceiver`]'s reductions (feature `jit`).
//!
//! A call `K:sum(buffer, offset, count)` on a receiver the script annotated as
//! `dream_data_Kind` becomes a loop in Luau's IR: the receiver tag, the buffer tag, the whole
//! non-negative bounds and the span's fit are checked up front; anything unexpected jumps to the
//! binder, which raises the bound method's own error, so native and bound paths cannot
//! disagree about what fails. The loop keeps its byte cursor, end and accumulator in the
//! receiver's scratch words (Luau's IR has no loop-carried registers), reads each element with
//! the same `BUFFER_READ*` command `buffer.read<kind>` lowers to, and accumulates in double in
//! element order: the number the bound loop returns, bit for bit.
//!
//! `min`/`max` seed the accumulator with the first element and only replace it when `<`/`>`
//! holds, so a leading NaN stays and later NaNs never replace, as the loop and the bound path
//! do. The `count*` methods branch on `JUMP_CMP_NUM` with the IEEE condition, so every NaN
//! comparison is false except `countNe`.

use std::ffi::c_int;
use std::sync::atomic::{AtomicUsize, Ordering};

use super::layout::{KIND, SCRATCH};
use super::{Comparison, Kind, KindReceiver};
use crate::native_code::hooks::{NamecallSite, NativeCodeHooks, NativeContext};
use crate::native_code::ir::{IrBlockKind, IrBuilder, IrCmd, IrCondition, IrOp, bytecode_type};
use crate::raw::ffi::{LUA_TBUFFER, LUA_TNIL, LUA_TNUMBER, LUA_TUSERDATA};

static LOWERED: AtomicUsize = AtomicUsize::new(0);

/// How many call sites the hook has lowered in this process (a diagnostic for tests).
#[doc(hidden)]
pub fn lowered_sites() -> usize {
    LOWERED.load(Ordering::Relaxed)
}

/// The hook set; [`super::DataExtension`] registers it.
pub struct KindLowering;

/// The scratch words, by use, all doubles: the byte cursor and its end (whole numbers, since
/// Luau's integer compare wants a constant operand), the accumulator and the threshold.
const S_CURSOR: i32 = 0;
const S_END: i32 = 1;
const S_ACC: i32 = 2;
const S_THRESHOLD: i32 = 3;

/// `offsetof(Buffer, data)`; the length is the high half of the word at 0.
const BUFFER_LENGTH_WORD: i64 = 0;
/// `offsetof(Udata, data)`: what a `BUFFER_READ*` under the userdata tag adds.
const UDATA_DATA: i64 = 16;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Member {
    Sum,
    Min,
    Max,
    Count(Comparison),
}

impl Member {
    fn of(name: &str) -> Option<Member> {
        Some(match name {
            "sum" => Member::Sum,
            "min" => Member::Min,
            "max" => Member::Max,
            "countEq" => Member::Count(Comparison::Eq),
            "countNe" => Member::Count(Comparison::Ne),
            "countLt" => Member::Count(Comparison::Lt),
            "countLe" => Member::Count(Comparison::Le),
            "countGt" => Member::Count(Comparison::Gt),
            "countGe" => Member::Count(Comparison::Ge),
            _ => return None,
        })
    }

    /// Arguments including the receiver.
    fn params(self) -> c_int {
        match self {
            Member::Count(_) => 5,
            Member::Sum | Member::Min | Member::Max => 4,
        }
    }
}

fn condition(comparison: Comparison) -> IrCondition {
    match comparison {
        Comparison::Eq => IrCondition::Equal,
        Comparison::Ne => IrCondition::NotEqual,
        Comparison::Lt => IrCondition::Less,
        Comparison::Le => IrCondition::LessEqual,
        Comparison::Gt => IrCondition::Greater,
        Comparison::Ge => IrCondition::GreaterEqual,
    }
}

/// The IR of one lowered site.
struct Emit<'b, 'a> {
    b: &'b mut IrBuilder<'a>,
    receiver: c_int,
    buffer: c_int,
}

impl Emit<'_, '_> {
    fn receiver(&mut self) -> IrOp {
        let receiver = self.b.vm_reg(self.receiver);
        self.b.inst(IrCmd::LOAD_POINTER, &[receiver])
    }

    fn buffer(&mut self) -> IrOp {
        let buffer = self.b.vm_reg(self.buffer);
        self.b.inst(IrCmd::LOAD_POINTER, &[buffer])
    }

    fn get_num(&mut self, word: i32) -> IrOp {
        let receiver = self.receiver();
        let offset = self.b.const_int(SCRATCH + word * 8);
        let tag = self.b.const_tag(LUA_TUSERDATA as u8);
        self.b.inst(IrCmd::BUFFER_READF64, &[receiver, offset, tag])
    }

    fn set_num(&mut self, word: i32, value: IrOp) {
        let receiver = self.receiver();
        let offset = self.b.const_int(SCRATCH + word * 8);
        let tag = self.b.const_tag(LUA_TUSERDATA as u8);
        self.b.inst(IrCmd::BUFFER_WRITEF64, &[receiver, offset, value, tag]);
    }

    /// The register's number as a whole non-negative int64, else `otherwise`.
    fn whole(&mut self, register: c_int, otherwise: IrOp) -> (IrOp, IrOp) {
        let register = self.b.vm_reg(register);
        self.b.load_and_check_tag(register, LUA_TNUMBER as u8, otherwise);
        let number = self.b.inst(IrCmd::LOAD_DOUBLE, &[register]);
        let value = self.b.inst(IrCmd::NUM_TO_INT64, &[number]);
        let back = self.b.inst(IrCmd::INT64_TO_NUM, &[value]);
        let equal = self.b.cond(IrCondition::Equal);
        self.b.inst(IrCmd::CHECK_CMP_NUM, &[back, number, equal, otherwise]);
        let zero = self.b.const_int64(0);
        let at_least = self.b.cond(IrCondition::GreaterEqual);
        self.b.inst(IrCmd::CHECK_CMP_INT64, &[value, zero, at_least, otherwise]);
        (value, number)
    }

    /// The element at the byte cursor (a whole double) as a double.
    fn element(&mut self, kind: Kind, cursor: IrOp) -> IrOp {
        let cursor = self.b.inst(IrCmd::NUM_TO_INT, &[cursor]);
        let buffer = self.buffer();
        let tag = self.b.const_tag(LUA_TBUFFER as u8);
        match kind {
            Kind::U8 => {
                let raw = self.b.inst(IrCmd::BUFFER_READU8, &[buffer, cursor, tag]);
                self.b.inst(IrCmd::INT_TO_NUM, &[raw])
            }
            Kind::I8 => {
                let raw = self.b.inst(IrCmd::BUFFER_READI8, &[buffer, cursor, tag]);
                self.b.inst(IrCmd::INT_TO_NUM, &[raw])
            }
            Kind::U16 => {
                let raw = self.b.inst(IrCmd::BUFFER_READU16, &[buffer, cursor, tag]);
                self.b.inst(IrCmd::INT_TO_NUM, &[raw])
            }
            Kind::I16 => {
                let raw = self.b.inst(IrCmd::BUFFER_READI16, &[buffer, cursor, tag]);
                self.b.inst(IrCmd::INT_TO_NUM, &[raw])
            }
            Kind::U32 => {
                let raw = self.b.inst(IrCmd::BUFFER_READI32, &[buffer, cursor, tag]);
                self.b.inst(IrCmd::UINT_TO_NUM, &[raw])
            }
            Kind::I32 => {
                let raw = self.b.inst(IrCmd::BUFFER_READI32, &[buffer, cursor, tag]);
                self.b.inst(IrCmd::INT_TO_NUM, &[raw])
            }
            Kind::F32 => {
                let raw = self.b.inst(IrCmd::BUFFER_READF32, &[buffer, cursor, tag]);
                self.b.inst(IrCmd::FLOAT_TO_NUM, &[raw])
            }
            Kind::F64 => self.b.inst(IrCmd::BUFFER_READF64, &[buffer, cursor, tag]),
        }
    }

    fn jump(&mut self, block: IrOp) {
        self.b.inst(IrCmd::JUMP, &[block]);
    }

    fn block(&mut self) -> IrOp {
        self.b.block(IrBlockKind::Internal)
    }
}

fn store_number(build: &mut IrBuilder<'_>, register: IrOp, value: IrOp) {
    build.inst(IrCmd::STORE_DOUBLE, &[register, value]);
    let tag = build.const_tag(LUA_TNUMBER as u8);
    build.inst(IrCmd::STORE_TAG, &[register, tag]);
}

impl NativeCodeHooks for KindLowering {
    fn userdata_namecall_type(&self, context: &NativeContext<'_>, userdata_type: u8, member: &str) -> u8 {
        if context.userdata_type_of::<KindReceiver>() != Some(userdata_type) {
            return bytecode_type::ANY;
        }
        match Member::of(member) {
            Some(Member::Sum | Member::Count(_)) => bytecode_type::NUMBER,
            Some(Member::Min | Member::Max) | None => bytecode_type::ANY,
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
        if context.userdata_type_of::<KindReceiver>() != Some(userdata_type) {
            return false;
        }
        let Some(member) = Member::of(member) else { return false };
        if site.params != member.params() || site.results != 1 {
            return false;
        }
        let Some(tag) = context.tag_of::<KindReceiver>() else { return false };

        // The element kind is a property of the receiver *value*, unknown at compile time:
        // one loop per kind is emitted behind a dispatch on the payload's kind word. Every
        // block reloads what it uses from registers or scratch; IR values do not cross blocks.
        let slow = build.fallback_block(site.pcpos as u32);
        let done = build.block(IrBlockKind::Internal);
        let dispatch = build.block(IrBlockKind::Internal);
        let kinds = [Kind::U8, Kind::I8, Kind::U16, Kind::I16, Kind::U32, Kind::I32, Kind::F32, Kind::F64];
        let loops: Vec<IrOp> = kinds.iter().map(|_| build.block(IrBlockKind::Internal)).collect();
        let offset_reg = site.arg_res_reg + 3;
        let count_reg = site.arg_res_reg + 4;

        let mut emit = Emit { b: build, receiver: site.source_reg, buffer: site.arg_res_reg + 2 };
        let receiver = emit.receiver();
        let tag = emit.b.const_int(i32::from(tag));
        emit.b.inst(IrCmd::CHECK_USERDATA_TAG, &[receiver, tag, slow]);
        let buffer_reg = emit.b.vm_reg(emit.buffer);
        emit.b.load_and_check_tag(buffer_reg, LUA_TBUFFER as u8, slow);
        let (_, offset_number) = emit.whole(offset_reg, slow);
        emit.whole(count_reg, slow);
        if let Member::Count(_) = member {
            let threshold_reg = emit.b.vm_reg(site.arg_res_reg + 5);
            emit.b.load_and_check_tag(threshold_reg, LUA_TNUMBER as u8, slow);
            let threshold = emit.b.inst(IrCmd::LOAD_DOUBLE, &[threshold_reg]);
            emit.set_num(S_THRESHOLD, threshold);
        }
        emit.set_num(S_CURSOR, offset_number);
        let zero_num = emit.b.const_double(0.0);
        emit.set_num(S_ACC, zero_num);
        emit.jump(dispatch);

        emit.dispatch(dispatch, &loops, slow);

        for (kind, &entry) in kinds.iter().zip(&loops) {
            let head = emit.block();
            let body = emit.block();
            let finish = emit.block();
            let size = kind.size() as i64;

            // The span must fit: offset + count * size <= length, in int64 so nothing wraps.
            emit.b.begin_block(entry);
            let offset64 = emit.int64_of(offset_reg);
            let count64 = emit.int64_of(count_reg);
            let size64 = emit.b.const_int64(size);
            let bytes = emit.b.inst(IrCmd::MUL_INT64, &[count64, size64]);
            let end64 = emit.b.inst(IrCmd::ADD_INT64, &[offset64, bytes]);
            let length = emit.buffer_length();
            let fits = emit.b.cond(IrCondition::UnsignedLessEqual);
            emit.b.inst(IrCmd::CHECK_CMP_INT64, &[end64, length, fits, slow]);
            let end_number = emit.b.inst(IrCmd::INT64_TO_NUM, &[end64]);
            emit.set_num(S_END, end_number);
            match member {
                Member::Min | Member::Max => {
                    // Seed with the first element, or answer nil for an empty span.
                    let seed = emit.block();
                    let empty = emit.block();
                    let count_zero = emit.b.const_int64(0);
                    let is_empty = emit.b.cond(IrCondition::Equal);
                    emit.b.inst(IrCmd::JUMP_CMP_INT64, &[count64, count_zero, is_empty, empty, seed]);
                    emit.b.begin_block(empty);
                    let result = emit.b.vm_reg(site.arg_res_reg);
                    let nil = emit.b.const_tag(LUA_TNIL as u8);
                    emit.b.inst(IrCmd::STORE_TAG, &[result, nil]);
                    emit.jump(done);
                    emit.b.begin_block(seed);
                    let cursor = emit.get_num(S_CURSOR);
                    let first = emit.element(*kind, cursor);
                    emit.set_num(S_ACC, first);
                    let cursor = emit.get_num(S_CURSOR);
                    let step = emit.b.const_double(size as f64);
                    let next = emit.b.inst(IrCmd::ADD_NUM, &[cursor, step]);
                    emit.set_num(S_CURSOR, next);
                    emit.jump(head);
                }
                Member::Sum | Member::Count(_) => emit.jump(head),
            }

            emit.b.begin_block(head);
            let cursor = emit.get_num(S_CURSOR);
            let end = emit.get_num(S_END);
            let more = emit.b.cond(IrCondition::Less);
            emit.b.inst(IrCmd::JUMP_CMP_NUM, &[cursor, end, more, body, finish]);

            emit.b.begin_block(body);
            let cursor = emit.get_num(S_CURSOR);
            let value = emit.element(*kind, cursor);
            let step = emit.b.const_double(size as f64);
            let next = emit.b.inst(IrCmd::ADD_NUM, &[cursor, step]);
            emit.set_num(S_CURSOR, next);
            match member {
                Member::Sum => {
                    let acc = emit.get_num(S_ACC);
                    let total = emit.b.inst(IrCmd::ADD_NUM, &[acc, value]);
                    emit.set_num(S_ACC, total);
                    emit.jump(head);
                }
                Member::Min | Member::Max => {
                    let replace = emit.block();
                    let acc = emit.get_num(S_ACC);
                    let better =
                        emit.b.cond(if member == Member::Min { IrCondition::Less } else { IrCondition::Greater });
                    emit.b.inst(IrCmd::JUMP_CMP_NUM, &[value, acc, better, replace, head]);
                    emit.b.begin_block(replace);
                    let cursor = emit.get_num(S_CURSOR);
                    let step = emit.b.const_double(size as f64);
                    let previous = emit.b.inst(IrCmd::SUB_NUM, &[cursor, step]);
                    let value = emit.element(*kind, previous);
                    emit.set_num(S_ACC, value);
                    emit.jump(head);
                }
                Member::Count(comparison) => {
                    let hit = emit.block();
                    let threshold = emit.get_num(S_THRESHOLD);
                    let holds = emit.b.cond(condition(comparison));
                    emit.b.inst(IrCmd::JUMP_CMP_NUM, &[value, threshold, holds, hit, head]);
                    emit.b.begin_block(hit);
                    let acc = emit.get_num(S_ACC);
                    let one = emit.b.const_double(1.0);
                    let counted = emit.b.inst(IrCmd::ADD_NUM, &[acc, one]);
                    emit.set_num(S_ACC, counted);
                    emit.jump(head);
                }
            }

            emit.b.begin_block(finish);
            let acc = emit.get_num(S_ACC);
            let result = emit.b.vm_reg(site.arg_res_reg);
            store_number(emit.b, result, acc);
            emit.jump(done);
        }

        emit.b.begin_block(slow);
        emit.b.namecall_call(site.pcpos);
        emit.jump(done);

        emit.b.begin_block(done);
        LOWERED.fetch_add(1, Ordering::Relaxed);
        true
    }
}

impl Emit<'_, '_> {
    /// A checked register's number as int64 (the checks ran in the preamble).
    fn int64_of(&mut self, register: c_int) -> IrOp {
        let register = self.b.vm_reg(register);
        let number = self.b.inst(IrCmd::LOAD_DOUBLE, &[register]);
        self.b.inst(IrCmd::NUM_TO_INT64, &[number])
    }

    /// The buffer's length: the high half of its first word.
    fn buffer_length(&mut self) -> IrOp {
        let buffer = self.buffer();
        let at = self.b.const_int64(BUFFER_LENGTH_WORD - UDATA_DATA);
        let base = self.b.inst(IrCmd::ADD_INT64, &[buffer, at]);
        let zero = self.b.const_int(0);
        let udata = self.b.const_tag(LUA_TUSERDATA as u8);
        let word = self.b.inst(IrCmd::BUFFER_READI64, &[base, zero, udata]);
        let thirty_two = self.b.const_int64(32);
        self.b.inst(IrCmd::BITRSHIFT_INT64, &[word, thirty_two])
    }

    /// The receiver's kind word.
    fn kind_word(&mut self) -> IrOp {
        let receiver = self.receiver();
        let at = self.b.const_int(KIND);
        let udata = self.b.const_tag(LUA_TUSERDATA as u8);
        self.b.inst(IrCmd::BUFFER_READI64, &[receiver, at, udata])
    }

    /// A chain of equality tests on the receiver's kind word into one loop per kind; a kind
    /// word outside the table goes to the binder.
    fn dispatch(&mut self, dispatch: IrOp, loops: &[IrOp], slow: IrOp) {
        let mut current = dispatch;
        for (index, &target) in loops.iter().enumerate() {
            self.b.begin_block(current);
            let next = if index + 1 == loops.len() { slow } else { self.block() };
            let kind = self.kind_word();
            let expected = self.b.const_int64(index as i64);
            let equal = self.b.cond(IrCondition::Equal);
            self.b.inst(IrCmd::JUMP_CMP_INT64, &[kind, expected, equal, target, next]);
            current = next;
        }
    }
}
