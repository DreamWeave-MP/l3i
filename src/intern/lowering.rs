//! Native lowering of `Pool:intern` and `Pool:find` (feature `jit`).
//!
//! In `--!native` code with the receiver annotated (`local ids: dream_intern_Pool = ...`), a
//! lookup that finds its identity never leaves native code: the receiver's tag check, the
//! argument tag and range checks, the hash of the span eight bytes at a time (bit-identical to
//! [`super::Interner`]'s, folding case under `ascii-nocase`), the probe of the slot array, and a
//! word-by-word compare against the arena. A hit writes the token. Anything else (an insert, a
//! wrong argument type, a span out of range, a non-integral offset) runs the ordinary
//! namecall and call in place, through the binder, and native code continues after it.
//!
//! # How native code reaches the pool
//!
//! The pool is tagged userdata, so its payload is a [`super::Pool`] at `offsetof(Udata, data)`
//! (16, pinned by `csrc/extra.cpp`). The payload starts with the address of the shared view
//! (`layout` in `intern/mod.rs` pins every offset at compile time), which holds the addresses
//! of the slot array, the entries and the arena, the mask and the policy, refreshed by every
//! insert. Luau's IR has no load from an arbitrary
//! address, but `BUFFER_READI64` under the userdata tag reads `base + index + 16` from any
//! register, so an address minus 16 is a base: every native read here is that, at index 0
//! (Luau's A64 lowering treats a negative constant index as dead code, so offsets are always
//! folded into the base). Spans read from a string start at `offsetof(TString, data)` (24) and
//! from a buffer at `offsetof(Buffer, data)` (8), with the lengths in the high halves of the
//! words at 16 and 0; both offsets are pinned and checked by the layout self-test.
//!
//! # Blocks and state
//!
//! Luau's register allocator gives a value one live range in block order, so a value used
//! after a back edge, or in a block with another predecessor, can be clobbered. The lowering
//! keeps every loop-carried value in the payload's scratch words and reloads it at the top of
//! each block; a value crosses a block boundary only into the block laid out right after its
//! own, when that is the only way in.
//!
//! # Reads past a short span
//!
//! A span shorter than eight bytes is read as the eight bytes ending at its end, shifted down:
//! for a source those bytes start inside the string's or buffer's own header at worst, and the
//! arena keeps eight zero bytes in front of its first spelling for the same reason.

use std::ffi::c_int;
use std::sync::atomic::{AtomicUsize, Ordering};

use super::Pool;
use super::layout::{POOL_SCRATCH, POOL_SHARED, VIEW_ENTRIES, VIEW_FOLD, VIEW_MASK, VIEW_SLOTS, VIEW_TEXT};
use crate::native_code::hooks::{NamecallSite, NativeCodeHooks, NativeContext};
use crate::native_code::ir::{IrBlockKind, IrBuilder, IrCmd, IrCondition, IrOp, bytecode_type};
use crate::raw::ffi::{LUA_TBUFFER, LUA_TNIL, LUA_TNUMBER, LUA_TSTRING, LUA_TUSERDATA};

static LOWERED: AtomicUsize = AtomicUsize::new(0);
static BINDER_CALLS: AtomicUsize = AtomicUsize::new(0);

/// How many call sites the hook has lowered in this process (a diagnostic for tests).
#[doc(hidden)]
pub fn lowered_sites() -> usize {
    LOWERED.load(Ordering::Relaxed)
}

/// How many times `Pool:intern` and `Pool:find` ran through the binder in this process: what a
/// lowered site's slow path costs (a diagnostic for tests).
#[doc(hidden)]
pub fn binder_calls() -> usize {
    BINDER_CALLS.load(Ordering::Relaxed)
}

pub(super) fn count_binder_call() {
    BINDER_CALLS.fetch_add(1, Ordering::Relaxed);
}

/// The hook set; [`super::InternExtension`] registers it.
pub struct InternLowering;

