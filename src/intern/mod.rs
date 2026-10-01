//! Textual identity as numbers: the `dream.intern` extension, module `@dream/intern`.
//!
//! A pool maps byte sequences to small whole numbers under one equivalence policy. Equivalent
//! inputs get the same number, different ones different numbers, and from then on identity is
//! number equality and number table keys:
//!
//! ```lua
//! local intern = require('@dream/intern')
//! local ids = intern.new('ascii-nocase')
//! local id = ids:intern(record, offset, length)   -- a buffer span: no Luau string is made
//! assert(id == ids:intern('Caius Cosades') and id == ids:intern('CAIUS COSADES'))
//! print(ids:resolve(id))                            -- the first spelling the pool saw
//! ```
//!
//! # The hot path
//!
//! Under `jit`, in `--!native` code with the pool annotated, `pool:intern` and `pool:find` lower
//! to native code ([`lowering`]): a lookup that finds its identity never leaves it, and an
//! insert or a bad argument runs the bound method in place. That is the fastest form by far:
//!
//! ```lua
//! --!native
//! local ids: dream_intern_Pool = intern.new('ascii-nocase')
//! for i = 0, count - 1 do
//!     local id = ids:intern(text, buffer.readu32(spans, i * 8), buffer.readu32(spans, i * 8 + 4))
//! end
//! ```
//!
//! `pool:interner()` returns the same operation as a plain function bound to the pool, which
//! skips the method lookup on every call. It is a call, not a namecall, so it never lowers; it
//! is the faster form only in interpreted code.
//!
//! # Policies
//!
//! - `exact`: byte-for-byte.
//! - `ascii-nocase`: `A`..`Z` equal `a`..`z`; every other byte, UTF-8 included, compares
//!   exactly. SQLite's `NOCASE`. Folding happens inside the hash and the comparison, a word at a
//!   time; a folded copy of the input is never made.
//!
//! - `rules` ([`Rules`], `intern.new({ nocase?, replace?, collapse?, trimStart?, trimEnd? })`):
//!   keys normalized before they are compared. ASCII letters lowered when `nocase`, up to two
//!   bytes replaced by others, runs of one byte collapsed to one, and one byte trimmed from the
//!   start and from the end, in that order, so `Meshes\\X//Rock.NIF` and `meshes/x/rock.nif`
//!   are one identity under `{ nocase = true, replace = { ['\\'] = '/' }, collapse = '/',
//!   trimStart = '/' }`. The pool keeps the normal form, which is what `resolve` returns: under
//!   rules that drop bytes, a first spelling would not say which bytes the identity has.
//!
//! Unicode case folding, and any rule a byte map and these switches cannot say, are a domain's
//! own, applied before interning.
//!
//! # Tokens and lifetime
//!
//! A token is the 1-based position of its identity in its pool: `1, 2, 3, ...` in first-seen
//! order, dense, below 2^32, as a Luau number. A number and not an `integer`, because a table
//! keyed by dense numbers keeps them in its array part, and an `integer` key always hashes: a
//! read costs about a fifth of the instructions of an integer or string key, and a third of an
//! integer key's L1 misses (`benches/intern.rs`). Dense tokens also fit a `u32` column
//! (`buffer.writeu32`).
//!
//! Tokens are pool-relative: two pools hand out the same numbers, and a token means something
//! only to the pool that made it. Pools only grow (no removal, no reuse), so a token never
//! changes meaning while its pool lives, and needs no generation or pool bits. Dropping the
//! pool frees everything at once; tokens kept after that are plain numbers.
//!
//! # Storage
//!
//! The first spelling of each identity is copied once into the pool's text arena; duplicates
//! copy nothing and allocate nothing, native or VM. The index is open addressing with the hash
//! in each slot: 8 bytes per slot, 8 per identity, plus the text.

#[cfg(feature = "jit")]
pub mod lowering;

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use crate::byte_rules::one_byte;
use crate::convert::{BytesView, Exact};
use crate::error::{Error, Result};
use crate::extension::{CompilerTypePolicy, Extension, ExtensionDescriptor, TagPolicy};
use crate::options::Options;
use crate::stack::ValueView;
use crate::userdata::Owned;

/// The extension id.
pub const EXTENSION_ID: &str = "dream.intern";
/// The module path.
pub const MODULE: &str = "@dream/intern";

/// When two byte sequences are the same identity.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Policy {
    /// Byte-for-byte.
    Exact,
    /// ASCII letters compare without case; every other byte exactly.
    AsciiNoCase,
    /// Keys compare by their normal form under a pool's [`Rules`].
    Rules,
}

