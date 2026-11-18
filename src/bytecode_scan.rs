//! A walk over a bytecode chunk's constants before `luau_load` sees it.
//!
//! Luau 0.747 removed the integer type but kept bytecode version 14, which Luau 0.746 also
//! writes, so a chunk cached by an l3i built on 0.746 passes Luau's version check. When such a
//! chunk holds a 64-bit integer constant, 0.747's loader asserts in a debug build and in a
//! release build skips the kind byte without its payload and misreads everything after it. This
//! walk finds that constant, and any constant kind this build does not know, and refuses the
//! chunk with a message naming it.
//!
//! The walk mirrors `loadsafe` and `loadProto` in `luau/VM/src/lvmload.cpp` and checks every
//! read against the chunk's length. It decides nothing else: a version or type version Luau
//! refuses, and an error chunk (version byte 0), pass through for `luau_load` to report.
//! `csrc/extra.cpp` pins the numbers below to `Luau/Bytecode.h`.

use crate::error::Error;

const VERSION_MIN: u8 = 3;
const VERSION_MAX: u8 = 14;
const VERSION_CLASSES: u8 = 100;
const TYPE_VERSION_MIN: u8 = 1;
const TYPE_VERSION_MAX: u8 = 3;

const CONSTANT_NIL: u8 = 0;
const CONSTANT_BOOLEAN: u8 = 1;
const CONSTANT_NUMBER: u8 = 2;
const CONSTANT_STRING: u8 = 3;
const CONSTANT_IMPORT: u8 = 4;
const CONSTANT_TABLE: u8 = 5;
const CONSTANT_CLOSURE: u8 = 6;
const CONSTANT_VECTOR: u8 = 7;
const CONSTANT_TABLE_WITH_CONSTANTS: u8 = 8;
const CONSTANT_INTEGER_DEPRECATED: u8 = 9;
const CONSTANT_CLASS_SHAPE: u8 = 10;
const CONSTANT_VECTORD: u8 = 11;

const FEEDBACK_CALL_TARGET: u8 = 0;
const FEEDBACK_CONSTRUCT: u8 = 1;
const PROTO_INLINABLE: u8 = 1 << 3;

/// Why a chunk was refused; the caller prefixes the chunk name.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Refusal {
    IntegerConstant,
    UnknownConstant(u8),
    UnknownFeedbackSlot(u8),
    Truncated,
}

impl Refusal {
    /// The message names the chunk as Luau's own load errors do, without an `=` or `@` prefix.
    pub(crate) fn into_error(self, chunk_name: &str) -> Error {
        let chunk_name = chunk_name.strip_prefix(['=', '@']).unwrap_or(chunk_name);
        Error::runtime(match self {
            Refusal::IntegerConstant => format!(
                "{chunk_name}: bytecode holds a 64-bit integer constant, which Luau 0.747 removed; \
                 it was compiled by an older build and must be compiled again"
            ),
            Refusal::UnknownConstant(kind) => {
                format!("{chunk_name}: bytecode holds constant kind {kind}, which this Luau build does not know")
            }
            Refusal::UnknownFeedbackSlot(kind) => {
                format!("{chunk_name}: bytecode holds feedback slot kind {kind}, which this Luau build does not know")
            }
            Refusal::Truncated => format!("{chunk_name}: bytecode ends inside a function"),
        })
    }
}

struct Reader<'a> {
    data: &'a [u8],
    at: usize,
}

impl Reader<'_> {
    fn byte(&mut self) -> std::result::Result<u8, Refusal> {
        let byte = *self.data.get(self.at).ok_or(Refusal::Truncated)?;
        self.at += 1;
        Ok(byte)
    }

    fn skip(&mut self, count: u64) -> std::result::Result<(), Refusal> {
        let count = usize::try_from(count).map_err(|_| Refusal::Truncated)?;
        let end = self.at.checked_add(count).filter(|&end| end <= self.data.len()).ok_or(Refusal::Truncated)?;
        self.at = end;
        Ok(())
    }

    /// `readVarInt` and `readVarInt64`: seven bits a byte, low first. Bits past 64 are dropped,
    /// as Luau's shifts drop them; only the length matters to the walk.
    fn varint(&mut self) -> std::result::Result<u64, Refusal> {
        let mut value = 0u64;
        let mut shift = 0u32;
        loop {
            let byte = self.byte()?;
            if shift < 64 {
                value |= u64::from(byte & 127) << shift;
            }
            shift += 7;
            if byte & 128 == 0 {
                return Ok(value);
            }
        }
    }

    /// `readVarInt` as Luau keeps it: an unsigned 32-bit count.
    fn count(&mut self) -> std::result::Result<u64, Refusal> {
        Ok(self.varint()? & u64::from(u32::MAX))
    }

    fn repeat(
        &mut self,
        count: u64,
        mut each: impl FnMut(&mut Self) -> std::result::Result<(), Refusal>,
    ) -> std::result::Result<(), Refusal> {
        for _ in 0..count {
            each(self)?;
        }
        Ok(())
    }
}