/// `offsetof(Udata, data)`: what `BUFFER_READI64` adds under the userdata tag.
const UDATA_DATA: i64 = 16;
/// `offsetof(TString, data)`; the length is the high half of the word at 16.
const STRING_DATA: i64 = 24;
const STRING_LENGTH_WORD: i64 = 16;
/// `offsetof(Buffer, data)`; the length is the high half of the word at 0.
const BUFFER_DATA: i64 = 8;
const BUFFER_LENGTH_WORD: i64 = 0;

/// The scratch words, by use.
const S_SOURCE: i32 = 0;
const S_LENGTH: i32 = 1;
const S_HASH: i32 = 2;
const S_INDEX: i32 = 3;
const S_SLOT: i32 = 4;
const S_TOKEN: i32 = 5;
const S_TEXT: i32 = 6;

const ONES: i64 = 0x0101_0101_0101_0101;
const HIGHS: i64 = 0x8080_8080_8080_8080_u64 as i64;
const K: i64 = 0x517c_c1b7_2722_0a95;
const FINISH: i64 = 0xbf58_476d_1ce4_e5b9_u64 as i64;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Member {
    Intern,
    Find,
}

impl Member {
    fn of(name: &str) -> Option<Member> {
        match name {
            "intern" => Some(Member::Intern),
            "find" => Some(Member::Find),
            _ => None,
        }
    }
}

/// The IR of one lowered site, with the receiver register it reloads the payload from.
struct Emit<'b, 'a> {
    b: &'b mut IrBuilder<'a>,
    receiver: c_int,
}

impl Emit<'_, '_> {
    fn int(&mut self, value: i64) -> IrOp {
        self.b.const_int64(value)
    }

    fn op(&mut self, cmd: IrCmd, a: IrOp, b: IrOp) -> IrOp {
        self.b.inst(cmd, &[a, b])
    }

    fn add(&mut self, a: IrOp, b: i64) -> IrOp {
        let b = self.int(b);
        self.op(IrCmd::ADD_INT64, a, b)
    }

    fn userdata(&mut self) -> IrOp {
        let receiver = self.b.vm_reg(self.receiver);
        self.b.inst(IrCmd::LOAD_POINTER, &[receiver])
    }

    fn payload_read(&mut self, offset: i32) -> IrOp {
        let userdata = self.userdata();
        let offset = self.b.const_int(offset);
        let tag = self.b.const_tag(LUA_TUSERDATA as u8);
        self.b.inst(IrCmd::BUFFER_READI64, &[userdata, offset, tag])
    }

    fn get(&mut self, word: i32) -> IrOp {
        self.payload_read(POOL_SCRATCH + word * 8)
    }

    fn set(&mut self, word: i32, value: IrOp) {
        let userdata = self.userdata();
        let offset = self.b.const_int(POOL_SCRATCH + word * 8);
        let tag = self.b.const_tag(LUA_TUSERDATA as u8);
        self.b.inst(IrCmd::BUFFER_WRITEI64, &[userdata, offset, value, tag]);
    }

    /// The eight bytes at `address + offset`, the offset folded into the base.
    fn read_at(&mut self, address: IrOp, offset: i64) -> IrOp {
        let base = self.add(address, offset - UDATA_DATA);
        let zero = self.b.const_int(0);
        let tag = self.b.const_tag(LUA_TUSERDATA as u8);
        self.b.inst(IrCmd::BUFFER_READI64, &[base, zero, tag])
    }

    /// The eight bytes at `address + index + offset`.
    fn read_indexed(&mut self, address: IrOp, index: IrOp, offset: i64) -> IrOp {
        let at = self.op(IrCmd::ADD_INT64, address, index);
        self.read_at(at, offset)
    }

    fn view(&mut self, field: i64) -> IrOp {
        let shared = self.payload_read(POOL_SHARED);
        self.read_at(shared, field)
    }