/// The `rules` policy: how a key is normalized before it is compared, step by step.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Rules {
    /// Lower `A`..`Z`.
    pub nocase: bool,
    /// Bytes replaced by others, at most [`Rules::MAX_REPLACEMENTS`]; a replaced byte is not
    /// also lowered.
    pub replace: Vec<(u8, u8)>,
    /// A byte whose runs, after the replacements, collapse to one.
    pub collapse: Option<u8>,
    /// A byte removed from the start, after collapsing.
    pub trim_start: Option<u8>,
    /// A byte removed from the end.
    pub trim_end: Option<u8>,
}

impl Rules {
    /// How many replacements a pool takes: what native code applies to every word it reads.
    pub const MAX_REPLACEMENTS: usize = 2;

    /// The byte each byte becomes.
    fn map(&self, byte: u8) -> u8 {
        match self.replace.iter().find(|(old, _)| *old == byte) {
            Some((_, new)) => *new,
            None if self.nocase => byte.to_ascii_lowercase(),
            None => byte,
        }
    }

    fn byte_rules(&self) -> crate::byte_rules::ByteRules {
        crate::byte_rules::ByteRules {
            map: std::array::from_fn(|byte| self.map(byte as u8)),
            collapse: self.collapse,
            trim_start: self.trim_start,
            trim_end: self.trim_end,
        }
    }
}

impl Policy {
    /// The policy named `name`: `exact` or `ascii-nocase`.
    pub fn parse(name: &str) -> Option<Policy> {
        match name {
            "exact" => Some(Policy::Exact),
            "ascii-nocase" => Some(Policy::AsciiNoCase),
            "rules" => Some(Policy::Rules),
            _ => None,
        }
    }

    /// The policy's name.
    pub const fn name(self) -> &'static str {
        match self {
            Policy::Exact => "exact",
            Policy::AsciiNoCase => "ascii-nocase",
            Policy::Rules => "rules",
        }
    }
}

const ONES: u64 = 0x0101_0101_0101_0101;
const HIGHS: u64 = 0x8080_8080_8080_8080;

/// Eight bytes with `A`..`Z` lowered: the high bit of `upper` marks a byte below 0x80 that is
/// at least `A` and at most `Z`, and shifting it to 0x20 is the case bit.
#[inline(always)]
const fn fold_word(word: u64) -> u64 {
    let low = word & !HIGHS;
    let at_least_a = low.wrapping_add(ONES * (0x80 - b'A' as u64));
    let above_z = low.wrapping_add(ONES * (0x80 - b'Z' as u64 - 1));
    let upper = at_least_a & !above_z & !word & HIGHS;
    word | (upper >> 2)
}

/// The little-endian word at `at`.
///
/// # Safety
/// `at + 8 <= bytes.len()`.
#[inline(always)]
unsafe fn load(bytes: &[u8], at: usize) -> u64 {
    debug_assert!(at + 8 <= bytes.len());
    // SAFETY: forwarded bounds.
    u64::from_le(unsafe { bytes.as_ptr().add(at).cast::<u64>().read_unaligned() })
}

/// Fewer than eight bytes, zero-extended, from at most three loads that may overlap.
#[inline(always)]
fn load_short(bytes: &[u8]) -> u64 {
    let len = bytes.len();
    debug_assert!(len < 8);
    let at = bytes.as_ptr();
    // SAFETY: every read lies inside `bytes`; overlapping reads put the same byte in the same
    // place, so OR-ing them is exact.
    unsafe {
        if len >= 4 {
            let low = u64::from(u32::from_le(at.cast::<u32>().read_unaligned()));
            let high = u64::from(u32::from_le(at.add(len - 4).cast::<u32>().read_unaligned()));
            low | (high << ((len - 4) * 8))
        } else if len > 0 {
            u64::from(*at)
                | (u64::from(*at.add(len / 2)) << (len / 2 * 8))
                | (u64::from(*at.add(len - 1)) << ((len - 1) * 8))
        } else {
            0
        }
    }
}

/// Calls `f` with every eight-byte word of `bytes`: the whole words, then the last eight bytes
/// again when the length is not a multiple of eight (an overlapping read, the same bytes in the
/// same place for equal inputs), or one zero-extended word for fewer than eight.
#[inline(always)]
fn words(bytes: &[u8], mut f: impl FnMut(u64)) {
    let len = bytes.len();
    if len < 8 {
        f(load_short(bytes));
        return;
    }
    let mut at = 0;
    while at + 8 <= len {
        // SAFETY: the loop bound.
        f(unsafe { load(bytes, at) });
        at += 8;
    }
    if at < len {
        // SAFETY: len >= 8.
        f(unsafe { load(bytes, len - 8) });
    }
}

#[inline(always)]
const fn fold<const FOLD: bool>(word: u64) -> u64 {
    if FOLD { fold_word(word) } else { word }
}

