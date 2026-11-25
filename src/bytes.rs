//! Lossless representation of CMake byte strings in Rust `String`s.
//!
//! CMake values are byte strings, while Rust `String` must be valid UTF-8.
//! This module maps every byte that is not part of a valid UTF-8 sequence to
//! `U+10FF00 + byte` (in the supplementary private-use area). Valid UTF-8 is
//! passed through unchanged. Because bytes `0x00..=0x7f` are valid UTF-8
//! individually, decoding only produces `U+10FF80..=U+10FFFF`.
//!
//! There is one unavoidable ambiguity: genuine input text containing
//! `U+10FF80..=U+10FFFF` cannot be distinguished from encoded non-UTF-8
//! bytes and is therefore not representable round-trip. These are plane-16
//! private-use characters and noncharacters, so they do not occur in ordinary
//! CMake input.

use std::borrow::Cow;

const BYTE_CHAR_BASE: u32 = 0x10ff00;
const BYTE_CHAR_FIRST: u32 = 0x10ff80;
const BYTE_CHAR_LAST: u32 = 0x10ffff;

fn byte_char(byte: u8) -> char {
    // The sum is at most U+10FFFF and never a surrogate.
    char::from_u32(BYTE_CHAR_BASE + u32::from(byte)).unwrap()
}

fn encoded_byte(ch: char) -> Option<u8> {
    let code = ch as u32;
    (BYTE_CHAR_FIRST..=BYTE_CHAR_LAST)
        .contains(&code)
        .then_some((code - BYTE_CHAR_BASE) as u8)
}

/// Decode external bytes into CMake's internal string representation.
///
/// Every maximal valid UTF-8 sequence passes through unchanged. Each byte of
/// an overlong, truncated, or otherwise invalid sequence becomes the private
/// use character `U+10FF00 + byte`.
pub fn decode(bytes: &[u8]) -> String {
    if let Ok(text) = std::str::from_utf8(bytes) {
        return text.to_owned();
    }
    let mut out = String::with_capacity(bytes.len());
    let mut rest = bytes;
    while !rest.is_empty() {
        match std::str::from_utf8(rest) {
            Ok(valid) => {
                out.push_str(valid);
                break;
            }
            Err(error) => {
                let valid = error.valid_up_to();
                // `valid_up_to` is by definition a UTF-8 boundary.
                out.push_str(unsafe { std::str::from_utf8_unchecked(&rest[..valid]) });
                let invalid = error.error_len().unwrap_or(rest.len() - valid);
                for &byte in &rest[valid..valid + invalid] {
                    out.push(byte_char(byte));
                }
                rest = &rest[valid + invalid..];
            }
        }
    }
    out
}

/// Encode CMake's internal string representation back to external bytes.
///
/// This is the inverse of [`decode`]. The borrowed fast path is used whenever
/// the string cannot contain an encoded byte character; a quick scan for the
/// UTF-8 leading byte shared by that plane avoids a character decode of most
/// strings.
pub fn encode(text: &str) -> Cow<'_, [u8]> {
    let bytes = text.as_bytes();
    if !bytes.contains(&0xf4) || !text.chars().any(|ch| encoded_byte(ch).is_some()) {
        return Cow::Borrowed(bytes);
    }
    let mut out = Vec::with_capacity(bytes.len());
    for ch in text.chars() {
        if let Some(byte) = encoded_byte(ch) {
            out.push(byte);
        } else {
            let mut utf8 = [0; 4];
            out.extend_from_slice(ch.encode_utf8(&mut utf8).as_bytes());
        }
    }
    Cow::Owned(out)
}

/// Decode an OS string on Unix, where arguments, paths and environment values
/// are byte strings.
#[cfg(unix)]
pub fn decode_os(value: &std::ffi::OsStr) -> String {
    use std::os::unix::ffi::OsStrExt;
    decode(value.as_bytes())
}

/// Encode an internal value as an OS string on Unix.
#[cfg(unix)]
pub fn encode_os(value: &str) -> std::ffi::OsString {
    use std::os::unix::ffi::OsStringExt;
    std::ffi::OsString::from_vec(encode(value).into_owned())
}