    /// `then` when `a condition b`, else `otherwise`.
    fn select(&mut self, a: IrOp, condition: IrCondition, b: IrOp, then: IrOp, otherwise: IrOp) -> IrOp {
        let condition = self.b.cond(condition);
        self.b.inst(IrCmd::SELECT_INT64, &[otherwise, then, a, b, condition])
    }

    fn jump(&mut self, block: IrOp) {
        self.b.inst(IrCmd::JUMP, &[block]);
    }

    fn branch(&mut self, a: IrOp, b: IrOp, condition: IrCondition, yes: IrOp, no: IrOp) {
        let condition = self.b.cond(condition);
        self.b.inst(IrCmd::JUMP_CMP_INT64, &[a, b, condition, yes, no]);
    }

    fn check(&mut self, a: IrOp, b: IrOp, condition: IrCondition, otherwise: IrOp) {
        let condition = self.b.cond(condition);
        self.b.inst(IrCmd::CHECK_CMP_INT64, &[a, b, condition, otherwise]);
    }

    fn block(&mut self) -> IrOp {
        self.b.block(IrBlockKind::Internal)
    }

    fn begin(&mut self, block: IrOp) {
        self.b.begin_block(block);
    }

    /// `fold_word`: `A`..`Z` lowered in each byte.
    fn fold(&mut self, word: IrOp) -> IrOp {
        let low_mask = self.int(!HIGHS);
        let low = self.op(IrCmd::BITAND_INT64, word, low_mask);
        let at_least_a = self.add(low, ONES * (0x80 - i64::from(b'A')));
        let above_z = self.add(low, ONES * (0x80 - i64::from(b'Z') - 1));
        let not_above_z = self.b.inst(IrCmd::BITNOT_INT64, &[above_z]);
        let not_word = self.b.inst(IrCmd::BITNOT_INT64, &[word]);
        let upper = self.op(IrCmd::BITAND_INT64, at_least_a, not_above_z);
        let upper = self.op(IrCmd::BITAND_INT64, upper, not_word);
        let highs = self.int(HIGHS);
        let upper = self.op(IrCmd::BITAND_INT64, upper, highs);
        let two = self.int(2);
        let case_bits = self.op(IrCmd::BITRSHIFT_INT64, upper, two);
        self.op(IrCmd::BITOR_INT64, word, case_bits)
    }

    fn folded(&mut self, word: IrOp, fold: bool) -> IrOp {
        if fold { self.fold(word) } else { word }
    }

    /// `(rotl(h, 5) ^ word) * K`.
    fn mix(&mut self, hash: IrOp, word: IrOp) -> IrOp {
        let five = self.int(5);
        let rotated = self.op(IrCmd::BITLROTATE_INT64, hash, five);
        let mixed = self.op(IrCmd::BITXOR_INT64, rotated, word);
        let k = self.int(K);
        self.op(IrCmd::MUL_INT64, mixed, k)
    }

    /// The `length < 8` bytes at `address`, zero-extended: the word ending at their end, shifted
    /// down by the bytes in front (a shift of 64, for no bytes, gives 0).
    fn short_word(&mut self, address: IrOp, length: IrOp) -> IrOp {
        let word = self.read_indexed(address, length, -8);
        let eight = self.int(8);
        let missing = self.op(IrCmd::SUB_INT64, eight, length);
        let three = self.int(3);
        let shift = self.op(IrCmd::BITLSHIFT_INT64, missing, three);
        self.op(IrCmd::BITRSHIFT_INT64, word, shift)
    }

