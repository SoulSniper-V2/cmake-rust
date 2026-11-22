//! Small value helpers shared across the interpreter, mirroring
//! `cmValue`, `cmList`, `cmSystemTools` behaviors.

use std::ffi::CString;

/// `cmValue::IsOn`.
pub fn is_on(value: &str) -> bool {
    let lower = value.to_ascii_lowercase();
    matches!(lower.as_str(), "1" | "y" | "on" | "yes" | "true")
}

/// `cmValue::IsNOTFOUND`.
pub fn is_notfound(value: &str) -> bool {
    value == "NOTFOUND" || value.ends_with("-NOTFOUND")
}

/// `cmValue::IsOff` for a present value.
pub fn is_off(value: &str) -> bool {
    let lower = value.to_ascii_lowercase();
    matches!(
        lower.as_str(),
        "" | "0" | "n" | "no" | "off" | "false" | "ignore"
    ) || is_notfound(value)
}

/// `cmIsOff(cmValue)`: a missing value is off.
pub fn is_off_opt(value: Option<&str>) -> bool {
    value.is_none_or(is_off)
}

/// `cmIsOn(cmValue)`.
pub fn is_on_opt(value: Option<&str>) -> bool {
    value.is_some_and(is_on)
}

/// `cmValue::IsOn(value) || !IsOff` style truthiness used by `cmMakefile::IsOn`
/// (IsOn of the definition) — note this is NOT the if() boolean rule.
/// C `strtod` over the whole string: returns the value only when the entire
/// string parses.
pub fn strtod_full(value: &str) -> Option<f64> {
    let c = CString::new(value).ok()?;
    let mut end: *mut libc::c_char = std::ptr::null_mut();
    // SAFETY: `c` is a valid NUL-terminated string; `end` receives a pointer
    // into it.
    let parsed = unsafe { libc::strtod(c.as_ptr(), &mut end) };
    // SAFETY: `end` points into `c` (possibly at its terminator).
    let rest = unsafe { *end };
    (rest == 0 && !std::ptr::eq(end, c.as_ptr())).then_some(parsed)
}

/// C `sscanf("%lg")`: parses a leading floating-point number, ignoring the
/// remainder.
pub fn scan_double(value: &str) -> Option<f64> {
    let c = CString::new(value).ok()?;
    let mut out: libc::c_double = 0.0;
    // SAFETY: format string and output pointer match.
    let count = unsafe { libc::sscanf(c.as_ptr(), c"%lg".as_ptr(), &mut out) };
    (count == 1).then_some(out)
}

/// C `strtol(value, &end, 10)` semantics: (parsed, fully consumed).
pub fn strtol(value: &str, base: i32) -> (i64, bool) {
    let Ok(c) = CString::new(value) else {
        return (0, false);
    };
    let mut end: *mut libc::c_char = std::ptr::null_mut();
    // SAFETY: valid string.
    let parsed = unsafe {
        *libc::__errno_location() = 0;
        libc::strtol(c.as_ptr(), &mut end, base)
    };
    // `cmStrToLong` treats ERANGE as failure.
    let erange = unsafe { *libc::__errno_location() } == libc::ERANGE;
    // SAFETY: end points into c.
    let rest = unsafe { *end };
    (
        parsed as i64,
        rest == 0 && !std::ptr::eq(end, c.as_ptr()) && !erange,
    )
}

/// `cmList` expansion of a single value (`cmExpandList`). Splits on `;`
/// outside of square brackets, turns `\;` into `;`, and drops empty
/// elements unless `keep_empty`.
pub fn expand_list(value: &str, keep_empty: bool) -> Vec<String> {
    let mut out = Vec::new();
    expand_list_into(value, keep_empty, &mut out);
    out
}

