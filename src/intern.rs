//! Textual identity as integers: the `dream.intern` extension, module `@dream/intern`.
//!
//! A pool maps byte sequences to Luau integers under one equivalence policy. Equivalent inputs
//! get the same integer, different ones different integers, and from then on identity is
//! integer equality and integer table keys:
//!
//! ```lua
//! local intern = require('@dream/intern')
//! local ids = intern.new('ascii-nocase')
//! local id = ids:intern(record, offset, length)   -- a buffer span: no Luau string is made
//! assert(id == ids:intern('Caius Cosades') and id == ids:intern('CAIUS COSADES'))
//! print(ids:resolve(id))                            -- the first spelling the pool saw
//! ```
//!
//! # Policies
//!
//! - `exact`: byte-for-byte.
//! - `ascii-nocase`: `A`..`Z` equal `a`..`z`; every other byte, UTF-8 included, compares
//!   exactly. SQLite's `NOCASE`. Folding happens inside the hash and the comparison, a word at a
//!   time; a folded copy of the input is never made.
//!
//! Nothing else belongs here: path separators, Unicode case folding and the like are a
//! domain's rules, and a domain that needs them folds its keys before interning them.
//!
//! # Tokens and lifetime
//!
//! A token is the 1-based position of its identity in its pool: `1, 2, 3, ...` in first-seen
//! order, dense, below 2^32. Tokens are pool-relative: two pools hand out the same integers, and
//! a token means something only to the pool that made it. Pools only grow (no removal, no
//! reuse), so a token never changes meaning while its pool lives, and needs no generation.
//! Dropping the pool frees everything at once; tokens kept after that are plain integers.
//! Dense tokens fit a `u32` column (`buffer.writeu32`) and index arrays directly.
//!
//! # Storage
//!
//! The first spelling of each identity is copied once into the pool's text arena; duplicates
//! copy nothing and allocate nothing, native or VM. The index is open addressing with the hash
//! in each slot: 8 bytes per slot, 8 per identity, plus the text.

use std::cell::RefCell;

use crate::convert::{BytesView, Exact, Integer};
use crate::error::{Error, Result};
use crate::extension::{Extension, ExtensionDescriptor, TagPolicy};
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
}

impl Policy {
    /// The policy named `name`: `exact` or `ascii-nocase`.
    pub fn parse(name: &str) -> Option<Policy> {
        match name {
            "exact" => Some(Policy::Exact),
            "ascii-nocase" => Some(Policy::AsciiNoCase),
            _ => None,
        }
    }