    /// The words of a span of `min_length` to 32 bytes (`min_length` at least 8), at
    /// `min(8 * i, length - 8)` for each of `words`: Rust's order, whole words then the last
    /// eight bytes, which for these lengths is the same list. A word whose offset cannot be the
    /// last eight bytes' is read at its constant offset.
    fn class_words(&mut self, address: IrOp, length: IrOp, words: usize, min_length: i64) -> Vec<IrOp> {
        // Offsets less the userdata bias, so each read is one add and one load.
        let last = self.add(length, -8 - UDATA_DATA);
        (0..words)
            .map(|i| {
                let whole = 8 * i as i64;
                let offset = if whole <= min_length - 8 {
                    self.int(whole - UDATA_DATA)
                } else {
                    let whole = self.int(whole - UDATA_DATA);
                    self.select(whole, IrCondition::Less, last, whole, last)
                };
                let base = self.op(IrCmd::ADD_INT64, address, offset);
                let zero = self.b.const_int(0);
                let tag = self.b.const_tag(LUA_TUSERDATA as u8);
                self.b.inst(IrCmd::BUFFER_READI64, &[base, zero, tag])
            })
            .collect()
    }

    /// A number argument in `register` that is an exact non-negative integer, else `otherwise`.
    fn whole(&mut self, register: c_int, otherwise: IrOp) -> IrOp {
        let register = self.b.vm_reg(register);
        self.b.load_and_check_tag(register, LUA_TNUMBER as u8, otherwise);
        let number = self.b.inst(IrCmd::LOAD_DOUBLE, &[register]);
        let value = self.b.inst(IrCmd::NUM_TO_INT64, &[number]);
        let back = self.b.inst(IrCmd::INT64_TO_NUM, &[value]);
        let equal = self.b.cond(IrCondition::Equal);
        self.b.inst(IrCmd::CHECK_CMP_NUM, &[back, number, equal, otherwise]);
        let zero = self.int(0);
        self.check(value, zero, IrCondition::GreaterEqual, otherwise);
        value
    }

    /// The source span into `S_SOURCE`/`S_LENGTH`, then a jump to `next`. A wrong type, a
    /// fractional or negative number, or a span past the end goes to `slow`.
    fn span(&mut self, site: NamecallSite, slow: IrOp, next: IrOp) {
        let source_reg = site.arg_res_reg + 2;
        let buffer = self.block();
        let not_buffer = self.block();
        let bounds = self.block();

        let source = self.b.vm_reg(source_reg);
        let tag = self.b.inst(IrCmd::LOAD_TAG, &[source]);
        let buffer_tag = self.b.const_tag(LUA_TBUFFER as u8);
        self.b.inst(IrCmd::JUMP_EQ_TAG, &[tag, buffer_tag, buffer, not_buffer]);

        for (block, data, length_word) in
            [(buffer, BUFFER_DATA, BUFFER_LENGTH_WORD), (not_buffer, STRING_DATA, STRING_LENGTH_WORD)]
        {
            self.begin(block);
            let source = self.b.vm_reg(source_reg);
            if block == not_buffer {
                self.b.load_and_check_tag(source, LUA_TSTRING as u8, slow);
            }
            let object = self.b.inst(IrCmd::LOAD_POINTER, &[source]);
            let data = self.add(object, data);
            let length_word = self.read_at(object, length_word);
            let thirty_two = self.int(32);
            let length = self.op(IrCmd::BITRSHIFT_INT64, length_word, thirty_two);
            self.set(S_SOURCE, data);
            self.set(S_LENGTH, length);
            self.jump(bounds);
        }

        self.begin(bounds);
        if site.params >= 3 {
            let total = self.get(S_LENGTH);
            let offset = self.whole(site.arg_res_reg + 3, slow);
            self.check(offset, total, IrCondition::LessEqual, slow);
            let rest = self.op(IrCmd::SUB_INT64, total, offset);
            let length = if site.params == 4 {
                let length = self.whole(site.arg_res_reg + 4, slow);
                self.check(length, rest, IrCondition::LessEqual, slow);
                length
            } else {
                rest
            };
            let source = self.get(S_SOURCE);
            let source = self.op(IrCmd::ADD_INT64, source, offset);
            self.set(S_SOURCE, source);
            self.set(S_LENGTH, length);
        }
        self.jump(next);
    }