const K: u64 = 0x517c_c1b7_2722_0a95;

#[inline(always)]
fn hash<const FOLD: bool>(bytes: &[u8]) -> u32 {
    let mut h = bytes.len() as u64;
    words(bytes, |word| h = (h.rotate_left(5) ^ fold::<FOLD>(word)).wrapping_mul(K));
    h ^= h >> 29;
    h = h.wrapping_mul(0xbf58_476d_1ce4_e5b9);
    (h ^ (h >> 32)) as u32
}

/// Two words equal under the policy. Most duplicates repeat a spelling exactly, so the raw
/// compare decides them and only a difference is folded.
#[inline(always)]
const fn same_word<const FOLD: bool>(a: u64, b: u64) -> bool {
    a == b || (FOLD && fold_word(a) == fold_word(b))
}

/// Equal under the policy; `a` and `b` have the same length.
#[inline(always)]
fn equal<const FOLD: bool>(a: &[u8], b: &[u8]) -> bool {
    debug_assert_eq!(a.len(), b.len());
    let len = a.len();
    if len < 8 {
        return same_word::<FOLD>(load_short(a), load_short(b));
    }
    let mut at = 0;
    while at + 8 <= len {
        // SAFETY: the loop bound, and the lengths are equal.
        if unsafe { !same_word::<FOLD>(load(a, at), load(b, at)) } {
            return false;
        }
        at += 8;
    }
    // SAFETY: len >= 8.
    at == len || unsafe { same_word::<FOLD>(load(a, len - 8), load(b, len - 8)) }
}

/// One identity's first spelling in the arena. Native code reads it as one little-endian word:
/// `start` in the low half, `len` in the high half.
#[derive(Clone, Copy)]
#[repr(C)]
struct Entry {
    start: u32,
    len: u32,
}

/// Zero bytes in front of the arena's first spelling, so a read of the eight bytes that end at a
/// short spelling's end never starts before the arena (native code compares spellings shorter
/// than a word that way).
const TEXT_PAD: usize = 8;
/// The table a pool starts with: never empty, so a lookup always has a slot to read.
const FIRST_SLOTS: usize = 16;

/// The pool itself, without Luau: usable from Rust, and what `dream.intern.Pool` wraps.
pub struct Interner {
    policy: Policy,
    /// The `rules` policy's rules, and the normalization they make.
    rules: Option<(Rules, crate::byte_rules::ByteRules)>,
    /// Where a key that isn't in its normal form is normalized: reused, never shrunk.
    scratch: Vec<u8>,
    text: Vec<u8>,
    entries: Vec<Entry>,
    /// 0 is empty; otherwise the hash in the high half and the token in the low half.
    slots: Vec<u64>,
}

impl Interner {
    /// An empty pool; `Policy::Rules` with rules that change nothing.
    pub fn new(policy: Policy) -> Interner {
        let rules = (policy == Policy::Rules).then(|| (Rules::default(), Rules::default().byte_rules()));
        Interner {
            policy,
            rules,
            scratch: Vec::new(),
            text: vec![0; TEXT_PAD],
            entries: Vec::new(),
            slots: vec![0; FIRST_SLOTS],
        }
    }

    /// An empty pool under `rules`; more than [`Rules::MAX_REPLACEMENTS`] replacements is an
    /// error.
    pub fn with_rules(rules: Rules) -> Result<Interner> {
        if rules.replace.iter().any(|(old, _)| *old == 0) {
            return Err(Error::runtime("intern: NUL can't be replaced"));
        }
        if rules.replace.len() > Rules::MAX_REPLACEMENTS {
            return Err(Error::runtime(format!(
                "intern: at most {} replacements, got {}",
                Rules::MAX_REPLACEMENTS,
                rules.replace.len()
            )));
        }
        let mut interner = Interner::new(Policy::Rules);
        let byte_rules = rules.byte_rules();
        interner.rules = Some((rules, byte_rules));
        Ok(interner)
    }

    /// The `rules` policy's rules.
    pub fn rules(&self) -> Option<&Rules> {
        self.rules.as_ref().map(|(rules, _)| rules)
    }

    /// The pool's policy.
    pub fn policy(&self) -> Policy {
        self.policy
    }

    /// How many identities the pool holds; the last token handed out.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Bytes of native memory the pool holds.
    pub fn memory(&self) -> usize {
        self.text.capacity()
            + self.scratch.capacity()
            + self.entries.capacity() * size_of::<Entry>()
            + self.slots.capacity() * size_of::<u64>()
    }

