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