    /// The hash of a span of 8 to 32 bytes, in one block: every mix computed, the one after
    /// the last real word selected.
    fn hash_class(&mut self, fold: bool, words: usize, min_length: i64) -> IrOp {
        let source = self.get(S_SOURCE);
        let length = self.get(S_LENGTH);
        let values = self.class_words(source, length, words, min_length);
        let mut hash = length;
        let mut mixes = Vec::with_capacity(words);
        for value in values {
            let value = self.folded(value, fold);
            hash = self.mix(hash, value);
            mixes.push(hash);
        }
        // `ceil(length / 8)` words: the mix after word i counts when length > 8 * i.
        let mut result = mixes[0];
        for (i, mix) in mixes.iter().enumerate().skip(1) {
            let bound = self.int(8 * i as i64);
            result = self.select(length, IrCondition::Greater, bound, *mix, result);
        }
        result
    }

    /// One policy's lookup: a branch on the span's length into four classes (under 8 bytes, 8
    /// to 16, 17 to 32, longer), each with its own hash and its own probe loop whose compare
    /// knows the class, ending in `hit` with the token in `S_TOKEN` or `absent`.
    fn lookup(&mut self, fold: bool, hit: IrOp, absent: IrOp) {
        let short = self.block();
        let sixteen = self.block();
        let thirty_two = self.block();
        let long = self.block();
        let up_to_sixteen = self.block();
        let up_to_thirty_two = self.block();

        let length = self.get(S_LENGTH);
        let eight = self.int(8);
        self.branch(length, eight, IrCondition::Less, short, up_to_sixteen);

        self.begin(up_to_sixteen);
        let length = self.get(S_LENGTH);
        let limit = self.int(16);
        self.branch(length, limit, IrCondition::LessEqual, sixteen, up_to_thirty_two);

        self.begin(up_to_thirty_two);
        let length = self.get(S_LENGTH);
        let limit = self.int(32);
        self.branch(length, limit, IrCondition::LessEqual, thirty_two, long);

        self.begin(short);
        let source = self.get(S_SOURCE);
        let length = self.get(S_LENGTH);
        let value = self.short_word(source, length);
        let value = self.folded(value, fold);
        let hash = self.mix(length, value);
        self.finish(hash);
        self.probe(fold, Class::Short, hit, absent);

        self.begin(sixteen);
        let hash = self.hash_class(fold, 2, 8);
        self.finish(hash);
        self.probe(fold, Class::Words(2, 8), hit, absent);

        self.begin(thirty_two);
        let hash = self.hash_class(fold, 4, 17);
        self.finish(hash);
        self.probe(fold, Class::Words(4, 17), hit, absent);

        self.begin(long);
        let finish = self.block();
        self.hash_loop(fold, finish);
        self.begin(finish);
        let hash = self.get(S_HASH);
        self.finish(hash);
        self.probe(fold, Class::Long, hit, absent);
    }

    /// Whole words in a loop, then the last eight bytes, into `S_HASH`; then `finish`.
    fn hash_loop(&mut self, fold: bool, finish: IrOp) {
        let words = self.block();
        let word = self.block();
        let tail = self.block();
        let tail_word = self.block();

        let length = self.get(S_LENGTH);
        self.set(S_HASH, length);
        let zero = self.int(0);
        self.set(S_INDEX, zero);
        self.jump(words);

        self.begin(words);
        let index = self.get(S_INDEX);
        let length = self.get(S_LENGTH);
        let end = self.add(index, 8);
        self.branch(end, length, IrCondition::UnsignedLessEqual, word, tail);

        self.begin(word);
        let source = self.get(S_SOURCE);
        let index = self.get(S_INDEX);
        let value = self.read_indexed(source, index, 0);
        let value = self.folded(value, fold);
        let hash = self.get(S_HASH);
        let hash = self.mix(hash, value);
        self.set(S_HASH, hash);
        let next = self.add(index, 8);
        self.set(S_INDEX, next);
        self.jump(words);

        self.begin(tail);
        let index = self.get(S_INDEX);
        let length = self.get(S_LENGTH);
        self.branch(index, length, IrCondition::UnsignedLess, tail_word, finish);

        self.begin(tail_word);
        let source = self.get(S_SOURCE);
        let length = self.get(S_LENGTH);
        let value = self.read_indexed(source, length, -8);
        let value = self.folded(value, fold);
        let hash = self.get(S_HASH);
        let hash = self.mix(hash, value);
        self.set(S_HASH, hash);
        self.jump(finish);
    }

