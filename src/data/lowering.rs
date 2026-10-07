//! Native lowering of the typed receivers' reductions (feature `jit`).
//!
//! A call `K:sum(buffer, offset, count)` or `K:countGt(buffer, offset, count, threshold)` on a
//! receiver the script annotated with its typed class (`dream_data_Kind_f32`) becomes a loop in
//! Luau's IR: the receiver
//! tag, the buffer tag, the whole non-negative bounds and the span's fit are checked up front;
//! anything unexpected jumps to the binder, which raises the bound method's own error, so
//! native and bound paths cannot disagree about what fails. The loop keeps its byte cursor,
//! end, accumulator, threshold and seen flag in the receiver's scratch words (Luau's IR has no
//! loop-carried values), reads each element with the same `BUFFER_READ*` command
//! `buffer.read<kind>` lowers to, and accumulates in double in element order: the number the
//! bound loop returns, bit for bit.
//!
//! Every member is one operation (`sum`, `min`, `max`, `count`, `any`, `all`) over the elements
//! that satisfy an optional comparison against the threshold. `min`/`max` keep the first
//! least/greatest by `<`/`>`, so a leading NaN stays and later NaNs never replace, as the bound
//! loop does. Comparisons branch on `JUMP_CMP_NUM` with the IEEE condition (equality is
//! `NotEqual` with its branches swapped, since Luau's number compare has no `Equal`), so every
//! NaN comparison is false except `Ne`. `any` and `all` leave the loop at the first deciding
//! element.
//!
//! A contiguous span (the common case, and the only one the JSL bridge emits) gets loops with
//! compile-time steps, unrolled eight elements per pass with one cursor round trip; a strided
//! span gets the simple loop whose step is the payload's stride.

use std::ffi::c_int;
use std::sync::atomic::{AtomicUsize, Ordering};

use super::layout::{SCRATCH, STRIDE};
use super::{Comparison, KINDS, Kind, Reduction, ReductionOp, Typed};
use crate::native_code::hooks::{NamecallSite, NativeCodeHooks, NativeContext};
use crate::native_code::ir::{IrBlockKind, IrBuilder, IrCmd, IrCondition, IrOp, bytecode_type};
use crate::raw::ffi::{LUA_TBOOLEAN, LUA_TBUFFER, LUA_TNIL, LUA_TNUMBER, LUA_TUSERDATA};

static LOWERED: AtomicUsize = AtomicUsize::new(0);

/// How many call sites the hook has lowered in this process (a diagnostic for tests).
#[doc(hidden)]
pub fn lowered_sites() -> usize {
    LOWERED.load(Ordering::Relaxed)
}

/// The hook set; [`super::DataExtension`] registers it.
pub struct KindLowering;

/// The scratch words, by use, all doubles: the byte cursor and its end (whole numbers, since
/// Luau's integer compare wants a constant operand), the accumulator, the threshold, and
/// whether a filtered min/max has seen an element.
const S_CURSOR: i32 = 0;
const S_END: i32 = 1;
const S_ACC: i32 = 2;
const S_THRESHOLD: i32 = 3;
const S_SEEN: i32 = 4;

/// Elements per unrolled pass over a contiguous span.
const UNROLL: usize = 8;

/// `offsetof(Buffer, data)`; the length is the high half of the word at 0.
const BUFFER_LENGTH_WORD: i64 = 0;
/// `offsetof(Udata, data)`: what a `BUFFER_READ*` under the userdata tag adds.
const UDATA_DATA: i64 = 16;

