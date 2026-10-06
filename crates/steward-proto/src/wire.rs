//! Paths on the wire. Linux file names are bytes, JSON strings are Unicode.
//! steward sends every path as a JSON string; a byte that isn't part of
//! valid UTF-8 travels as the lone surrogate U+DC80 + byte (`\udcae` for
//! 0xAE), Python's "surrogateescape" (PEP 383). Valid UTF-8 is unchanged.
//!
//! Rust strings can't hold lone surrogates, so inside steward such a byte
//! is the private-use character U+E000 followed by two hex digits, and a
//! real U+E000 is doubled. `out` and `incoming` translate between the two
//! forms at the socket; `path` and `to_path` convert to and from paths.

use std::os::unix::ffi::{OsStrExt as _, OsStringExt as _};
use std::path::{Path, PathBuf};

/// Starts an escaped byte (followed by two hex digits) or, doubled, stands
/// for itself.
pub const MARK: char = '\u{E000}';
const MARK_UTF8: [u8; 3] = [0xEE, 0x80, 0x80];

/// Bytes as a string, invalid UTF-8 escaped.
pub fn encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len());
    for chunk in bytes.utf8_chunks() {
        for c in chunk.valid().chars() {
            out.push(c);
            if c == MARK {
                out.push(MARK);
            }
        }
        for b in chunk.invalid() {
            out.push(MARK);
            out.push_str(&format!("{b:02x}"));
        }
    }
    out
}

/// The bytes an `encode`d string stands for. Tolerant: a stray marker is
/// kept as itself.
pub fn decode(s: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c != MARK {
            let mut buf = [0; 4];
            out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
            continue;
        }
        if chars.peek() == Some(&MARK) {
            chars.next();
            out.extend_from_slice(&MARK_UTF8);
            continue;
        }
        let hex: String = chars.clone().take(2).collect();
        match u8::from_str_radix(&hex, 16) {
            Ok(b) if hex.len() == 2 => {
                out.push(b);
                chars.next();
                chars.next();
            }
            _ => out.extend_from_slice(&MARK_UTF8),
        }
    }
    out
}

/// A path as a wire string.
pub fn path(p: &Path) -> String {
    encode(p.as_os_str().as_bytes())
}

/// The path a wire string names.
pub fn to_path(s: &str) -> PathBuf {
    PathBuf::from(std::ffi::OsString::from_vec(decode(s)))
}

/// A wire string for people: each escaped byte as `\xAE`.
pub fn display(s: &str) -> String {
    let bytes = decode(s);
    let mut out = String::with_capacity(bytes.len());
    for chunk in bytes.utf8_chunks() {
        out.push_str(chunk.valid());
        for b in chunk.invalid() {
            out.push_str(&format!("\\x{b:02X}"));
        }
    }
    out
}

/// Serialized JSON on its way out: escaped bytes become `\udcXX`, doubled
/// markers a single U+E000. (serde_json writes non-ASCII characters as
/// they are, so markers appear as their UTF-8 bytes.)
pub fn out(json: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(json.len());
    let mut i = 0;
    while i < json.len() {
        if json[i..].starts_with(&MARK_UTF8) {
            let rest = &json[i + 3..];
            if rest.starts_with(&MARK_UTF8) {
                out.extend_from_slice(&MARK_UTF8);
                i += 6;
                continue;
            }
            if rest.len() >= 2 && rest[0].is_ascii_hexdigit() && rest[1].is_ascii_hexdigit() {
                out.extend_from_slice(b"\\udc");
                out.extend_from_slice(&rest[..2]);
                i += 5;
                continue;
            }
        }
        out.push(json[i]);
        i += 1;
    }
    out
}