    /// Finishes `hash` to 32 bits into `S_HASH` and the first slot into `S_SLOT`.
    fn finish(&mut self, hash: IrOp) {
        let twenty_nine = self.int(29);
        let high = self.op(IrCmd::BITRSHIFT_INT64, hash, twenty_nine);
        let hash = self.op(IrCmd::BITXOR_INT64, hash, high);
        let finish_k = self.int(FINISH);
        let hash = self.op(IrCmd::MUL_INT64, hash, finish_k);
        let thirty_two = self.int(32);
        let high = self.op(IrCmd::BITRSHIFT_INT64, hash, thirty_two);
        let hash = self.op(IrCmd::BITXOR_INT64, hash, high);
        let low_half = self.int(0xffff_ffff);
        let hash = self.op(IrCmd::BITAND_INT64, hash, low_half);
        self.set(S_HASH, hash);
        let mask = self.view(VIEW_MASK);
        let slot = self.op(IrCmd::BITAND_INT64, hash, mask);
        self.set(S_SLOT, slot);
    }

    /// `S_SOURCE` against `S_TEXT` over a span of `min_length` to 32 bytes: the raw words
    /// first, since a duplicate usually repeats its spelling, and under `ascii-nocase` the
    /// folded words only when they differ.
    fn compare_class(&mut self, fold: bool, words: usize, min_length: i64, hit: IrOp, next: IrOp) {
        let source = self.get(S_SOURCE);
        let spelling = self.get(S_TEXT);
        let length = self.get(S_LENGTH);
        let a = self.class_words(source, length, words, min_length);
        let b = self.class_words(spelling, length, words, min_length);
        let mut difference = self.int(0);
        for (a, b) in a.iter().zip(&b) {
            let different = self.op(IrCmd::BITXOR_INT64, *a, *b);
            difference = self.op(IrCmd::BITOR_INT64, difference, different);
        }
        let zero = self.int(0);
        if !fold {
            self.branch(difference, zero, IrCondition::Equal, hit, next);
            return;
        }
        // `folded` follows this block and is reached only from it: the words are still live.
        let folded = self.block();
        self.branch(difference, zero, IrCondition::Equal, hit, folded);
        self.begin(folded);
        let mut difference = self.int(0);
        for (a, b) in a.into_iter().zip(b) {
            let a = self.fold(a);
            let b = self.fold(b);
            let different = self.op(IrCmd::BITXOR_INT64, a, b);
            difference = self.op(IrCmd::BITOR_INT64, difference, different);
        }
        let zero = self.int(0);
        self.branch(difference, zero, IrCondition::Equal, hit, next);
    }