/// Encode an internal path value for a filesystem or process API on Unix.
#[cfg(unix)]
pub fn path(value: &str) -> std::path::PathBuf {
    std::path::PathBuf::from(encode_os(value))
}

/// Write text to stdout at an output boundary.
pub fn write_stdout(text: &str) {
    use std::io::Write;
    let mut out = std::io::stdout().lock();
    let _ = out.write_all(&encode(text));
    let _ = out.flush();
}

/// Write text plus one newline to stdout at an output boundary.
pub fn stdout_line(text: &str) {
    use std::io::Write;
    let mut out = std::io::stdout().lock();
    let _ = out.write_all(&encode(text));
    let _ = out.write_all(b"\n");
    let _ = out.flush();
}

/// Write text plus one newline to stderr at an output boundary.
pub fn stderr_line(text: &str) {
    use std::io::Write;
    let mut out = std::io::stderr().lock();
    let _ = out.write_all(&encode(text));
    let _ = out.write_all(b"\n");
    let _ = out.flush();
}

/// First occurrence of `needle` in `haystack`, using byte-string semantics.
pub fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() {
        return Some(0);
    }
    if needle.len() > haystack.len() {
        return None;
    }
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

/// Last occurrence of `needle` in `haystack`, using byte-string semantics.
pub fn rfind(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() {
        return Some(haystack.len());
    }
    if needle.len() > haystack.len() {
        return None;
    }
    haystack
        .windows(needle.len())
        .rposition(|window| window == needle)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mapped(bytes: &[u8]) -> String {
        bytes.iter().map(|&byte| byte_char(byte)).collect()
    }

    #[test]
    fn round_trip_valid_and_invalid() {
        for bytes in [
            &b""[..],
            b"plain ascii",
            "caf\u{e9} \u{20ac} \u{1f600}".as_bytes(),
            b"caf\xe9\n",
            b"\x80\x81\xfe\xff",
            b"a\xc3(b",
            b"\xed\xa0\x80",
        ] {
            assert_eq!(&*encode(&decode(bytes)), bytes, "bytes: {bytes:02x?}");
        }
    }

    #[test]
    fn overlong_and_truncated_sequences_are_bytewise() {
        assert_eq!(decode(b"\xc0\x80"), mapped(b"\xc0\x80"));
        assert_eq!(decode(b"\xe0\x80\x80"), mapped(b"\xe0\x80\x80"));
        assert_eq!(decode(b"\xf4\x90\x80\x80"), mapped(b"\xf4\x90\x80\x80"));
        assert_eq!(decode(b"\xe2\x82"), mapped(b"\xe2\x82"));
        assert_eq!(decode(b"\xf0\x9f\x98"), mapped(b"\xf0\x9f\x98"));
        // A truncated prefix does not consume the following ASCII byte.
        assert_eq!(decode(b"\xc3("), format!("{}(", byte_char(0xc3)));
    }

    #[test]
    fn invalid_bytes_do_not_swallow_valid_sequences() {
        let input = b"\xff\xc3\xa9\x80x";
        assert_eq!(&*encode(&decode(input)), input);
        let text = decode(input);
        assert!(text.contains('é'));
    }

    #[test]
    fn encode_fast_path_borrows() {
        let text = "ordinary utf-8 \u{20ac}";
        assert!(matches!(encode(text), Cow::Borrowed(bytes) if bytes == text.as_bytes()));
        let with_mapped = decode(b"\xe9");
        assert!(matches!(encode(&with_mapped), Cow::Owned(bytes) if bytes == b"\xe9"));
    }

    #[test]
    fn byte_search() {
        assert_eq!(find(b"abcabc", b"bc"), Some(1));
        assert_eq!(rfind(b"abcabc", b"bc"), Some(4));
        assert_eq!(find(b"abc", b""), Some(0));
        assert_eq!(rfind(b"abc", b""), Some(3));
        assert_eq!(find(b"abc", b"d"), None);
    }
}
