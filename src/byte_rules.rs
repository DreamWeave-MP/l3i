//! Byte normalization rules shared by `bytes.translate` and the intern pools' `rules` policy: a
//! 256-entry byte map, then runs of one byte collapsed to one, then one byte trimmed from the
//! start and from the end. Each step works on the bytes the one before produced.

use crate::error::{Error, Result};

/// One byte named by a one-byte string option.
pub(crate) fn one_byte(what: &str, name: &str, text: &[u8]) -> Result<Option<u8>> {
    match text {
        [byte] => Ok(Some(*byte)),
        other => Err(Error::runtime(format!("{what}: {name} must be one byte, got {} bytes", other.len()))),
    }
}

/// The rules table `bytes.translate` and `intern.new` both read, as Luau declares it.
#[cfg(any(feature = "bytes", feature = "intern"))]
pub(crate) const RULES_TYPE: &str = "{ nocase: boolean?, replace: { [string]: string }?, collapse: string?, \
    trimStart: string?, trimEnd: string? }";

/// A rules table as a script spells it: `nocase` lowers `A`..`Z`, `replace` maps single bytes to
/// single bytes (a replaced byte is not also lowered), then `collapse` makes runs of one byte one,
/// and `trimStart` and `trimEnd` remove one byte from the ends.
#[cfg(any(feature = "bytes", feature = "intern"))]
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct RuleOptions {
    pub(crate) nocase: bool,
    /// Sorted: table order is no order, so the same table is the same rules.
    pub(crate) replace: Vec<(u8, u8)>,
    pub(crate) collapse: Option<u8>,
    pub(crate) trim_start: Option<u8>,
    pub(crate) trim_end: Option<u8>,
}

#[cfg(any(feature = "bytes", feature = "intern"))]
impl RuleOptions {
    /// The rules `options` names, for the function `what`.
    pub(crate) fn read(
        scope: &impl crate::stack::Scope,
        what: &str,
        options: crate::stack::ValueView<'_>,
    ) -> Result<RuleOptions> {
        let mut rules = RuleOptions::default();
        crate::options::Options::read(scope, options, what, |o| {
            rules.nocase = o.or("nocase", false)?;
            rules.collapse = o.optional_bytes("collapse", |text| one_byte(what, "collapse", text))?.flatten();
            rules.trim_start = o.optional_bytes("trimStart", |text| one_byte(what, "trimStart", text))?.flatten();
            rules.trim_end = o.optional_bytes("trimEnd", |text| one_byte(what, "trimEnd", text))?.flatten();
            o.optional_table("replace", |frame, table| {
                table.for_each(frame, |_, key, value| {
                    let old = key
                        .read::<&[u8]>()
                        .ok()
                        .and_then(|text| one_byte(what, "a replaced byte", text).ok().flatten());
                    let new = value
                        .read::<&[u8]>()
                        .ok()
                        .and_then(|text| one_byte(what, "a replacement", text).ok().flatten());
                    match (old, new) {
                        (Some(old), Some(new)) => {
                            rules.replace.push((old, new));
                            Ok(())
                        }
                        _ => Err(Error::runtime(format!("{what}: replace maps one-byte strings to one-byte strings"))),
                    }
                })
            })?;
            Ok(())
        })?;
        rules.replace.sort_unstable();
        Ok(rules)
    }

    /// The byte each byte becomes.
    pub(crate) fn map(&self, byte: u8) -> u8 {
        match self.replace.iter().find(|(old, _)| *old == byte) {
            Some((_, new)) => *new,
            None if self.nocase => byte.to_ascii_lowercase(),
            None => byte,
        }
    }

    pub(crate) fn byte_rules(&self) -> ByteRules {
        ByteRules {
            map: std::array::from_fn(|byte| self.map(byte as u8)),
            collapse: self.collapse,
            trim_start: self.trim_start,
            trim_end: self.trim_end,
        }
    }
}

/// A normalization: map every byte, collapse runs of `collapse`, trim `trim_start` from the
/// front and `trim_end` from the back.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ByteRules {
    pub(crate) map: [u8; 256],
    pub(crate) collapse: Option<u8>,
    pub(crate) trim_start: Option<u8>,
    pub(crate) trim_end: Option<u8>,
}

impl ByteRules {
    /// Rules that change nothing.
    #[cfg(any(feature = "bytes", test))]
    pub(crate) fn identity() -> ByteRules {
        ByteRules { map: std::array::from_fn(|byte| byte as u8), collapse: None, trim_start: None, trim_end: None }
    }

    /// Whether `bytes` is already in its normal form: then normalizing it copies nothing.
    #[inline]
    pub(crate) fn is_normal(&self, bytes: &[u8]) -> bool {
        if bytes.iter().any(|&byte| self.map[usize::from(byte)] != byte) {
            return false;
        }
        if let Some(first) = bytes.first()
            && Some(*first) == self.trim_start
        {
            return false;
        }
        if let Some(last) = bytes.last()
            && Some(*last) == self.trim_end
        {
            return false;
        }
        match self.collapse {
            Some(run) => !bytes.windows(2).any(|pair| pair[0] == run && pair[1] == run),
            None => true,
        }
    }

    /// The normal form of `bytes`, appended to `out`.
    pub(crate) fn normalize_into(&self, bytes: &[u8], out: &mut Vec<u8>) {
        let start = out.len();
        out.reserve(bytes.len());
        let mut previous: Option<u8> = None;
        for &byte in bytes {
            let mapped = self.map[usize::from(byte)];
            if out.len() == start && Some(mapped) == self.trim_start {
                continue;
            }
            if self.collapse.is_some() && previous == self.collapse && Some(mapped) == self.collapse {
                continue;
            }
            out.push(mapped);
            previous = Some(mapped);
        }
        if let Some(trim) = self.trim_end {
            while out.len() > start && out.last() == Some(&trim) {
                out.pop();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::ByteRules;

    fn path_rules() -> ByteRules {
        let mut rules = ByteRules::identity();
        for byte in b'A'..=b'Z' {
            rules.map[usize::from(byte)] = byte.to_ascii_lowercase();
        }
        rules.map[usize::from(b'\\')] = b'/';
        rules.collapse = Some(b'/');
        rules.trim_start = Some(b'/');
        rules
    }

    fn normal(rules: &ByteRules, bytes: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        rules.normalize_into(bytes, &mut out);
        out
    }

    #[test]
    fn the_steps_run_in_order_and_agree_with_is_normal() {
        let rules = path_rules();
        for (input, expected) in [
            (&b"Meshes\\X\\Rock.NIF"[..], &b"meshes/x/rock.nif"[..]),
            (b"\\\\Textures//a.dds", b"textures/a.dds"),
            (b"//", b""),
            (b"", b""),
            (b"a/", b"a/"),
            (b"plain/key", b"plain/key"),
            (b"caf\xc3\xa9/X", b"caf\xc3\xa9/x"),
        ] {
            let out = normal(&rules, input);
            assert_eq!(out, expected, "{input:?}");
            assert_eq!(rules.is_normal(input), input == expected, "{input:?}");
            assert!(rules.is_normal(&out));
        }
        let mut trailing = path_rules();
        trailing.trim_end = Some(b'/');
        assert_eq!(normal(&trailing, b"a//b//"), b"a/b");
        assert_eq!(normal(&trailing, b"///"), b"");
    }
}