    /// The first spelling of `token`, which this pool made.
    #[inline(always)]
    fn spelling(&self, token: u32) -> &[u8] {
        debug_assert!(token as usize >= 1 && token as usize <= self.entries.len());
        // SAFETY: tokens in slots index `entries`, and entries index `text`; neither shrinks.
        unsafe {
            let entry = *self.entries.get_unchecked(token as usize - 1);
            self.text.get_unchecked(entry.start as usize..(entry.start + entry.len) as usize)
        }
    }

    /// The token for `bytes`, or the empty slot it would take. `slots` is not empty.
    #[inline(always)]
    fn probe<const FOLD: bool>(&self, bytes: &[u8], hash: u32) -> std::result::Result<u32, usize> {
        let mask = self.slots.len() - 1;
        let mut at = hash as usize & mask;
        loop {
            // SAFETY: `at` is masked to the power-of-two table.
            let slot = unsafe { *self.slots.get_unchecked(at) };
            if slot == 0 {
                return Err(at);
            }
            if (slot >> 32) as u32 == hash {
                let stored = self.spelling(slot as u32);
                if stored.len() == bytes.len() && equal::<FOLD>(stored, bytes) {
                    return Ok(slot as u32);
                }
            }
            at = (at + 1) & mask;
        }
    }

    /// The token `bytes` already has, without adding it.
    pub fn find(&self, bytes: &[u8]) -> Option<u32> {
        match self.policy {
            Policy::Exact => self.probe::<false>(bytes, hash::<false>(bytes)).ok(),
            Policy::AsciiNoCase => self.probe::<true>(bytes, hash::<true>(bytes)).ok(),
            Policy::Rules => {
                let normal = &self.rules.as_ref()?.1;
                if normal.is_normal(bytes) {
                    return self.probe::<false>(bytes, hash::<false>(bytes)).ok();
                }
                let mut key = Vec::new();
                normal.normalize_into(bytes, &mut key);
                self.probe::<false>(&key, hash::<false>(&key)).ok()
            }
        }
    }

    /// The token for `bytes`, adding it on first sight.
    pub fn intern(&mut self, bytes: &[u8]) -> Result<u32> {
        match self.policy {
            Policy::Exact => self.intern_with::<false>(bytes),
            Policy::AsciiNoCase => self.intern_with::<true>(bytes),
            Policy::Rules => {
                let Some((_, normal)) = &self.rules else { return self.intern_with::<false>(bytes) };
                if normal.is_normal(bytes) {
                    return self.intern_with::<false>(bytes);
                }
                let mut key = std::mem::take(&mut self.scratch);
                key.clear();
                normal.normalize_into(bytes, &mut key);
                let token = self.intern_with::<false>(&key);
                self.scratch = key;
                token
            }
        }
    }

    #[inline(always)]
    fn intern_with<const FOLD: bool>(&mut self, bytes: &[u8]) -> Result<u32> {
        if (self.entries.len() + 1) * 4 > self.slots.len() * 3 {
            self.grow()?;
        }
        let hash = hash::<FOLD>(bytes);
        let at = match self.probe::<FOLD>(bytes, hash) {
            Ok(token) => return Ok(token),
            Err(at) => at,
        };
        self.insert(bytes, hash, at)
    }

    #[cold]
    #[inline(never)]
    fn insert(&mut self, bytes: &[u8], hash: u32, at: usize) -> Result<u32> {
        if self.text.len() + bytes.len() > u32::MAX as usize {
            return Err(Error::runtime("intern: the pool's text passed 4 GiB"));
        }
        let start = self.text.len() as u32;
        self.text.extend_from_slice(bytes);
        self.entries.push(Entry { start, len: bytes.len() as u32 });
        let token = self.entries.len() as u32;
        self.slots[at] = (u64::from(hash) << 32) | u64::from(token);
        Ok(token)
    }

    /// The first spelling of `token`, if this pool made it.
    pub fn resolve(&self, token: i64) -> Option<&[u8]> {
        let token = u32::try_from(token).ok().filter(|t| *t != 0 && (*t as usize) <= self.entries.len())?;
        Some(self.spelling(token))
    }

    #[cold]
    fn grow(&mut self) -> Result<()> {
        if self.entries.len() >= u32::MAX as usize - 1 {
            return Err(Error::runtime("intern: the pool holds 2^32 - 1 identities"));
        }
        let capacity = self.slots.len() * 2;
        let old = std::mem::replace(&mut self.slots, vec![0; capacity]);
        let mask = capacity - 1;
        for slot in old.into_iter().filter(|slot| *slot != 0) {
            let mut at = (slot >> 32) as usize & mask;
            while self.slots[at] != 0 {
                at = (at + 1) & mask;
            }
            self.slots[at] = slot;
        }
        Ok(())
    }
}