/// JSON text as received, before parsing: `\udc80`–`\udcff` become escaped
/// bytes, and U+E000 (raw or `\ue000`) is doubled.
pub fn incoming(json: &[u8]) -> Vec<u8> {
    let hex4 = |s: &[u8]| -> Option<u16> {
        let s = std::str::from_utf8(s.get(..4)?).ok()?;
        u16::from_str_radix(s, 16).ok()
    };
    let mut out = Vec::with_capacity(json.len());
    let mut i = 0;
    while i < json.len() {
        if json[i..].starts_with(&MARK_UTF8) {
            out.extend_from_slice(&MARK_UTF8);
            out.extend_from_slice(&MARK_UTF8);
            i += 3;
            continue;
        }
        if json[i] == b'\\' && i + 1 < json.len() {
            if json[i + 1] == b'u'
                && let Some(code) = hex4(&json[i + 2..])
            {
                if (0xDC80..=0xDCFF).contains(&code) {
                    out.extend_from_slice(&MARK_UTF8);
                    out.extend_from_slice(format!("{:02x}", code - 0xDC00).as_bytes());
                    i += 6;
                    continue;
                }
                if code == 0xE000 {
                    out.extend_from_slice(&MARK_UTF8);
                    out.extend_from_slice(&MARK_UTF8);
                    i += 6;
                    continue;
                }
            }
            // Any other escape, `\\` included, passes through whole.
            out.extend_from_slice(&json[i..i + 2]);
            i += 2;
            continue;
        }
        out.push(json[i]);
        i += 1;
    }
    out
}

/// `#[serde(with = "steward_proto::wire::serde_path")]` for a `PathBuf`.
pub mod serde_path {
    use serde::{Deserialize, Deserializer, Serializer};
    use std::path::{Path, PathBuf};

    pub fn serialize<S: Serializer>(p: &Path, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&super::path(p))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<PathBuf, D::Error> {
        Ok(super::to_path(&String::deserialize(d)?))
    }
}

/// `#[serde(with = "steward_proto::wire::serde_paths")]` for `Vec<PathBuf>`.
pub mod serde_paths {
    use serde::ser::SerializeSeq as _;
    use serde::{Deserialize, Deserializer, Serializer};
    use std::path::PathBuf;

    pub fn serialize<S: Serializer>(ps: &[PathBuf], s: S) -> Result<S::Ok, S::Error> {
        let mut seq = s.serialize_seq(Some(ps.len()))?;
        for p in ps {
            seq.serialize_element(&super::path(p))?;
        }
        seq.end()
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<PathBuf>, D::Error> {
        Ok(Vec::<String>::deserialize(d)?
            .iter()
            .map(|s| super::to_path(s))
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invalid_bytes_round_trip_through_json() {
        let raw: &[u8] = b"/m/Arch Deluxe\xae.doc \xee\x80\x80 caf\xc3\xa9 \\udcae";
        let s = encode(raw);
        assert_eq!(decode(&s), raw);
        assert_eq!(display(&s), "/m/Arch Deluxe\\xAE.doc \u{E000} café \\udcae");

        let json = out(&serde_json::to_vec(&serde_json::json!({ "path": s })).unwrap());
        let text = String::from_utf8(json.clone()).unwrap();
        assert!(text.contains(r"Deluxe\udcae.doc"), "{text}");
        assert!(
            text.contains("\u{E000}"),
            "a real U+E000 travels as itself: {text}"
        );
        assert!(
            text.contains(r"\\udcae"),
            "a literal backslash-u stays literal: {text}"
        );

        let back: serde_json::Value = serde_json::from_slice(&incoming(&json)).unwrap();
        assert_eq!(decode(back["path"].as_str().unwrap()), raw);
    }

    #[test]
    fn python_style_input_decodes() {
        // What Python's json.dumps sends for os.fsdecode(b"\xe9t\xe9") + "\ue000".
        let sent = b"{\"path\": \"\\udce9t\\udce9\\ue000\"}";
        let v: serde_json::Value = serde_json::from_slice(&incoming(sent)).unwrap();
        assert_eq!(
            decode(v["path"].as_str().unwrap()),
            b"\xe9t\xe9\xee\x80\x80"
        );
    }
}