    /// `S_SOURCE` against `S_TEXT` over a span longer than 32 bytes, in a loop.
    fn compare_loop(&mut self, fold: bool, hit: IrOp, next: IrOp) {
        let words = self.block();
        let word = self.block();
        let word_equal = self.block();
        let tail = self.block();
        let tail_word = self.block();

        let zero = self.int(0);
        self.set(S_INDEX, zero);
        self.jump(words);

        self.begin(words);
        let index = self.get(S_INDEX);
        let length = self.get(S_LENGTH);
        let end = self.add(index, 8);
        self.branch(end, length, IrCondition::UnsignedLessEqual, word, tail);

        self.begin(word);
        let source = self.get(S_SOURCE);
        let spelling = self.get(S_TEXT);
        let index = self.get(S_INDEX);
        let a = self.read_indexed(source, index, 0);
        let b = self.read_indexed(spelling, index, 0);
        let a = self.folded(a, fold);
        let b = self.folded(b, fold);
        self.branch(a, b, IrCondition::Equal, word_equal, next);

        self.begin(word_equal);
        let index = self.get(S_INDEX);
        let index = self.add(index, 8);
        self.set(S_INDEX, index);
        self.jump(words);

        self.begin(tail);
        let index = self.get(S_INDEX);
        let length = self.get(S_LENGTH);
        self.branch(index, length, IrCondition::UnsignedLess, tail_word, hit);

        self.begin(tail_word);
        let source = self.get(S_SOURCE);
        let spelling = self.get(S_TEXT);
        let length = self.get(S_LENGTH);
        let a = self.read_indexed(source, length, -8);
        let b = self.read_indexed(spelling, length, -8);
        let a = self.folded(a, fold);
        let b = self.folded(b, fold);
        self.branch(a, b, IrCondition::Equal, hit, next);
    }

    /// The probe loop from `S_SLOT` for one length class, comparing candidates against the
    /// span; the current block jumps into it.
    fn probe(&mut self, fold: bool, class: Class, hit: IrOp, absent: IrOp) {
        let probe = self.block();
        let occupied = self.block();
        let candidate = self.block();
        let compare = self.block();
        let next = self.block();

        self.jump(probe);

        // One slot: empty ends the probe; a different hash moves on.
        self.begin(probe);
        let slots = self.view(VIEW_SLOTS);
        let at = self.get(S_SLOT);
        let three = self.int(3);
        let byte_offset = self.op(IrCmd::BITLSHIFT_INT64, at, three);
        let slot = self.read_indexed(slots, byte_offset, 0);
        let zero = self.int(0);
        self.branch(slot, zero, IrCondition::Equal, absent, occupied);

        // `occupied` follows `probe` and is reached only from it, so `slot` is still live; so
        // is `candidate` from `occupied`.
        self.begin(occupied);
        let thirty_two_bits = self.int(32);
        let slot_hash = self.op(IrCmd::BITRSHIFT_INT64, slot, thirty_two_bits);
        let hash = self.get(S_HASH);
        self.branch(slot_hash, hash, IrCondition::Equal, candidate, next);

        self.begin(candidate);
        let low_half = self.int(0xffff_ffff);
        let token = self.op(IrCmd::BITAND_INT64, slot, low_half);
        self.set(S_TOKEN, token);
        let entries = self.view(VIEW_ENTRIES);
        let three = self.int(3);
        let byte_offset = self.op(IrCmd::BITLSHIFT_INT64, token, three);
        let entry = self.read_indexed(entries, byte_offset, -8);
        let text = self.view(VIEW_TEXT);
        let start = self.op(IrCmd::BITAND_INT64, entry, low_half);
        let spelling = self.op(IrCmd::ADD_INT64, text, start);
        self.set(S_TEXT, spelling);
        let thirty_two_bits = self.int(32);
        let stored_length = self.op(IrCmd::BITRSHIFT_INT64, entry, thirty_two_bits);
        let length = self.get(S_LENGTH);
        self.branch(stored_length, length, IrCondition::Equal, compare, next);

        self.begin(compare);
        match class {
            Class::Short => {
                let source = self.get(S_SOURCE);
                let spelling = self.get(S_TEXT);
                let length = self.get(S_LENGTH);
                let a = self.short_word(source, length);
                let b = self.short_word(spelling, length);
                let a = self.folded(a, fold);
                let b = self.folded(b, fold);
                self.branch(a, b, IrCondition::Equal, hit, next);
            }
            Class::Words(words, min_length) => self.compare_class(fold, words, min_length, hit, next),
            Class::Long => self.compare_loop(fold, hit, next),
        }

        self.begin(next);
        let at = self.get(S_SLOT);
        let at = self.add(at, 1);
        let mask = self.view(VIEW_MASK);
        let at = self.op(IrCmd::BITAND_INT64, at, mask);
        self.set(S_SLOT, at);
        self.jump(probe);
    }
}