fn member_of(name: &str) -> Option<Reduction> {
    let (op, rest) = [
        ("sum", ReductionOp::Sum),
        ("min", ReductionOp::Min),
        ("max", ReductionOp::Max),
        ("count", ReductionOp::Count),
        ("any", ReductionOp::Any),
        ("all", ReductionOp::All),
    ]
    .into_iter()
    .find_map(|(prefix, op)| name.strip_prefix(prefix).map(|rest| (op, rest)))?;
    let filter = match rest {
        "" => None,
        suffix => Some(Comparison::ALL.into_iter().find(|comparison| comparison.suffix() == suffix)?),
    };
    match (op, filter) {
        (ReductionOp::Count | ReductionOp::Any | ReductionOp::All, None) => None,
        _ => Some(Reduction { op, filter }),
    }
}

/// Arguments including the receiver.
fn params(member: Reduction) -> c_int {
    if member.filter.is_some() { 5 } else { 4 }
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
    result: c_int,
    done: IrOp,
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

    fn set_const(&mut self, word: i32, value: f64) {
        let value = self.b.const_double(value);
        self.set_num(word, value);
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

    /// The receiver's stride in bytes, a double in its payload.
    fn stride(&mut self) -> IrOp {
        let receiver = self.receiver();
        let at = self.b.const_int(STRIDE);
        let tag = self.b.const_tag(LUA_TUSERDATA as u8);
        self.b.inst(IrCmd::BUFFER_READF64, &[receiver, at, tag])
    }

    /// The byte step between elements: a compile-time constant for a contiguous span, else the
    /// receiver's stride.
    fn step(&mut self, step: Option<i64>) -> IrOp {
        match step {
            Some(size) => self.b.const_double(size as f64),
            None => self.stride(),
        }
    }

    /// The element at the byte cursor (a whole double) as a double.
    fn element(&mut self, kind: Kind, cursor: IrOp) -> IrOp {
        let cursor = self.b.inst(IrCmd::NUM_TO_INT, &[cursor]);
        self.element_at(kind, cursor)
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

    /// Element `k` of an unrolled pass (contiguous, `size` bytes apart) whose cursor has
    /// already advanced by `UNROLL` elements.
    fn unrolled_element(&mut self, kind: Kind, k: usize, size: i64) -> IrOp {
        let cursor = self.get_num(S_CURSOR);
        let base = self.b.inst(IrCmd::NUM_TO_INT, &[cursor]);
        let back = self.b.const_int(-((UNROLL - k) as i64 * size) as i32);
        let offset = self.b.inst(IrCmd::ADD_INT, &[base, back]);
        self.element_at(kind, offset)
    }

    fn jump(&mut self, block: IrOp) {
        self.b.inst(IrCmd::JUMP, &[block]);
    }

    fn block(&mut self) -> IrOp {
        self.b.block(IrBlockKind::Internal)
    }

    /// Stores a number result and leaves.
    fn answer_number(&mut self, value: IrOp) {
        let result = self.b.vm_reg(self.result);
        self.b.inst(IrCmd::STORE_DOUBLE, &[result, value]);
        let tag = self.b.const_tag(LUA_TNUMBER as u8);
        self.b.inst(IrCmd::STORE_TAG, &[result, tag]);
        let done = self.done;
        self.jump(done);
    }

    fn answer_nil(&mut self) {
        let result = self.b.vm_reg(self.result);
        let nil = self.b.const_tag(LUA_TNIL as u8);
        self.b.inst(IrCmd::STORE_TAG, &[result, nil]);
        let done = self.done;
        self.jump(done);
    }

    fn answer_bool(&mut self, value: bool) {
        let result = self.b.vm_reg(self.result);
        let bit = self.b.const_int(i32::from(value));
        self.b.inst(IrCmd::STORE_INT, &[result, bit]);
        let tag = self.b.const_tag(LUA_TBOOLEAN as u8);
        self.b.inst(IrCmd::STORE_TAG, &[result, tag]);
        let done = self.done;
        self.jump(done);
    }

    /// Branches on `value <filter> threshold`: `hit` when it holds, `miss` otherwise.
    fn test(&mut self, comparison: Comparison, value: IrOp, hit: IrOp, miss: IrOp) {
        let threshold = self.get_num(S_THRESHOLD);
        let (test, swapped) = condition(comparison);
        let holds = self.b.cond(test);
        let (yes, no) = if swapped { (miss, hit) } else { (hit, miss) };
        self.b.inst(IrCmd::JUMP_CMP_NUM, &[value, threshold, holds, yes, no]);
    }

    /// What an accepted element does to the accumulator, then on to `next`. `value` must be
    /// recomputable in a fresh block, so it is passed as a closure.
    fn accept(&mut self, member: Reduction, next: IrOp, value: impl Fn(&mut Self) -> IrOp) {
        match member.op {
            ReductionOp::Sum => {
                let acc = self.get_num(S_ACC);
                let value = value(self);
                let total = self.b.inst(IrCmd::ADD_NUM, &[acc, value]);
                self.set_num(S_ACC, total);
                self.jump(next);
            }
            ReductionOp::Count => {
                let acc = self.get_num(S_ACC);
                let one = self.b.const_double(1.0);
                let counted = self.b.inst(IrCmd::ADD_NUM, &[acc, one]);
                self.set_num(S_ACC, counted);
                self.jump(next);
            }
            ReductionOp::Min | ReductionOp::Max => {
                let replace = self.block();
                if member.filter.is_some() {
                    // The first accepted element seeds, whatever it is (a NaN stays).
                    let compare = self.block();
                    let seen = self.get_num(S_SEEN);
                    let zero = self.b.const_double(0.0);
                    let unseen = self.b.cond(IrCondition::NotEqual);
                    self.b.inst(IrCmd::JUMP_CMP_NUM, &[seen, zero, unseen, compare, replace]);
                    self.b.begin_block(compare);
                }
                let acc = self.get_num(S_ACC);
                let value_now = value(self);
                let better =
                    self.b.cond(if member.op == ReductionOp::Min { IrCondition::Less } else { IrCondition::Greater });
                self.b.inst(IrCmd::JUMP_CMP_NUM, &[value_now, acc, better, replace, next]);
                self.b.begin_block(replace);
                let value_now = value(self);
                self.set_num(S_ACC, value_now);
                if member.filter.is_some() {
                    self.set_const(S_SEEN, 1.0);
                }
                self.jump(next);
            }
            ReductionOp::Any => self.answer_bool(true),
            ReductionOp::All => self.jump(next),
        }
    }

    /// What a rejected element does: nothing, except that `all` is decided.
    fn reject(&mut self, member: Reduction, next: IrOp) {
        if member.op == ReductionOp::All {
            self.answer_bool(false);
        } else {
            self.jump(next);
        }
    }

    /// One element: filtered through the comparison when there is one, then accepted.
    fn visit(&mut self, member: Reduction, next: IrOp, value: impl Fn(&mut Self) -> IrOp + Copy) {
        match member.filter {
            Some(comparison) => {
                let hit = self.block();
                let miss = self.block();
                let value_now = value(self);
                self.test(comparison, value_now, hit, miss);
                self.b.begin_block(hit);
                self.accept(member, next, value);
                self.b.begin_block(miss);
                self.reject(member, next);
            }
            None => self.accept(member, next, value),
        }
    }

    /// The loop for one member over one kind, from the block already begun: the seed for
    /// unfiltered extrema, the unrolled pass (contiguous spans only), the single-step tail,
    /// the result. The cursor and end are set; every block reloads what it uses.
    #[allow(clippy::too_many_lines)]
    fn reduction_loop(&mut self, member: Reduction, kind: Kind, step: Option<i64>, count_reg: c_int) {
        let head = self.block();
        let body = self.block();
        let finish = self.block();
        let extremum = matches!(member.op, ReductionOp::Min | ReductionOp::Max);
        if extremum && member.filter.is_none() {
            // Seed with the first element, or answer nil for an empty span.
            let seed = self.block();
            let empty = self.block();
            let count64 = self.int64_of(count_reg);
            let count_zero = self.b.const_int64(0);
            let is_empty = self.b.cond(IrCondition::Equal);
            self.b.inst(IrCmd::JUMP_CMP_INT64, &[count64, count_zero, is_empty, empty, seed]);
            self.b.begin_block(empty);
            self.answer_nil();
            self.b.begin_block(seed);
            let cursor = self.get_num(S_CURSOR);
            let first = self.element(kind, cursor);
            self.set_num(S_ACC, first);
            let cursor = self.get_num(S_CURSOR);
            let step = self.step(step);
            let next = self.b.inst(IrCmd::ADD_NUM, &[cursor, step]);
            self.set_num(S_CURSOR, next);
            self.jump(head);
        } else {
            self.jump(head);
        }

        self.b.begin_block(head);
        if let Some(size) = step {
            // UNROLL elements per pass with one cursor round trip through scratch (the
            // single-step loop pays one per element, and that latency bounds it). An unfiltered
            // sum folds in one block; everything else branches, which ends an IR block, so it
            // chains one block per element with the cursor already advanced and the accumulator
            // touched only when an element is accepted.
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
            if member.op == ReductionOp::Sum && member.filter.is_none() {
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
            } else {
                let chain: Vec<IrOp> = (0..UNROLL).map(|_| self.block()).collect();
                self.jump(chain[0]);
                for k in 0..UNROLL {
                    let next_block = if k + 1 < UNROLL { chain[k + 1] } else { head };
                    self.b.begin_block(chain[k]);
                    self.visit(member, next_block, move |emit| emit.unrolled_element(kind, k, size));
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
        let step_value = self.step(step);
        let next = self.b.inst(IrCmd::ADD_NUM, &[cursor, step_value]);
        self.set_num(S_CURSOR, next);
        // The element now sits one step before the advanced cursor.
        self.visit(member, head, move |emit| {
            let cursor = emit.get_num(S_CURSOR);
            let step_value = emit.step(step);
            let previous = emit.b.inst(IrCmd::SUB_NUM, &[cursor, step_value]);
            emit.element(kind, previous)
        });

        self.b.begin_block(finish);
        match member.op {
            ReductionOp::Sum | ReductionOp::Count => {
                let acc = self.get_num(S_ACC);
                self.answer_number(acc);
            }
            ReductionOp::Min | ReductionOp::Max => {
                if member.filter.is_some() {
                    let some = self.block();
                    let none = self.block();
                    let seen = self.get_num(S_SEEN);
                    let zero = self.b.const_double(0.0);
                    let unseen = self.b.cond(IrCondition::NotEqual);
                    self.b.inst(IrCmd::JUMP_CMP_NUM, &[seen, zero, unseen, some, none]);
                    self.b.begin_block(none);
                    self.answer_nil();
                    self.b.begin_block(some);
                }
                let acc = self.get_num(S_ACC);
                self.answer_number(acc);
            }
            ReductionOp::Any => self.answer_bool(false),
            ReductionOp::All => self.answer_bool(true),
        }
    }
}

/// Which typed receiver a compiler userdata type is, with the tag this VM gave it.
fn typed_receiver(context: &NativeContext<'_>, userdata_type: u8) -> Option<(Kind, crate::userdata::RuntimeTag)> {
    macro_rules! resolve {
        ($($index:literal),*) => {$(
            if context.userdata_type_of::<Typed<$index>>() == Some(userdata_type) {
                return context.tag_of::<Typed<$index>>().map(|tag| (KINDS[$index], tag));
            }
        )*};
    }
    resolve!(0, 1, 2, 3, 4, 5, 6, 7);
    None
}

impl NativeCodeHooks for KindLowering {
    fn userdata_namecall_type(&self, context: &NativeContext<'_>, userdata_type: u8, member: &str) -> u8 {
        if typed_receiver(context, userdata_type).is_none() {
            return bytecode_type::ANY;
        }
        match member_of(member).map(|member| member.op) {
            Some(ReductionOp::Sum | ReductionOp::Count) => bytecode_type::NUMBER,
            Some(ReductionOp::Any | ReductionOp::All) => bytecode_type::BOOLEAN,
            Some(ReductionOp::Min | ReductionOp::Max) | None => bytecode_type::ANY,
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
        let Some((kind, tag)) = typed_receiver(context, userdata_type) else { return false };
        let Some(member) = member_of(member) else { return false };
        if site.params != params(member) || site.results != 1 {
            return false;
        }

        // The element kind is the receiver's type, known here; the stride is the value's, so
        // one loop pair per site: contiguous (constant steps, unrolled) and strided (simple).
        // Every block reloads what it uses from registers or scratch; IR values do not cross
        // blocks.
        let slow = build.fallback_block(site.pcpos as u32);
        let done = build.block(IrBlockKind::Internal);
        let contiguous = build.block(IrBlockKind::Internal);
        let strided = build.block(IrBlockKind::Internal);
        let offset_reg = site.arg_res_reg + 3;
        let count_reg = site.arg_res_reg + 4;
        let size = kind.size() as i64;

        let mut emit =
            Emit { b: build, receiver: site.source_reg, buffer: site.arg_res_reg + 2, result: site.arg_res_reg, done };
        let receiver = emit.receiver();
        let tag = emit.b.const_int(i32::from(tag));
        emit.b.inst(IrCmd::CHECK_USERDATA_TAG, &[receiver, tag, slow]);
        let buffer_reg = emit.b.vm_reg(emit.buffer);
        emit.b.load_and_check_tag(buffer_reg, LUA_TBUFFER as u8, slow);
        let (offset64, offset_number) = emit.whole(offset_reg, slow);
        let (count64, _) = emit.whole(count_reg, slow);
        if member.filter.is_some() {
            let threshold_reg = emit.b.vm_reg(site.arg_res_reg + 5);
            emit.b.load_and_check_tag(threshold_reg, LUA_TNUMBER as u8, slow);
            let threshold = emit.b.inst(IrCmd::LOAD_DOUBLE, &[threshold_reg]);
            emit.set_num(S_THRESHOLD, threshold);
        }
        // The span must fit: offset + count * stride <= length, in int64 so nothing wraps.
        // Stricter than the binder's last-element rule for a stride above the element size; a
        // span the binder would accept but this rejects takes the slow path.
        let stride = emit.stride();
        let stride64 = emit.b.inst(IrCmd::NUM_TO_INT64, &[stride]);
        let bytes = emit.b.inst(IrCmd::MUL_INT64, &[count64, stride64]);
        let end64 = emit.b.inst(IrCmd::ADD_INT64, &[offset64, bytes]);
        let length = emit.buffer_length();
        let fits = emit.b.cond(IrCondition::UnsignedLessEqual);
        emit.b.inst(IrCmd::CHECK_CMP_INT64, &[end64, length, fits, slow]);
        let end_number = emit.b.inst(IrCmd::INT64_TO_NUM, &[end64]);
        emit.set_num(S_END, end_number);
        emit.set_num(S_CURSOR, offset_number);
        emit.set_const(S_ACC, 0.0);
        emit.set_const(S_SEEN, 0.0);
        let element = emit.b.const_int64(size);
        let same = emit.b.cond(IrCondition::Equal);
        emit.b.inst(IrCmd::JUMP_CMP_INT64, &[stride64, element, same, contiguous, strided]);

        for (variant, step) in [(contiguous, Some(size)), (strided, None)] {
            emit.b.begin_block(variant);
            emit.reduction_loop(member, kind, step, count_reg);
        }

        emit.b.begin_block(slow);
        emit.b.namecall_call(site.pcpos);
        emit.jump(done);

        emit.b.begin_block(done);
        LOWERED.fetch_add(1, Ordering::Relaxed);
        true
    }
}