pub fn expand_list_into(value: &str, keep_empty: bool, out: &mut Vec<String>) {
    // Mirrors cmList::Insert with ExpandElements::Yes.
    if value.is_empty() && !keep_empty {
        return;
    }
    if !value.contains(';') {
        out.push(value.to_owned());
        return;
    }
    let bytes = value.as_bytes();
    let mut item: Vec<u8> = Vec::new();
    let mut square: i64 = 0;
    let mut last = 0usize;
    let mut index = 0usize;
    while index < bytes.len() {
        match bytes[index] {
            b'\\' if bytes.get(index + 1) == Some(&b';') => {
                item.extend_from_slice(&bytes[last..index]);
                last = index + 1;
                index += 1;
            }
            b'[' => square += 1,
            b']' => square -= 1,
            b';' if square == 0 => {
                item.extend_from_slice(&bytes[last..index]);
                last = index + 1;
                if !item.is_empty() || keep_empty {
                    out.push(String::from_utf8_lossy(&item).into_owned());
                    item.clear();
                }
            }
            _ => {}
        }
        index += 1;
    }
    item.extend_from_slice(&bytes[last..]);
    if !item.is_empty() || keep_empty {
        out.push(String::from_utf8_lossy(&item).into_owned());
    }
}

/// `cmList::to_string`: join with `;`.
pub fn join_list<S: AsRef<str>>(items: &[S]) -> String {
    let mut out = String::new();
    for (index, item) in items.iter().enumerate() {
        if index > 0 {
            out.push(';');
        }
        out.push_str(item.as_ref());
    }
    out
}

/// `cmSystemTools::VersionCompare`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CompareOp {
    Less,
    LessEqual,
    Greater,
    GreaterEqual,
    Equal,
}

impl CompareOp {
    fn bits(self) -> u8 {
        match self {
            CompareOp::Less => 1,
            CompareOp::Greater => 2,
            CompareOp::Equal => 4,
            CompareOp::LessEqual => 1 | 4,
            CompareOp::GreaterEqual => 2 | 4,
        }
    }
}

pub fn version_compare(op: CompareOp, lhs: &str, rhs: &str) -> bool {
    let l = lhs.as_bytes();
    let r = rhs.as_bytes();
    let at = |s: &[u8], i: usize| s.get(i).copied().unwrap_or(0);
    let (mut el, mut er) = (0usize, 0usize);
    while at(l, el).is_ascii_digit() || at(r, er).is_ascii_digit() {
        while at(l, el) == b'0' {
            el += 1;
        }
        while at(r, er) == b'0' {
            er += 1;
        }
        let (bl, br) = (el, er);
        while at(l, el).is_ascii_digit() {
            el += 1;
        }
        while at(r, er).is_ascii_digit() {
            er += 1;
        }
        let mut ord = (el - bl).cmp(&(er - br));
        if ord.is_eq() {
            ord = l[bl..el].cmp(&r[br..er]);
        }
        match ord {
            std::cmp::Ordering::Less => return op.bits() & 1 != 0,
            std::cmp::Ordering::Greater => return op.bits() & 2 != 0,
            std::cmp::Ordering::Equal => {}
        }
        if at(r, er) == b'.' {
            er += 1;
        }
        if at(l, el) == b'.' {
            el += 1;
        }
    }
    op.bits() & 4 != 0
}

/// `cmOutputConverter::EscapeForCMake` with quotes.
pub fn escape_for_cmake(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for ch in value.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '$' => out.push_str("\\$"),
            '\\' => out.push_str("\\\\"),
            _ => out.push(ch),
        }
    }
    out.push('"');
    out
}

/// `cmSystemTools::MakeCidentifier`.
pub fn make_c_identifier(input: &str) -> String {
    let mut result = String::with_capacity(input.len() + 1);
    if input.as_bytes().first().is_some_and(u8::is_ascii_digit) {
        result.push('_');
    }
    for byte in input.bytes() {
        result.push(if byte.is_ascii_alphanumeric() || byte == b'_' {
            char::from(byte)
        } else {
            '_'
        });
    }
    result
}

/// `cmSystemTools::GetFilenamePath`: directory part using `/`.
pub fn filename_path(path: &str) -> String {
    match path.rfind('/') {
        Some(0) => "/".to_owned(),
        Some(index) => path[..index].to_owned(),
        None => String::new(),
    }
}

/// `cmSystemTools::GetFilenameName`.
pub fn filename_name(path: &str) -> String {
    match path.rfind('/') {
        Some(index) => path[index + 1..].to_owned(),
        None => path.to_owned(),
    }
}

/// `cmSystemTools::GetFilenameLastExtension`.
pub fn filename_last_extension(path: &str) -> String {
    let name = filename_name(path);
    name.rfind('.')
        .map_or_else(String::new, |dot| name[dot..].to_owned())
}