/// Where native code finds a pool's memory: the addresses and sizes of the interner's arrays,
/// refreshed after every change that can move them. Addresses are stored as integers.
#[repr(C)]
struct View {
    /// `slots[0]`.
    slots: Cell<u64>,
    /// `slots.len() - 1`.
    mask: Cell<u64>,
    /// `entries[0]`.
    entries: Cell<u64>,
    /// `text[0]`.
    text: Cell<u64>,
    /// 0 under `exact`, 1 under `ascii-nocase`, 2 under `rules`.
    fold: Cell<u64>,
    /// The `rules` policy, as native code applies it to a word: `rules` in `intern/mod.rs`.
    rules: NativeRules,
}

/// A pool's [`Rules`] as words native code applies eight bytes at a time. A slot that is unused
/// flips nothing, and a switch that is off matches no byte.
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct NativeRules {
    /// All ones when `nocase`, else 0: the case bits are masked with it.
    fold_mask: u64,
    /// Per replacement: the old byte in every lane, and in every lane the bits that turn its
    /// (folded) self into the new byte.
    replace: [[u64; 2]; Rules::MAX_REPLACEMENTS],
    /// The collapse byte in every lane, and `0x80` in every lane when it is on, else 0.
    collapse: u64,
    collapse_mask: u64,
    /// The trimmed bytes, or 0x100, which no byte equals.
    trim_start: u64,
    trim_end: u64,
}

impl NativeRules {
    const ONES: u64 = 0x0101_0101_0101_0101;

    fn of(rules: &Rules) -> NativeRules {
        let mut native = NativeRules {
            fold_mask: if rules.nocase { u64::MAX } else { 0 },
            collapse: u64::from(rules.collapse.unwrap_or(0)) * Self::ONES,
            collapse_mask: if rules.collapse.is_some() { HIGHS } else { 0 },
            trim_start: rules.trim_start.map_or(0x100, u64::from),
            trim_end: rules.trim_end.map_or(0x100, u64::from),
            ..NativeRules::default()
        };
        for (slot, &(old, new)) in native.replace.iter_mut().zip(&rules.replace) {
            let folded = if rules.nocase { old.to_ascii_lowercase() } else { old };
            *slot = [u64::from(old) * Self::ONES, u64::from(folded ^ new) * Self::ONES];
        }
        native
    }
}

/// An interner and the view of it native code reads, shared by a pool and the functions its
/// `interner()` binds.
#[repr(C)]
struct Shared {
    view: View,
    interner: RefCell<Interner>,
}

impl Shared {
    fn new(interner: Interner) -> Shared {
        let fold = match interner.policy {
            Policy::Exact => 0,
            Policy::AsciiNoCase => 1,
            Policy::Rules => 2,
        };
        let rules = interner.rules().map(NativeRules::of).unwrap_or_default();
        let view = View {
            slots: Cell::new(0),
            mask: Cell::new(0),
            entries: Cell::new(0),
            text: Cell::new(0),
            fold: Cell::new(fold),
            rules,
        };
        let shared = Shared { view, interner: RefCell::new(interner) };
        shared.refresh();
        shared
    }

    /// Points the view at the interner's arrays as they are now.
    fn refresh(&self) {
        let interner = self.interner.borrow();
        self.view.slots.set(interner.slots.as_ptr() as u64);
        self.view.mask.set(interner.slots.len() as u64 - 1);
        self.view.entries.set(interner.entries.as_ptr() as u64);
        self.view.text.set(interner.text.as_ptr() as u64);
    }

    fn intern(&self, bytes: &[u8]) -> Result<u32> {
        let mut interner = self.interner.borrow_mut();
        let before = interner.len();
        let token = interner.intern(bytes)?;
        if interner.len() != before {
            drop(interner);
            self.refresh();
        }
        Ok(token)
    }
}

/// Words of scratch in a pool's payload for the native lowering's loop state.
const SCRATCH_WORDS: usize = 13;

/// `dream.intern.Pool`, from `intern.new(policy?)`. Tagged, and laid out for native code: the
/// shared view's address first, then the lowering's scratch.
#[repr(C)]
pub struct Pool {
    shared_address: *const Shared,
    scratch: [Cell<u64>; SCRATCH_WORDS],
    shared: Rc<Shared>,
}

impl Pool {
    fn new(interner: Interner) -> Pool {
        let shared = Rc::new(Shared::new(interner));
        Pool { shared_address: Rc::as_ptr(&shared), scratch: Default::default(), shared }
    }

    fn interner(&self) -> std::cell::Ref<'_, Interner> {
        self.shared.interner.borrow()
    }
}

// SAFETY: the payload holds no Lua references.
unsafe impl crate::userdata::Userdata for Pool {
    const NAME: &'static str = "dream.intern.Pool";
}

