//! Host names as `@dream/dns` resolves them and `@dream/tls` verifies them: one mapping, so the
//! name a script resolved is the name its TLS session checks.
//!
//! A name maps to ASCII as URLs map host names (UTS #46 nontransitional processing, WHATWG's
//! forbidden host code points refused, so `_` is allowed), then is checked: at most 253
//! characters, labels of 1 to 63 letters, digits, `-` or `_`, one optional trailing dot. An
//! IPv4 or IPv6 literal, bracketed or not, is an address and maps to its canonical spelling.

use std::net::IpAddr;

/// A host name: what the caller wrote, its ASCII form, and the address when it is a literal.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Name {
    /// As given.
    pub original: String,
    /// IDNA A-labels, lower case; an address literal's canonical spelling.
    pub ascii: String,
    /// The address, for a literal.
    pub literal: Option<IpAddr>,
}

impl Name {
    /// The ASCII form without its trailing dot, as certificates spell names.
    pub fn bare(&self) -> &str {
        self.ascii.strip_suffix('.').unwrap_or(&self.ascii)
    }
}

/// Checks and maps a host name; the message says what is wrong.
pub fn normalize(host: &str) -> Result<Name, String> {
    if host.is_empty() {
        return Err("the name is empty".to_owned());
    }
    if host.len() > 1024 {
        return Err(format!("the name is {} bytes, more than 1024", host.len()));
    }
    if host.contains('\0') {
        return Err("the name contains NUL".to_owned());
    }
    let unbracketed = host.strip_prefix('[').and_then(|inner| inner.strip_suffix(']'));
    if let Some(inner) = unbracketed {
        let ip: std::net::Ipv6Addr = inner.parse().map_err(|_| format!("'{host}' is not an IPv6 literal"))?;
        return Ok(Name { original: host.to_owned(), ascii: ip.to_string(), literal: Some(IpAddr::V6(ip)) });
    }
    if let Ok(ip) = host.parse::<IpAddr>() {
        return Ok(Name { original: host.to_owned(), ascii: ip.to_string(), literal: Some(ip) });
    }
    let ascii = idna::domain_to_ascii_cow(host.as_bytes(), idna::AsciiDenyList::URL)
        .map_err(|_| format!("'{host}' is not a valid host name (IDNA mapping failed)"))?
        .into_owned();
    let bare = ascii.strip_suffix('.').unwrap_or(&ascii);
    if bare.is_empty() {
        return Err(format!("'{host}' has no labels"));
    }
    if bare.len() > 253 {
        return Err(format!("'{host}' is {} characters as ASCII, more than 253", bare.len()));
    }
    for label in bare.split('.') {
        if label.is_empty() || label.len() > 63 {
            return Err(format!("'{host}' has a label of {} characters (1 to 63)", label.len()));
        }
        if let Some(bad) = label.chars().find(|c| !(c.is_ascii_alphanumeric() || *c == '-' || *c == '_')) {
            return Err(format!("'{host}' has a label with '{bad}' (letters, digits, '-' and '_' only)"));
        }
    }
    Ok(Name { original: host.to_owned(), ascii, literal: None })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_map_to_ascii_and_bad_names_say_why() {
        let name = normalize("Bücher.Example.").unwrap();
        assert_eq!(
            (name.original.as_str(), name.ascii.as_str(), name.literal),
            ("Bücher.Example.", "xn--bcher-kva.example.", None)
        );
        assert_eq!(name.bare(), "xn--bcher-kva.example");
        assert_eq!(normalize("_srv.Internal-Host").unwrap().ascii, "_srv.internal-host");
        assert_eq!(normalize("127.0.0.1").unwrap().literal, Some("127.0.0.1".parse().unwrap()));
        assert_eq!(normalize("[::1]").unwrap().literal, Some("::1".parse().unwrap()));
        assert_eq!(normalize("::1").unwrap().ascii, "::1");
        for (bad, why) in [
            ("", "empty"),
            ("a\0b", "NUL"),
            ("[127.0.0.1]", "IPv6 literal"),
            ("exa mple.com", "IDNA"),
            ("a..b", "label of 0"),
            (".", "no labels"),
            ("example.com:443", "IDNA"),
            ("a!b.com", "'!'"),
        ] {
            let error = normalize(bad).unwrap_err();
            assert!(error.contains(why), "{bad:?}: {error}");
        }
        let long_label = format!("{}.com", "a".repeat(64));
        assert!(normalize(&long_label).unwrap_err().contains("64 characters"));
        let long_name = vec!["abcdefghi"; 26].join(".");
        assert!(normalize(&long_name).unwrap_err().contains("more than 253"));
        assert!(normalize(&"a".repeat(1025)).unwrap_err().contains("1024"));
    }
}