/// `cmSystemTools::GetFilenameWithoutLastExtension`.
pub fn filename_without_last_extension(path: &str) -> String {
    let name = filename_name(path);
    name.rfind('.')
        .map_or_else(|| name.clone(), |dot| name[..dot].to_owned())
}

/// `cmSystemTools::GetCurrentDateTime`: `strftime` of the local time.
pub fn current_date_time(format: &str) -> String {
    let Ok(format) = CString::new(format) else {
        return String::new();
    };
    // SAFETY: time/localtime_r/strftime with valid buffers.
    unsafe {
        let now = libc::time(std::ptr::null_mut());
        let mut tm: libc::tm = std::mem::zeroed();
        libc::localtime_r(&now, &mut tm);
        let mut buffer = [0u8; 1024];
        let n = libc::strftime(
            buffer.as_mut_ptr().cast(),
            buffer.len(),
            format.as_ptr(),
            &tm,
        );
        String::from_utf8_lossy(&buffer[..n]).into_owned()
    }
}

/// `cmSystemTools::FileIsFullPath` (POSIX).
pub fn is_full_path(path: &str) -> bool {
    path.starts_with('/') || path.starts_with('~')
}

/// `cmSystemTools::CollapseFullPath(path, base)`: joins relative paths with
/// `base` and normalizes `.`/`..` components lexically (no symlink
/// resolution), matching kwsys `CollapseFullPath`.
pub fn collapse_full_path(path: &str, base: &str) -> String {
    let joined = if path.starts_with('/') {
        path.to_owned()
    } else if let Some(rest) = path.strip_prefix('~') {
        let home = std::env::var("HOME").unwrap_or_default();
        format!("{home}{rest}")
    } else if base.is_empty() {
        let cwd = std::env::current_dir()
            .map(|dir| dir.to_string_lossy().into_owned())
            .unwrap_or_default();
        format!("{cwd}/{path}")
    } else {
        format!("{base}/{path}")
    };
    let mut parts: Vec<&str> = Vec::new();
    for part in joined.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            other => parts.push(other),
        }
    }
    let mut out = String::from("/");
    out.push_str(&parts.join("/"));
    out
}

/// `cmSystemTools::IsSubDirectory(cSubdir, cDir)`.
pub fn is_subdirectory(sub: &str, dir: &str) -> bool {
    // kwsys IsSubDirectory: both paths go through ConvertToUnixSlashes,
    // which drops trailing slashes.
    let trim = |path: &str| -> String {
        let mut path = path.replace('\\', "/");
        while path.len() > 1 && path.ends_with('/') {
            path.pop();
        }
        path
    };
    let sub = trim(sub);
    let dir = trim(dir);
    if dir.is_empty() || sub.len() <= dir.len() {
        return false;
    }
    let root = dir.ends_with('/');
    let slash = if root { dir.len() - 1 } else { dir.len() };
    sub.as_bytes()[slash] == b'/' && sub[..dir.len()] == dir[..]
}

/// `cmSystemTools::RelativeIfUnder`.
pub fn relative_if_under(top: &str, path: &str) -> String {
    if path == top {
        ".".to_owned()
    } else if is_subdirectory(path, top) {
        path[top.len() + 1..].to_owned()
    } else {
        path.to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lists() {
        assert_eq!(expand_list("a;b;;c", false), ["a", "b", "c"]);
        assert_eq!(expand_list("a;b;;c", true), ["a", "b", "", "c"]);
        assert_eq!(expand_list("a\\;b;[c;d]", false), ["a;b", "[c;d]"]);
        assert_eq!(expand_list("a\\b", false), ["a\\b"]);
        assert_eq!(expand_list("", true), [""]);
    }

    #[test]
    fn versions() {
        assert!(version_compare(CompareOp::Less, "1.2", "1.10"));
        assert!(version_compare(CompareOp::Equal, "1.02", "1.2"));
        assert!(version_compare(CompareOp::Equal, "1.2", "1.2.0"));
        assert!(version_compare(CompareOp::Greater, "3.28.1", "3.28"));
    }

    #[test]
    fn numbers() {
        assert_eq!(strtod_full("0x10"), Some(16.0));
        assert_eq!(strtod_full("1e3"), Some(1000.0));
        assert_eq!(strtod_full("1a"), None);
        assert_eq!(scan_double(" 5abc"), Some(5.0));
    }
}