/// Byte offsets native code reads at, pinned here at compile time: in the pool's payload, in the
/// shared view, and in an entry.
pub(crate) mod layout {
    use super::{Entry, NativeRules, Pool, Shared, View};

    pub const POOL_SHARED: i32 = 0;
    pub const POOL_SCRATCH: i32 = 8;
    pub const VIEW_SLOTS: i64 = 0;
    pub const VIEW_MASK: i64 = 8;
    pub const VIEW_ENTRIES: i64 = 16;
    pub const VIEW_TEXT: i64 = 24;
    pub const VIEW_FOLD: i64 = 32;
    pub const VIEW_RULES: i64 = 40;
    pub const RULES_FOLD_MASK: i64 = VIEW_RULES;
    pub const RULES_REPLACE: i64 = VIEW_RULES + 8;
    pub const RULES_COLLAPSE: i64 = VIEW_RULES + 40;
    pub const RULES_COLLAPSE_MASK: i64 = VIEW_RULES + 48;
    pub const RULES_TRIM_START: i64 = VIEW_RULES + 56;
    pub const RULES_TRIM_END: i64 = VIEW_RULES + 64;

    const _: () = {
        assert!(std::mem::offset_of!(Pool, shared_address) == POOL_SHARED as usize);
        assert!(std::mem::offset_of!(Pool, scratch) == POOL_SCRATCH as usize);
        assert!(std::mem::offset_of!(Shared, view) == 0);
        assert!(std::mem::offset_of!(View, slots) == VIEW_SLOTS as usize);
        assert!(std::mem::offset_of!(View, mask) == VIEW_MASK as usize);
        assert!(std::mem::offset_of!(View, entries) == VIEW_ENTRIES as usize);
        assert!(std::mem::offset_of!(View, text) == VIEW_TEXT as usize);
        assert!(std::mem::offset_of!(View, fold) == VIEW_FOLD as usize);
        assert!(std::mem::offset_of!(View, rules) == VIEW_RULES as usize);
        assert!(std::mem::offset_of!(NativeRules, fold_mask) == (RULES_FOLD_MASK - VIEW_RULES) as usize);
        assert!(std::mem::offset_of!(NativeRules, replace) == (RULES_REPLACE - VIEW_RULES) as usize);
        assert!(std::mem::offset_of!(NativeRules, collapse) == (RULES_COLLAPSE - VIEW_RULES) as usize);
        assert!(std::mem::offset_of!(NativeRules, collapse_mask) == (RULES_COLLAPSE_MASK - VIEW_RULES) as usize);
        assert!(std::mem::offset_of!(NativeRules, trim_start) == (RULES_TRIM_START - VIEW_RULES) as usize);
        assert!(std::mem::offset_of!(NativeRules, trim_end) == (RULES_TRIM_END - VIEW_RULES) as usize);
        assert!(std::mem::size_of::<Entry>() == 8);
        assert!(std::mem::offset_of!(Entry, start) == 0);
        assert!(std::mem::offset_of!(Entry, len) == 4);
        assert!(std::mem::size_of::<std::cell::Cell<u64>>() == 8);
        assert!(cfg!(target_endian = "little"));
    };
}

/// The bytes `offset..offset + length` of `source`, the whole of it when both are absent.
#[inline(always)]
fn span<'a>(
    what: &str,
    source: &'a BytesView<'a>,
    offset: Option<Exact<i64>>,
    length: Option<Exact<i64>>,
) -> Result<&'a [u8]> {
    // SAFETY: interning reads the input and calls nothing in Lua; the pool's own storage is
    // never a Luau buffer.
    let bytes = unsafe { source.bytes_unchecked() };
    let start = match offset {
        None => 0,
        Some(offset) => {
            usize::try_from(offset.0).map_err(|_| Error::runtime(format!("{what}: negative offset {}", offset.0)))?
        }
    };
    let len = match length {
        None => bytes.len().saturating_sub(start),
        Some(length) => {
            usize::try_from(length.0).map_err(|_| Error::runtime(format!("{what}: negative length {}", length.0)))?
        }
    };
    match start.checked_add(len) {
        Some(end) if end <= bytes.len() => Ok(&bytes[start..end]),
        _ => {
            Err(Error::runtime(format!("{what}: {len} bytes at offset {start} past the end (length {})", bytes.len())))
        }
    }
}

const RULES_TYPE: &str = "{ nocase: boolean?, replace: { [string]: string }?, collapse: string?, \
    trimStart: string?, trimEnd: string? }";

