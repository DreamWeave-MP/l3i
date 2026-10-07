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

use super::layout::{KIND, SCRATCH, STRIDE};
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

/// Elements per unrolled `sum` block.
const UNROLL: usize = 8;

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

/// The IR condition for a comparison and whether its branches are swapped: Luau's number
/// compare has no `Equal`, so equality is `NotEqual` with the targets exchanged, which keeps
/// the IEEE rule (NaN is never equal, always not equal).
fn condition(comparison: Comparison) -> (IrCondition, bool) {
    match comparison {
        Comparison::Eq => (IrCondition::NotEqual, true),
        Comparison::Ne => (IrCondition::NotEqual, false),
        Comparison::Lt => (IrCondition::Less, false),
        Comparison::Le => (IrCondition::LessEqual, false),
        Comparison::Gt => (IrCondition::Greater, false),
        Comparison::Ge => (IrCondition::GreaterEqual, false),
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
        self.element_at(kind, cursor)
    }

    /// The receiver's stride in bytes, a double in its payload.
    fn stride(&mut self) -> IrOp {
        let receiver = self.receiver();
        let at = self.b.const_int(STRIDE);
        let tag = self.b.const_tag(LUA_TUSERDATA as u8);
        self.b.inst(IrCmd::BUFFER_READF64, &[receiver, at, tag])
    }

    /// Element `k` of an unrolled pass (contiguous, `size` bytes apart) whose cursor has
    /// already advanced by `UNROLL` elements.
    fn unrolled_element(&mut self, kind: Kind, k: usize, size: i64) -> IrOp {
        let cursor = self.get_num(S_CURSOR);
        let base = self.b.inst(IrCmd::NUM_TO_INT, &[cursor]);
        let back = self.b.const_int(-((UNROLL - k) as i64 * size) as i32);
        let offset = self.b.inst(IrCmd::ADD_INT, &[base, back]);
        self.element_at(kind, offset)
    }

    /// The element at an int byte offset as a double.
    fn element_at(&mut self, kind: Kind, cursor: IrOp) -> IrOp {
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
            let size = kind.size() as i64;
            let contiguous = emit.block();
            let strided = emit.block();

            // The span must fit: offset + count * stride <= length, in int64 so nothing wraps.
            // Stricter than the binder's last-element rule for a stride above the element
            // size; a span the binder would accept but this rejects takes the slow path.
            emit.b.begin_block(entry);
            let offset64 = emit.int64_of(offset_reg);
            let count64 = emit.int64_of(count_reg);
            let stride = emit.stride();
            let stride64 = emit.b.inst(IrCmd::NUM_TO_INT64, &[stride]);
            let bytes = emit.b.inst(IrCmd::MUL_INT64, &[count64, stride64]);
            let end64 = emit.b.inst(IrCmd::ADD_INT64, &[offset64, bytes]);
            let length = emit.buffer_length();
            let fits = emit.b.cond(IrCondition::UnsignedLessEqual);
            emit.b.inst(IrCmd::CHECK_CMP_INT64, &[end64, length, fits, slow]);
            let end_number = emit.b.inst(IrCmd::INT64_TO_NUM, &[end64]);
            emit.set_num(S_END, end_number);
            // A contiguous span (the common case, and the only one the JSL bridge emits) gets
            // the loop with compile-time steps and unrolling; a strided span the simple loop
            // whose step is the payload's stride. Both are measured in DATA_PLANE.md.
            let stride = emit.stride();
            let stride64 = emit.b.inst(IrCmd::NUM_TO_INT64, &[stride]);
            let element = emit.b.const_int64(size);
            let same = emit.b.cond(IrCondition::Equal);
            emit.b.inst(IrCmd::JUMP_CMP_INT64, &[stride64, element, same, contiguous, strided]);

            for (variant, step) in [(contiguous, Some(size)), (strided, None)] {
                emit.b.begin_block(variant);
                emit.reduction_loop(member, *kind, step, count_reg, site.arg_res_reg, done);
            }
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

impl Emit<'_, '_> {
    /// The byte step between elements: a compile-time constant for a contiguous span, else the
    /// receiver's stride.
    fn step(&mut self, step: Option<i64>) -> IrOp {
        match step {
            Some(size) => self.b.const_double(size as f64),
            None => self.stride(),
        }
    }

    /// The loop for one member over one kind, from the block already begun: the seed for
    /// extrema, the unrolled pass (contiguous spans only), the single-step tail, the result.
    /// The cursor and end are set; every block reloads what it uses.
    #[allow(clippy::too_many_lines)]
    fn reduction_loop(
        &mut self,
        member: Member,
        kind: Kind,
        step: Option<i64>,
        count_reg: c_int,
        result_reg: c_int,
        done: IrOp,
    ) {
        let head = self.block();
        let body = self.block();
        let finish = self.block();
        match member {
            Member::Min | Member::Max => {
                // Seed with the first element, or answer nil for an empty span.
                let seed = self.block();
                let empty = self.block();
                let count64 = self.int64_of(count_reg);
                let count_zero = self.b.const_int64(0);
                let is_empty = self.b.cond(IrCondition::Equal);
                self.b.inst(IrCmd::JUMP_CMP_INT64, &[count64, count_zero, is_empty, empty, seed]);
                self.b.begin_block(empty);
                let result = self.b.vm_reg(result_reg);
                let nil = self.b.const_tag(LUA_TNIL as u8);
                self.b.inst(IrCmd::STORE_TAG, &[result, nil]);
                self.jump(done);
                self.b.begin_block(seed);
                let cursor = self.get_num(S_CURSOR);
                let first = self.element(kind, cursor);
                self.set_num(S_ACC, first);
                let cursor = self.get_num(S_CURSOR);
                let step = self.step(step);
                let next = self.b.inst(IrCmd::ADD_NUM, &[cursor, step]);
                self.set_num(S_CURSOR, next);
                self.jump(head);
            }
            Member::Sum | Member::Count(_) => self.jump(head),
        }

        self.b.begin_block(head);
        if let Some(size) = step {
            // UNROLL elements per pass with one cursor round trip through scratch (the
            // single-step loop pays one per element, and that latency bounds it). Pure adds
            // fold in one block; comparisons branch, which ends an IR block, so they chain one
            // block per element with the cursor already advanced and the accumulator touched
            // only on a hit or a replacement.
            let unrolled = self.block();
            let single = self.block();
            let span_bytes = (UNROLL as i64 * size) as f64;
            let cursor = self.get_num(S_CURSOR);
            let span = self.b.const_double(span_bytes);
            let reach = self.b.inst(IrCmd::ADD_NUM, &[cursor, span]);
            let end = self.get_num(S_END);
            let fits = self.b.cond(IrCondition::LessEqual);
            self.b.inst(IrCmd::JUMP_CMP_NUM, &[reach, end, fits, unrolled, single]);

            self.b.begin_block(unrolled);
            let cursor = self.get_num(S_CURSOR);
            let span = self.b.const_double(span_bytes);
            let next = self.b.inst(IrCmd::ADD_NUM, &[cursor, span]);
            self.set_num(S_CURSOR, next);
            match member {
                Member::Sum => {
                    let base = self.b.inst(IrCmd::NUM_TO_INT, &[cursor]);
                    let mut acc = self.get_num(S_ACC);
                    for k in 0..UNROLL {
                        let at = self.b.const_int((k as i64 * size) as i32);
                        let offset = self.b.inst(IrCmd::ADD_INT, &[base, at]);
                        let value = self.element_at(kind, offset);
                        acc = self.b.inst(IrCmd::ADD_NUM, &[acc, value]);
                    }
                    self.set_num(S_ACC, acc);
                    self.jump(head);
                }
                Member::Count(_) | Member::Min | Member::Max => {
                    // Element k sits (UNROLL - k) elements before the advanced cursor.
                    let chain: Vec<IrOp> = (0..UNROLL).map(|_| self.block()).collect();
                    self.jump(chain[0]);
                    for k in 0..UNROLL {
                        let next_block = if k + 1 < UNROLL { chain[k + 1] } else { head };
                        let hit = self.block();
                        self.b.begin_block(chain[k]);
                        let value = self.unrolled_element(kind, k, size);
                        match member {
                            Member::Count(comparison) => {
                                let threshold = self.get_num(S_THRESHOLD);
                                let (test, swapped) = condition(comparison);
                                let holds = self.b.cond(test);
                                let (yes, no) = if swapped { (next_block, hit) } else { (hit, next_block) };
                                self.b.inst(IrCmd::JUMP_CMP_NUM, &[value, threshold, holds, yes, no]);
                                self.b.begin_block(hit);
                                let acc = self.get_num(S_ACC);
                                let one = self.b.const_double(1.0);
                                let counted = self.b.inst(IrCmd::ADD_NUM, &[acc, one]);
                                self.set_num(S_ACC, counted);
                            }
                            _ => {
                                let acc = self.get_num(S_ACC);
                                let better = self.b.cond(if member == Member::Min {
                                    IrCondition::Less
                                } else {
                                    IrCondition::Greater
                                });
                                self.b.inst(IrCmd::JUMP_CMP_NUM, &[value, acc, better, hit, next_block]);
                                self.b.begin_block(hit);
                                let value = self.unrolled_element(kind, k, size);
                                self.set_num(S_ACC, value);
                            }
                        }
                        self.jump(next_block);
                    }
                }
            }

            self.b.begin_block(single);
        }
        let cursor = self.get_num(S_CURSOR);
        let end = self.get_num(S_END);
        let more = self.b.cond(IrCondition::Less);
        self.b.inst(IrCmd::JUMP_CMP_NUM, &[cursor, end, more, body, finish]);

        self.b.begin_block(body);
        let cursor = self.get_num(S_CURSOR);
        let value = self.element(kind, cursor);
        let step_value = self.step(step);
        let next = self.b.inst(IrCmd::ADD_NUM, &[cursor, step_value]);
        self.set_num(S_CURSOR, next);
        match member {
            Member::Sum => {
                let acc = self.get_num(S_ACC);
                let total = self.b.inst(IrCmd::ADD_NUM, &[acc, value]);
                self.set_num(S_ACC, total);
                self.jump(head);
            }
            Member::Min | Member::Max => {
                let replace = self.block();
                let acc = self.get_num(S_ACC);
                let better = self.b.cond(if member == Member::Min { IrCondition::Less } else { IrCondition::Greater });
                self.b.inst(IrCmd::JUMP_CMP_NUM, &[value, acc, better, replace, head]);
                self.b.begin_block(replace);
                let cursor = self.get_num(S_CURSOR);
                let step_value = self.step(step);
                let previous = self.b.inst(IrCmd::SUB_NUM, &[cursor, step_value]);
                let value = self.element(kind, previous);
                self.set_num(S_ACC, value);
                self.jump(head);
            }
            Member::Count(comparison) => {
                let hit = self.block();
                let threshold = self.get_num(S_THRESHOLD);
                let (test, swapped) = condition(comparison);
                let holds = self.b.cond(test);
                let (yes, no) = if swapped { (head, hit) } else { (hit, head) };
                self.b.inst(IrCmd::JUMP_CMP_NUM, &[value, threshold, holds, yes, no]);
                self.b.begin_block(hit);
                let acc = self.get_num(S_ACC);
                let one = self.b.const_double(1.0);
                let counted = self.b.inst(IrCmd::ADD_NUM, &[acc, one]);
                self.set_num(S_ACC, counted);
                self.jump(head);
            }
        }

        self.b.begin_block(finish);
        let acc = self.get_num(S_ACC);
        let result = self.b.vm_reg(result_reg);
        store_number(self.b, result, acc);
        self.jump(done);
    }
}