    /// The policy's name.
    pub const fn name(self) -> &'static str {
        match self {
            Policy::Exact => "exact",
            Policy::AsciiNoCase => "ascii-nocase",
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

#[inline(always)]
fn word_at(bytes: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(bytes[at..at + 8].try_into().unwrap())
}

/// The trailing `bytes.len() % 8` bytes, zero-padded.
#[inline(always)]
fn tail_word(bytes: &[u8]) -> u64 {
    let mut tail = [0u8; 8];
    let rest = bytes.len() & !7;
    tail[..bytes.len() - rest].copy_from_slice(&bytes[rest..]);
    u64::from_le_bytes(tail)
}

const K: u64 = 0x517c_c1b7_2722_0a95;

#[inline(always)]
fn hash<const FOLD: bool>(bytes: &[u8]) -> u32 {
    let mut h = bytes.len() as u64;
    let words = bytes.len() / 8;
    for i in 0..words {
        let word = word_at(bytes, i * 8);
        let word = if FOLD { fold_word(word) } else { word };
        h = (h.rotate_left(5) ^ word).wrapping_mul(K);
    }
    if !bytes.len().is_multiple_of(8) {
        let word = tail_word(bytes);
        let word = if FOLD { fold_word(word) } else { word };
        h = (h.rotate_left(5) ^ word).wrapping_mul(K);
    }
    h ^= h >> 29;
    h = h.wrapping_mul(0xbf58_476d_1ce4_e5b9);
    (h ^ (h >> 32)) as u32
}

#[inline(always)]
fn equal_folded(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let words = a.len() / 8;
    for i in 0..words {
        if fold_word(word_at(a, i * 8)) != fold_word(word_at(b, i * 8)) {
            return false;
        }
    }
    a.len().is_multiple_of(8) || fold_word(tail_word(a)) == fold_word(tail_word(b))
}

/// One identity's first spelling in the arena.
#[derive(Clone, Copy)]
struct Entry {
    start: u32,
    len: u32,
}

/// The pool itself, without Luau: usable from Rust, and what `dream.intern.Pool` wraps.
pub struct Interner {
    policy: Policy,
    text: Vec<u8>,
    entries: Vec<Entry>,
    /// 0 is empty; otherwise the hash in the high half and the token in the low half.
    slots: Vec<u64>,
}

impl Interner {
    /// An empty pool.
    pub fn new(policy: Policy) -> Interner {
        Interner { policy, text: Vec::new(), entries: Vec::new(), slots: Vec::new() }
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
        self.text.capacity() + self.entries.capacity() * size_of::<Entry>() + self.slots.capacity() * size_of::<u64>()
    }

    #[inline(always)]
    fn hash_of(&self, bytes: &[u8]) -> u32 {
        match self.policy {
            Policy::Exact => hash::<false>(bytes),
            Policy::AsciiNoCase => hash::<true>(bytes),
        }
    }

    #[inline(always)]
    fn spelling(&self, token: u32) -> &[u8] {
        let entry = self.entries[token as usize - 1];
        &self.text[entry.start as usize..(entry.start + entry.len) as usize]
    }

    #[inline(always)]
    fn same(&self, token: u32, bytes: &[u8]) -> bool {
        let stored = self.spelling(token);
        match self.policy {
            Policy::Exact => stored == bytes,
            Policy::AsciiNoCase => equal_folded(stored, bytes),
        }
    }

    /// The token for `bytes`, or the empty slot it would take.
    #[inline(always)]
    fn probe(&self, bytes: &[u8], hash: u32) -> std::result::Result<u32, usize> {
        let mask = self.slots.len() - 1;
        let mut at = hash as usize & mask;
        loop {
            let slot = self.slots[at];
            if slot == 0 {
                return Err(at);
            }
            if (slot >> 32) as u32 == hash && self.same(slot as u32, bytes) {
                return Ok(slot as u32);
            }
            at = (at + 1) & mask;
        }
    }

    /// The token `bytes` already has, without adding it.
    pub fn find(&self, bytes: &[u8]) -> Option<u32> {
        if self.slots.is_empty() {
            return None;
        }
        self.probe(bytes, self.hash_of(bytes)).ok()
    }

    /// The token for `bytes`, adding it on first sight.
    pub fn intern(&mut self, bytes: &[u8]) -> Result<u32> {
        if (self.entries.len() + 1) * 4 > self.slots.len() * 3 {
            self.grow()?;
        }
        let hash = self.hash_of(bytes);
        match self.probe(bytes, hash) {
            Ok(token) => Ok(token),
            Err(at) => {
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
        }
    }

    /// The first spelling of `token`, if this pool made it.
    pub fn resolve(&self, token: i64) -> Option<&[u8]> {
        let token = u32::try_from(token).ok().filter(|t| *t != 0 && (*t as usize) <= self.entries.len())?;
        Some(self.spelling(token))
    }

    fn grow(&mut self) -> Result<()> {
        if self.entries.len() >= u32::MAX as usize - 1 {
            return Err(Error::runtime("intern: the pool holds 2^32 - 1 identities"));
        }
        let capacity = (self.slots.len() * 2).max(16);
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

/// `dream.intern.Pool`, from `intern.new(policy?)`.
pub struct Pool(RefCell<Interner>);

// SAFETY: the payload holds no Lua references.
unsafe impl crate::userdata::Userdata for Pool {
    const NAME: &'static str = "dream.intern.Pool";
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

/// The `dream.intern` extension.
#[derive(Clone, Copy, Debug, Default)]
pub struct InternExtension;

impl Extension for InternExtension {
    fn id(&self) -> &'static str {
        EXTENSION_ID
    }

    fn describe(&self, d: &mut ExtensionDescriptor) -> Result<()> {
        let mut pool = d.userdata::<Pool>("dream.intern.Pool");
        pool.tag(TagPolicy::Preferred).doc("Byte sequences to integer identities under one policy.");
        pool.method(
            "intern",
            |pool: &Pool,
             source: BytesView<'_>,
             offset: Option<Exact<i64>>,
             length: Option<Exact<i64>>|
             -> Result<Integer> {
                let bytes = span("Pool:intern", &source, offset, length)?;
                pool.0.borrow_mut().intern(bytes).map(|token| Integer(i64::from(token)))
            },
        )
        .signature("(self, source: string | buffer, offset: number?, length: number?): integer")
        .doc("The identity of length bytes of source from offset (all of it by default), added on first sight.");
        pool.method(
            "find",
            |pool: &Pool,
             source: BytesView<'_>,
             offset: Option<Exact<i64>>,
             length: Option<Exact<i64>>|
             -> Result<Option<Integer>> {
                let bytes = span("Pool:find", &source, offset, length)?;
                Ok(pool.0.borrow().find(bytes).map(|token| Integer(i64::from(token))))
            },
        )
        .signature("(self, source: string | buffer, offset: number?, length: number?): integer?")
        .doc("The identity the bytes already have, or nil; never adds one.");
        pool.method("resolve", |pool: &Pool, token: Integer| -> Result<Vec<u8>> {
            pool.0
                .borrow()
                .resolve(token.0)
                .map(<[u8]>::to_vec)
                .ok_or_else(|| Error::runtime(format!("Pool:resolve: {} is not an identity of this pool", token.0)))
        })
        .signature("(self, token: integer): string")
        .doc("The first spelling the pool saw for token.");
        pool.method("count", |pool: &Pool| pool.0.borrow().len() as f64)
            .signature("(self): number")
            .doc("How many identities the pool holds; also the last token it handed out.");
        pool.method("memory", |pool: &Pool| pool.0.borrow().memory() as f64)
            .signature("(self): number")
            .doc("Bytes of native memory the pool holds.");
        pool.method("policy", |pool: &Pool| pool.0.borrow().policy().name()).signature("(self): string");

        d.module(MODULE)
            .doc("Textual identity as integers: pools that intern byte sequences under exact or ASCII case-insensitive equality.")
            .function("new", |policy: Option<&str>| -> Result<Owned<Pool>> {
                let policy = match policy {
                    None => Policy::Exact,
                    Some(name) => Policy::parse(name).ok_or_else(|| {
                        Error::runtime(format!("intern.new: unknown policy '{name}' (exact or ascii-nocase)"))
                    })?,
                };
                Ok(Owned(Pool(RefCell::new(Interner::new(policy)))))
            })
            .signature("(policy: (\"exact\" | \"ascii-nocase\")?) -> dream_intern_Pool")
            .doc("An empty pool; exact by default.");
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