/// The rules a script's table names.
fn read_rules(call: &crate::bind::Call<'_>, options: ValueView<'_>) -> Result<Rules> {
    const WHAT: &str = "intern.new";
    let mut rules = Rules::default();
    Options::read(call, options, WHAT, |o| {
        rules.nocase = o.or("nocase", false)?;
        rules.collapse = o.optional_bytes("collapse", |text| one_byte(WHAT, "collapse", text))?.flatten();
        rules.trim_start = o.optional_bytes("trimStart", |text| one_byte(WHAT, "trimStart", text))?.flatten();
        rules.trim_end = o.optional_bytes("trimEnd", |text| one_byte(WHAT, "trimEnd", text))?.flatten();
        o.optional_table("replace", |frame, table| {
            table.for_each(frame, |_, key, value| {
                let old =
                    key.read::<&[u8]>().ok().and_then(|text| one_byte(WHAT, "a replaced byte", text).ok().flatten());
                let new =
                    value.read::<&[u8]>().ok().and_then(|text| one_byte(WHAT, "a replacement", text).ok().flatten());
                match (old, new) {
                    (Some(old), Some(new)) => {
                        rules.replace.push((old, new));
                        Ok(())
                    }
                    _ => Err(Error::runtime(format!("{WHAT}: replace maps one-byte strings to one-byte strings"))),
                }
            })
        })?;
        Ok(())
    })?;
    // Table order is no order: sorted, the same rules make the same pool.
    rules.replace.sort_unstable();
    Ok(rules)
}

/// `intern.new(policy?)`.
fn new_pool(call: &crate::bind::Call<'_>, policy: Option<ValueView<'_>>) -> Result<Owned<Pool>> {
    let interner = match policy.filter(|view| !view.is_nil()) {
        None => Interner::new(Policy::Exact),
        Some(view) if view.is_table() => {
            let rules = read_rules(call, view)?;
            Interner::with_rules(rules).map_err(|error| Error::runtime(format!("intern.new: {error}")))?
        }
        Some(view) => {
            let name = view.read::<&str>()?;
            match Policy::parse(name) {
                Some(Policy::Rules) | None => {
                    return Err(Error::runtime(format!(
                        "intern.new: unknown policy '{name}' (exact, ascii-nocase, or a rules table)"
                    )));
                }
                Some(policy) => Interner::new(policy),
            }
        }
    };
    Ok(Owned(Pool::new(interner)))
}

/// The `dream.intern` extension.
#[derive(Clone, Copy, Debug, Default)]
pub struct InternExtension;