/// Refuses a chunk Luau would misread or trap on. Passes everything `luau_load` itself rejects.
pub(crate) fn check(bytecode: &[u8]) -> std::result::Result<(), Refusal> {
    let mut r = Reader { data: bytecode, at: 0 };
    let Ok(version) = r.byte() else { return Ok(()) };
    if version == 0 || (!(VERSION_MIN..=VERSION_MAX).contains(&version) && version != VERSION_CLASSES) {
        return Ok(());
    }
    let mut types = 0;
    if version >= 4 {
        types = r.byte()?;
        if !(TYPE_VERSION_MIN..=TYPE_VERSION_MAX).contains(&types) {
            return Ok(());
        }
    }
    let strings = r.count()?;
    r.repeat(strings, |r| {
        let length = r.count()?;
        r.skip(length)
    })?;
    if types == 3 {
        while r.byte()? != 0 {
            r.count()?;
        }
    }
    let protos = r.count()?;
    r.repeat(protos, |r| proto(r, version))?;
    r.count().map(drop) // the main function's id
}

fn proto(r: &mut Reader<'_>, version: u8) -> std::result::Result<(), Refusal> {
    let sized = if version >= 12 {
        let size = r.count()?;
        let start = r.at;
        Some(start.checked_add(usize::try_from(size).map_err(|_| Refusal::Truncated)?).ok_or(Refusal::Truncated)?)
    } else {
        None
    };
    r.skip(4)?; // maxstacksize, numparams, nups, is_vararg
    let mut flags = 0;
    if version >= 4 {
        flags = r.byte()?;
        let size = r.count()?; // type information, in any of the three type versions
        r.skip(size)?;
    }
    let sizecode = r.count()?;
    r.skip(sizecode * 4)?;
    let constants = r.count()?;
    r.repeat(constants, constant)?;
    if let Some(end) = sized {
        // Luau skips to the recorded end, past fields newer than it.
        if end > r.data.len() {
            return Err(Refusal::Truncated);
        }
        r.at = end;
        return Ok(());
    }
    let children = r.count()?;
    r.repeat(children, |r| r.count().map(drop))?;
    r.count()?; // linedefined
    r.count()?; // debugname
    if r.byte()? != 0 {
        let gap = r.byte()?;
        let intervals = (sizecode.wrapping_sub(1) as u32 >> (gap & 31)) + 1;
        r.skip(sizecode)?;
        r.skip(u64::from(intervals) * 4)?;
    }
    if r.byte()? != 0 {
        let locals = r.count()?;
        r.repeat(locals, |r| {
            r.count()?;
            r.count()?;
            r.count()?;
            r.skip(1)
        })?;
        let upvalues = r.count()?;
        r.repeat(upvalues, |r| r.count().map(drop))?;
    }
    if version >= 11 {
        let slots = r.count()?;
        r.repeat(slots, |r| match r.byte()? {
            FEEDBACK_CALL_TARGET | FEEDBACK_CONSTRUCT => r.count().map(drop),
            kind => Err(Refusal::UnknownFeedbackSlot(kind)),
        })?;
    }
    if version >= 12 && flags & PROTO_INLINABLE != 0 {
        r.varint()?;
    }
    Ok(())
}

fn constant(r: &mut Reader<'_>) -> std::result::Result<(), Refusal> {
    match r.byte()? {
        CONSTANT_NIL => Ok(()),
        CONSTANT_BOOLEAN => r.skip(1),
        CONSTANT_NUMBER => r.skip(8),
        CONSTANT_STRING | CONSTANT_CLOSURE => r.count().map(drop),
        CONSTANT_IMPORT => r.skip(4),
        CONSTANT_TABLE => {
            let keys = r.count()?;
            r.repeat(keys, |r| r.count().map(drop))
        }
        CONSTANT_VECTOR => r.skip(16),
        CONSTANT_TABLE_WITH_CONSTANTS => {
            let keys = r.count()?;
            r.repeat(keys, |r| {
                r.count()?;
                r.skip(4)
            })
        }
        CONSTANT_INTEGER_DEPRECATED => Err(Refusal::IntegerConstant),
        CONSTANT_CLASS_SHAPE => {
            r.count()?; // class name
            let private_fields = u64::from(r.byte()?);
            let private_methods = u64::from(r.byte()?);
            let properties = r.count()?;
            let methods = r.count()?;
            // Public properties, then public methods: what `loadProto` reads by name.
            let named =
                (properties + methods).checked_sub(private_fields + private_methods).ok_or(Refusal::Truncated)?;
            r.repeat(named, |r| r.count().map(drop))
        }
        CONSTANT_VECTORD => r.skip(32),
        kind => Err(Refusal::UnknownConstant(kind)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn compile(source: &str) -> Vec<u8> {
        crate::source::compile(source, &crate::source::CompileOptions::default()).unwrap()
    }

    #[test]
    fn this_builds_chunks_pass() {
        for source in [
            "return 1",
            "local t = {a = 1, b = 'x', c = vector.create(1, 2, 3)} return function() return t, math.sqrt end",
            "local function f(...) local x: number = 1 return x, ... end return f",
        ] {
            assert_eq!(check(&compile(source)), Ok(()), "{source}");
        }
    }

    #[test]
    fn every_truncation_of_a_chunk_is_refused_or_left_to_luau() {
        let chunk = compile("local t = {a = 1, 2, 'x'} return function(y) return t.a + y end");
        for end in 0..chunk.len() {
            // Never panics, never reads past the end; a cut in the header passes to luau_load.
            let _ = check(&chunk[..end]);
        }
        assert_eq!(check(&chunk[..chunk.len() - 1]), Err(Refusal::Truncated));
    }

    #[test]
    fn error_chunks_and_foreign_versions_pass_to_luau() {
        assert_eq!(check(b"\0something failed"), Ok(()));
        assert_eq!(check(&[2]), Ok(()));
        assert_eq!(check(&[15]), Ok(()));
        assert_eq!(check(&[6, 9]), Ok(()));
        assert_eq!(check(&[]), Ok(()));
    }
}