/// The span lengths a probe's compare is specialised for.
#[derive(Clone, Copy)]
enum Class {
    /// Under 8 bytes: one shifted word.
    Short,
    /// 8 to 16 bytes (2 words) or 17 to 32 (4), with the class's shortest length: every word
    /// in one block.
    Words(usize, i64),
    /// Longer: a loop.
    Long,
}

fn store_number(build: &mut IrBuilder<'_>, register: IrOp, value: IrOp) {
    build.inst(IrCmd::STORE_DOUBLE, &[register, value]);
    let tag = build.const_tag(LUA_TNUMBER as u8);
    build.inst(IrCmd::STORE_TAG, &[register, tag]);
}

impl NativeCodeHooks for InternLowering {
    fn userdata_namecall_type(&self, context: &NativeContext<'_>, userdata_type: u8, member: &str) -> u8 {
        if context.userdata_type_of::<Pool>() != Some(userdata_type) {
            return bytecode_type::ANY;
        }
        match Member::of(member) {
            Some(Member::Intern) => bytecode_type::NUMBER,
            Some(Member::Find) => bytecode_type::ANY,
            None => bytecode_type::ANY,
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
        if context.userdata_type_of::<Pool>() != Some(userdata_type) {
            return false;
        }
        let Some(member) = Member::of(member) else { return false };
        if !(2..=4).contains(&site.params) || !(0..=1).contains(&site.results) {
            return false;
        }
        let Some(tag) = context.tag_of::<Pool>() else { return false };

        // The slow path: the namecall and call Luau would have emitted, then on to `done`.
        let slow = build.fallback_block(site.pcpos as u32);
        let hit = build.block(IrBlockKind::Internal);
        let absent = build.block(IrBlockKind::Internal);
        let done = build.block(IrBlockKind::Internal);
        let exact = build.block(IrBlockKind::Internal);
        let nocase = build.block(IrBlockKind::Internal);
        let policy = build.block(IrBlockKind::Internal);

        let mut emit = Emit { b: build, receiver: site.source_reg };
        let userdata = emit.userdata();
        let tag = emit.b.const_int(i32::from(tag));
        emit.b.inst(IrCmd::CHECK_USERDATA_TAG, &[userdata, tag, slow]);
        emit.span(site, slow, policy);

        emit.begin(policy);
        let fold = emit.view(VIEW_FOLD);
        let zero = emit.int(0);
        emit.branch(fold, zero, IrCondition::Equal, exact, nocase);

        emit.begin(exact);
        emit.lookup(false, hit, absent);
        emit.begin(nocase);
        emit.lookup(true, hit, absent);

        emit.begin(hit);
        if site.results == 1 {
            let token = emit.get(S_TOKEN);
            let number = emit.b.inst(IrCmd::INT64_TO_NUM, &[token]);
            let result = emit.b.vm_reg(site.arg_res_reg);
            store_number(emit.b, result, number);
        }
        emit.jump(done);

        emit.begin(absent);
        match member {
            // Not there: an insert, which is the binder's. Its own copy of the call, since a
            // regular block whose only way out is a fallback block breaks Luau's live-register
            // analysis, which reads a fallback block as another implementation of the same work.
            Member::Intern => {
                emit.b.namecall_call(site.pcpos);
                emit.jump(done);
            }
            Member::Find => {
                if site.results == 1 {
                    let result = emit.b.vm_reg(site.arg_res_reg);
                    let nil = emit.b.const_tag(LUA_TNIL as u8);
                    emit.b.inst(IrCmd::STORE_TAG, &[result, nil]);
                }
                emit.jump(done);
            }
        }

        emit.begin(slow);
        emit.b.namecall_call(site.pcpos);
        emit.jump(done);

        emit.begin(done);
        LOWERED.fetch_add(1, Ordering::Relaxed);
        true
    }
}