impl Extension for InternExtension {
    fn id(&self) -> &'static str {
        EXTENSION_ID
    }

    fn describe(&self, d: &mut ExtensionDescriptor) -> Result<()> {
        d.type_alias("dream_intern_Rules", RULES_TYPE);
        let mut pool = d.userdata::<Pool>("dream.intern.Pool");
        pool.tag(TagPolicy::Required)
            .compiler_type(CompilerTypePolicy::Required)
            .doc("Byte sequences to dense number identities under one policy.");
        pool.method(
            "intern",
            |pool: &Pool,
             source: BytesView<'_>,
             offset: Option<Exact<i64>>,
             length: Option<Exact<i64>>|
             -> Result<f64> {
                #[cfg(feature = "jit")]
                lowering::count_binder_call();
                let bytes = span("Pool:intern", &source, offset, length)?;
                pool.shared.intern(bytes).map(f64::from)
            },
        )
        .signature("(self, source: string | buffer, offset: number?, length: number?): number")
        .doc("The identity of length bytes of source from offset (all of it by default), added on first sight.");
        pool.method(
            "find",
            |pool: &Pool,
             source: BytesView<'_>,
             offset: Option<Exact<i64>>,
             length: Option<Exact<i64>>|
             -> Result<Option<f64>> {
                #[cfg(feature = "jit")]
                lowering::count_binder_call();
                let bytes = span("Pool:find", &source, offset, length)?;
                Ok(pool.interner().find(bytes).map(f64::from))
            },
        )
        .signature("(self, source: string | buffer, offset: number?, length: number?): number?")
        .doc("The identity the bytes already have, or nil; never adds one.");
        pool.method("resolve", |pool: &Pool, token: Exact<i64>| -> Result<Vec<u8>> {
            pool.interner()
                .resolve(token.0)
                .map(<[u8]>::to_vec)
                .ok_or_else(|| Error::runtime(format!("Pool:resolve: {} is not an identity of this pool", token.0)))
        })
        .signature("(self, token: number): string")
        .doc("The first spelling the pool saw for token; under rules, its normal form.");
        pool.method("interner", |pool: &Pool, call: &crate::bind::Call<'_>| -> Result<crate::value::Function> {
            let shared = Rc::clone(&pool.shared);
            crate::bind::function(
                call.stack(),
                &["dream"],
                "dream.intern.Pool.interner",
                move |source: BytesView<'_>, offset: Option<Exact<i64>>, length: Option<Exact<i64>>| -> Result<f64> {
                    let bytes = span("interner", &source, offset, length)?;
                    shared.intern(bytes).map(f64::from)
                },
            )
        })
        .signature("(self): (source: string | buffer, offset: number?, length: number?) -> number")
        .doc("Pool:intern as a plain function bound to this pool: no method lookup on each call.");
        pool.method("count", |pool: &Pool| pool.interner().len() as f64)
            .signature("(self): number")
            .doc("How many identities the pool holds; also the last token it handed out.");
        pool.method("memory", |pool: &Pool| pool.interner().memory() as f64)
            .signature("(self): number")
            .doc("Bytes of native memory the pool holds.");
        pool.method("policy", |pool: &Pool| pool.interner().policy().name()).signature("(self): string");

        #[cfg(feature = "jit")]
        d.native_hooks(lowering::InternLowering);

        d.module(MODULE)
            .doc("Textual identity as numbers: pools that intern byte sequences exactly, ASCII case-insensitively, or by their normal form under byte rules.")
            .function("new", new_pool)
            .signature("(policy: (\"exact\" | \"ascii-nocase\" | dream_intern_Rules)?) -> dream_intern_Pool")
            .doc("An empty pool: exact by default, ASCII case-insensitive, or keys normalized by rules.");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn folding_lowers_ascii_letters_only() {
        for byte in 0..=255u8 {
            let word = u64::from_le_bytes([byte; 8]);
            assert_eq!(fold_word(word), u64::from_le_bytes([byte.to_ascii_lowercase(); 8]), "byte {byte:#x}");
        }
    }

    #[test]
    fn word_reads_hashes_and_equality_agree_with_bytewise_references() {
        let mut seed = 0x9e37_79b9_7f4a_7c15u64;
        let mut next = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        for len in 0..=40 {
            for _ in 0..200 {
                let a: Vec<u8> = (0..len).map(|_| b"aAzZ@[`{0_\xc3"[next() as usize % 11]).collect();
                let b: Vec<u8> =
                    a.iter().map(|byte| if next() & 1 == 0 { byte.to_ascii_uppercase() } else { *byte }).collect();
                let mut flipped = b.clone();
                if len > 0 {
                    flipped[next() as usize % len] ^= 0x01;
                }
                assert!(equal::<false>(&a, &a) && equal::<true>(&a, &b), "{a:?} {b:?}");
                assert_eq!(hash::<true>(&a), hash::<true>(&b));
                assert_eq!(equal::<false>(&a, &b), a == b);
                assert_eq!(equal::<true>(&a, &flipped), a.eq_ignore_ascii_case(&flipped), "{a:?} {flipped:?}");
                if len < 8 {
                    let mut padded = [0u8; 8];
                    padded[..len].copy_from_slice(&a);
                    assert_eq!(load_short(&a), u64::from_le_bytes(padded));
                }
            }
        }
    }

    #[test]
    fn equivalent_spellings_share_a_token_and_keep_the_first() {
        let mut pool = Interner::new(Policy::AsciiNoCase);
        let first = pool.intern(b"cAiUs CoSaDeS").unwrap();
        for spelling in [&b"Caius Cosades"[..], b"caius cosades", b"CAIUS COSADES"] {
            assert_eq!(pool.intern(spelling).unwrap(), first);
        }
        assert_eq!(pool.resolve(i64::from(first)), Some(&b"cAiUs CoSaDeS"[..]));
        assert_ne!(pool.intern(b"caius cosade").unwrap(), first);
        assert_ne!(pool.intern("caïus cosades".as_bytes()).unwrap(), first);
        assert_eq!(pool.len(), 3);

        let mut exact = Interner::new(Policy::Exact);
        assert_ne!(exact.intern(b"Fargoth").unwrap(), exact.intern(b"fargoth").unwrap());
    }

    #[test]
    fn tokens_are_dense_and_survive_growth() {
        let mut pool = Interner::new(Policy::AsciiNoCase);
        let names: Vec<String> = (0..100_000).map(|i| format!("Record_{i:x}_{}", i % 7)).collect();
        for (i, name) in names.iter().enumerate() {
            assert_eq!(pool.intern(name.as_bytes()).unwrap() as usize, i + 1);
        }
        for (i, name) in names.iter().enumerate() {
            assert_eq!(pool.find(name.to_ascii_uppercase().as_bytes()), Some(i as u32 + 1));
        }
        assert_eq!(pool.find(b"absent"), None);
        assert_eq!(pool.resolve(0), None);
        assert_eq!(pool.resolve(100_001), None);
        assert_eq!(pool.intern(b"").unwrap(), 100_001);
        assert_eq!(pool.find(b""), Some(100_001));
    }
}
