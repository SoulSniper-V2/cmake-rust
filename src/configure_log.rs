//! Configure log (`CMakeFiles/CMakeConfigureLog.yaml`) — port of
//! `cmConfigureLog`. The log is created lazily: the file is opened (append
//! mode) the first time an event is written, and closed with the trailing
//! `...` document-end marker when the log is dropped.

use std::io::Write;

use crate::interp::Interp;
use crate::messages::Frame;
use crate::util;

/// Most recent configure-log format version (cmConfigureLog constructor).
const LATEST_LOG_VERSION: u64 = 1;

#[derive(Debug)]
enum Sink {
    /// Real log file, opened lazily by `ensure_init`.
    File(std::fs::File),
    /// In-memory sink used by tests and in-process reuse.
    #[allow(dead_code)]
    Buffer(Vec<u8>),
}

/// Port of `cmConfigureLog`.
#[derive(Debug)]
pub struct ConfigureLog {
    log_dir: String,
    log_versions: Vec<u64>,
    indent: usize,
    opened: bool,
    sink: Option<Sink>,
}

impl ConfigureLog {
    /// `cmConfigureLog(logDir, logVersions)`. `log_dir` is the directory
    /// holding `CMakeConfigureLog.yaml` (`<bindir>/CMakeFiles`).
    pub fn new(log_dir: String, mut log_versions: Vec<u64>) -> ConfigureLog {
        // Always emit events for the latest log version.
        if !log_versions.contains(&LATEST_LOG_VERSION) {
            log_versions.push(LATEST_LOG_VERSION);
        }
        ConfigureLog {
            log_dir,
            log_versions,
            indent: 0,
            opened: false,
            sink: None,
        }
    }

    /// A log writing to an in-memory buffer (tests and in-process reuse).
    #[allow(dead_code)]
    pub fn buffered() -> ConfigureLog {
        let mut log = ConfigureLog::new(String::new(), vec![1]);
        log.sink = Some(Sink::Buffer(Vec::new()));
        log
    }

    /// Buffer contents (buffered logs only).
    #[allow(dead_code)]
    pub fn text(&self) -> Option<String> {
        match &self.sink {
            Some(Sink::Buffer(buffer)) => Some(String::from_utf8_lossy(buffer).into_owned()),
            _ => None,
        }
    }

    /// `cmConfigureLog::IsAnyLogVersionEnabled`.
    pub fn is_any_log_version_enabled(&self, versions: &[u64]) -> bool {
        // Both input lists are sorted.  Look for a matching element.
        let mut i1 = 0;
        let mut i2 = 0;
        while i1 < versions.len() && i2 < self.log_versions.len() {
            if versions[i1] < self.log_versions[i2] {
                i1 += 1;
            } else if self.log_versions[i2] < versions[i1] {
                i2 += 1;
            } else {
                return true;
            }
        }
        false
    }

    fn out(&mut self, text: &str) {
        if let Some(sink) = self.sink.as_mut() {
            match sink {
                Sink::File(file) => {
                    let _ = file.write_all(text.as_bytes());
                }
                Sink::Buffer(buffer) => buffer.extend_from_slice(text.as_bytes()),
            }
        }
    }

    /// `cmConfigureLog::EnsureInit`: opens `<log_dir>/CMakeConfigureLog.yaml`
    /// in append mode on first use and starts the events document.
    pub fn ensure_init(&mut self) {
        if self.opened {
            return;
        }
        if self.sink.is_none() {
            let name = format!("{}/CMakeConfigureLog.yaml", self.log_dir);
            match std::fs::OpenOptions::new()
                .append(true)
                .create(true)
                .open(&name)
            {
                Ok(file) => self.sink = Some(Sink::File(file)),
                Err(_) => return,
            }
        }
        self.opened = true;
        self.out("\n---\n");
        self.begin_object("events");
    }

    fn begin_line(&mut self) {
        for _ in 0..self.indent {
            self.out("  ");
        }
    }

    fn end_line(&mut self) {
        self.out("\n");
    }

    /// `cmConfigureLog::BeginObject`.
    pub fn begin_object(&mut self, key: &str) {
        self.begin_line();
        self.out(&format!("{key}:"));
        self.end_line();
        self.indent += 1;
    }

    /// `cmConfigureLog::BeginArray`.
    pub fn begin_array(&mut self) {
        self.indent += 1;
    }

    /// `cmConfigureLog::NextArrayElement`.
    pub fn next_array_element(&mut self) {
        assert!(self.indent > 0);
        self.indent -= 1;
        self.begin_line();
        self.out("-");
        self.end_line();
        self.indent += 1;
    }

    /// `cmConfigureLog::EndArray`.
    pub fn end_array(&mut self) {
        assert!(self.indent > 0);
        self.indent -= 1;
    }

    /// `cmConfigureLog::EndObject`.
    pub fn end_object(&mut self) {
        assert!(self.indent > 0);
        self.indent -= 1;
    }

    /// `cmConfigureLog::BeginEvent` — `mf` supplies the backtrace and the
    /// home directory used to relativize paths.
    pub fn begin_event(&mut self, kind: &str, interp: &Interp) {
        self.ensure_init();
        self.begin_line();
        self.out("-");
        self.end_line();
        self.indent += 1;
        self.write_value_str("kind", kind);
        self.write_backtrace(interp);
        self.write_checks(&interp.ext.message_checks);
    }

    /// `cmConfigureLog::EndEvent`.
    pub fn end_event(&mut self) {
        assert!(self.indent > 0);
        self.indent -= 1;
    }

    /// `cmConfigureLog::WriteBacktrace`.
    fn write_backtrace(&mut self, interp: &Interp) {
        let mut backtrace = Vec::new();
        let root = interp.home_source.clone();
        // cmListFileBacktrace iterates from the most recent frame downward.
        for frame in interp.backtrace.iter().rev() {
            if frame.name.as_ref().is_some_and(|name| !name.is_empty()) || frame.deferred {
                let file = util::relative_if_under(&root, &frame.file);
                backtrace.push(format_frame(&file, frame));
            }
        }
        self.write_value_list("backtrace", &backtrace);
    }

    /// `cmConfigureLog::WriteChecks`.
    fn write_checks(&mut self, checks: &[String]) {
        if checks.is_empty() {
            return;
        }
        self.begin_object("checks");
        for check in checks.iter().rev() {
            let encoded = encode_string(check);
            self.begin_line();
            self.out("- ");
            self.out(&encoded);
            self.end_line();
        }
        self.end_object();
    }

    /// `WriteValue(key, nullptr)`.
    #[allow(dead_code)]
    pub fn write_value_null(&mut self, key: &str) {
        self.begin_line();
        self.out(&format!("{key}: null"));
        self.end_line();
    }

    /// `WriteValue(key, bool)`.
    pub fn write_value_bool(&mut self, key: &str, value: bool) {
        self.begin_line();
        self.out(&format!("{key}: {}", if value { "true" } else { "false" }));
        self.end_line();
    }

    /// `WriteValue(key, int)`.
    pub fn write_value_int(&mut self, key: &str, value: i64) {
        self.begin_line();
        self.out(&format!("{key}: {value}"));
        self.end_line();
    }

    /// `WriteValue(key, string)` — JSON-encoded string.
    pub fn write_value_str(&mut self, key: &str, value: &str) {
        self.begin_line();
        self.out(&format!("{key}: "));
        self.out(&encode_string(value));
        self.end_line();
    }

    /// `WriteValue(key, vector<string>)`.
    pub fn write_value_list(&mut self, key: &str, list: &[String]) {
        self.begin_object(key);
        for value in list {
            self.begin_line();
            self.out("- ");
            self.out(&encode_string(value));
            self.end_line();
        }
        self.end_object();
    }

    /// `WriteValue(key, map<string,string>)`.
    pub fn write_value_map(&mut self, key: &str, map: &std::collections::BTreeMap<String, String>) {
        self.begin_object(key);
        for (name, value) in map {
            if name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
            {
                self.write_value_str(name, value);
            } else {
                self.begin_line();
                self.out(&encode_string(name));
                self.out(": ");
                self.out(&encode_string(value));
                self.end_line();
            }
        }
        self.end_object();
    }

    /// `cmConfigureLog::WriteLiteralTextBlock`.
    pub fn write_literal_text_block(&mut self, key: &str, text: &str) {
        self.begin_line();
        self.out(&format!("{key}: |"));
        self.end_line();

        let bytes = text.as_bytes();
        if bytes.is_empty() {
            return;
        }
        self.indent += 1;
        self.begin_line();

        let mut index = 0;
        while index < bytes.len() {
            // YAML allows ' ', '\t' and "printable characters", but NOT other
            // ASCII whitespace; those must be escaped, as must the upper
            // UNICODE control characters (U+0080 - U+009F).
            const C1_LAST: u32 = 0x9F;
            let c = bytes[index];
            match c {
                b'\r' => {
                    // Print a carriage return only if it is not followed by a
                    // line feed.
                    index += 1;
                    if index == bytes.len() || bytes[index] != b'\n' {
                        self.write_escape(c);
                    }
                }
                b'\n' => {
                    // Print any line feeds except the very last one.
                    if index + 1 < bytes.len() {
                        self.end_line();
                        self.begin_line();
                    }
                    index += 1;
                }
                b'\t' => {
                    // Print horizontal tab verbatim.
                    self.out("\t");
                    index += 1;
                }
                b'\\' => {
                    // Escape backslash for disambiguation.
                    self.out("\\\\");
                    index += 1;
                }
                _ => {
                    if (32..127).contains(&c) {
                        // Print ascii byte.
                        self.out(&format!("{}", c as char));
                        index += 1;
                    } else if c > 127 {
                        // Decode a UTF-8 sequence.
                        let (codepoint, len) = utf8_decode(&bytes[index..]);
                        if len > 0 && codepoint > C1_LAST {
                            let text =
                                String::from_utf8_lossy(&bytes[index..index + len]).into_owned();
                            self.out(&text);
                            index += len;
                            continue;
                        }
                        // Escape non-printable byte.
                        self.write_escape(c);
                        index += 1;
                    } else {
                        // Escape non-printable byte.
                        self.write_escape(c);
                        index += 1;
                    }
                }
            }
        }

        self.end_line();
        self.indent -= 1;
    }

    /// `cmConfigureLog::WriteEscape` (`\xNN`).
    fn write_escape(&mut self, c: u8) {
        self.out(&format!("\\x{c:02x}"));
    }

    /// Flush buffered content to disk and write the document end marker.
    fn finish(&mut self) {
        if self.opened {
            self.end_object();
            self.out("...\n");
            self.opened = false;
        }
    }
}

impl Drop for ConfigureLog {
    fn drop(&mut self) {
        self.finish();
    }
}

/// `operator<<(cmListFileContext)`: `file`, `file:line (name)` or
/// `file:DEFERRED`.
fn format_frame(file: &str, frame: &Frame) -> String {
    if frame.deferred {
        format!("{file}:DEFERRED")
    } else if frame.line > 0 {
        match &frame.name {
            Some(name) if !name.is_empty() => format!("{file}:{} ({name})", frame.line),
            _ => format!("{file}:{}", frame.line),
        }
    } else {
        file.to_owned()
    }
}

/// Decodes one UTF-8 sequence, returning (codepoint, bytes consumed) or
/// (0xFFFD, 0) on invalid input (`cm_utf8_decode_character`, which also
/// accepts the pre-RFC 5- and 6-byte forms).
fn utf8_decode(bytes: &[u8]) -> (u32, usize) {
    const MINS: [u32; 7] = [0, 0, 1 << 7, 1 << 11, 1 << 16, 1 << 21, 1 << 26];
    let Some(&first) = bytes.first() else {
        return (0xFFFD, 0);
    };
    if first < 0x80 {
        return (first as u32, 1);
    }
    let width = match first {
        0xC0..=0xDF => 2,
        0xE0..=0xEF => 3,
        0xF0..=0xF7 => 4,
        0xF8..=0xFB => 5,
        0xFC..=0xFD => 6,
        _ => return (0xFFFD, 0),
    };
    let mut codepoint = u32::from(first) & (0xFF >> (width + 1));
    let mut index = 1;
    while index < width {
        let Some(&byte) = bytes.get(index) else {
            return (0xFFFD, 0);
        };
        if !(0x80..=0xBF).contains(&byte) {
            return (0xFFFD, 0);
        }
        codepoint = (codepoint << 6) | u32::from(byte & 0x3F);
        index += 1;
    }
    if codepoint < MINS[width - 1] {
        return (0xFFFD, 0);
    }
    (codepoint, width)
}

/// `utf8ToCodepoint` from cm3p jsoncpp: like `utf8_decode` but returns
/// U+FFFD while consuming only the first byte for invalid sequences.
fn utf8_to_codepoint(bytes: &[u8]) -> (u32, usize) {
    let Some(&first) = bytes.first() else {
        return (0xFFFD, 0);
    };
    if first < 0x80 {
        return (first as u32, 0);
    }
    if first < 0xE0 {
        if bytes.len() < 2 {
            return (0xFFFD, 0);
        }
        let calculated = (u32::from(first & 0x1F) << 6) | u32::from(bytes[1] & 0x3F);
        // oversized encoded characters are invalid
        return if calculated < 0x80 {
            (0xFFFD, 1)
        } else {
            (calculated, 1)
        };
    }
    if first < 0xF0 {
        if bytes.len() < 3 {
            return (0xFFFD, 0);
        }
        let calculated = (u32::from(first & 0x0F) << 12)
            | (u32::from(bytes[1] & 0x3F) << 6)
            | u32::from(bytes[2] & 0x3F);
        // surrogates aren't valid codepoints itself
        // shouldn't be UTF-8 encoded
        if (0xD800..=0xDFFF).contains(&calculated) {
            return (0xFFFD, 2);
        }
        // oversized encoded characters are invalid
        return if calculated < 0x800 {
            (0xFFFD, 2)
        } else {
            (calculated, 2)
        };
    }
    if first < 0xF8 {
        if bytes.len() < 4 {
            return (0xFFFD, 0);
        }
        let calculated = (u32::from(first & 0x07) << 18)
            | (u32::from(bytes[1] & 0x3F) << 12)
            | (u32::from(bytes[2] & 0x3F) << 6)
            | u32::from(bytes[3] & 0x3F);
        return if calculated < 0x10000 {
            (0xFFFD, 3)
        } else {
            (calculated, 3)
        };
    }
    (0xFFFD, 0)
}

/// `Json::valueToQuotedStringN` with the default writer (emitUTF8=false).
fn encode_string(value: &str) -> String {
    let bytes = value.as_bytes();
    let needs_escape = bytes
        .iter()
        .any(|&c| c == b'\\' || c == b'"' || !(0x20..=0x7F).contains(&c));
    if !needs_escape {
        return format!("\"{value}\"");
    }
    let mut result = String::with_capacity(value.len() + 2);
    result.push('"');
    let mut index = 0;
    while index < bytes.len() {
        let c = bytes[index];
        match c {
            b'"' => result.push_str("\\\""),
            b'\\' => result.push_str("\\\\"),
            0x08 => result.push_str("\\b"),
            0x0C => result.push_str("\\f"),
            b'\n' => result.push_str("\\n"),
            b'\r' => result.push_str("\\r"),
            b'\t' => result.push_str("\\t"),
            _ => {
                let (codepoint, consumed) = utf8_to_codepoint(&bytes[index..]);
                index += consumed;
                if codepoint < 0x20 {
                    result.push_str(&format!("\\u{codepoint:04x}"));
                } else if codepoint < 0x80 {
                    result.push(char::from_u32(codepoint).unwrap_or('\u{fffd}'));
                } else if codepoint < 0x10000 {
                    result.push_str(&format!("\\u{codepoint:04x}"));
                } else {
                    // Extended Unicode. Encode 20 bits as a surrogate pair.
                    let cp = codepoint - 0x10000;
                    result.push_str(&format!("\\u{:04x}", 0xd800 + ((cp >> 10) & 0x3ff)));
                    result.push_str(&format!("\\u{:04x}", 0xdc00 + (cp & 0x3ff)));
                }
            }
        }
        index += 1;
    }
    result.push('"');
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_encoding() {
        assert_eq!(encode_string("plain"), "\"plain\"");
        assert_eq!(encode_string("a\"b"), "\"a\\\"b\"");
        assert_eq!(encode_string("a\\b"), "\"a\\\\b\"");
        assert_eq!(encode_string("a\nb\rc\td"), "\"a\\nb\\rc\\td\"");
        assert_eq!(encode_string("\u{1}"), "\"\\u0001\"");
        assert_eq!(encode_string("é"), "\"\\u00e9\"");
        assert_eq!(encode_string("中"), "\"\\u4e2d\"");
        assert_eq!(encode_string("\u{1F600}"), "\"\\ud83d\\ude00\"");
        assert_eq!(encode_string(""), "\"\"");
    }

    #[test]
    fn literal_block() {
        let mut log = ConfigureLog::buffered();
        log.ensure_init();
        log.indent = 2;
        log.write_literal_text_block("stdout", "line1\nline2\n");
        log.write_literal_text_block("empty", "");
        log.finish();
        let text = log.text().unwrap();
        assert_eq!(
            text,
            "\n---\nevents:\n    stdout: |\n      line1\n      line2\n    empty: |\n...\n"
        );
    }

    #[test]
    fn literal_block_escapes() {
        let mut log = ConfigureLog::buffered();
        log.ensure_init();
        log.indent = 1;
        log.write_literal_text_block("out", "a\rb\r\nc\\d\x01z");
        log.finish();
        let text = log.text().unwrap();
        assert_eq!(
            text,
            "\n---\nevents:\n  out: |\n    a\\x0db\n    c\\\\d\\x01z\n...\n"
        );
    }

    #[test]
    fn map_writing() {
        let mut log = ConfigureLog::buffered();
        log.ensure_init();
        log.indent = 1;
        let mut map = std::collections::BTreeMap::new();
        map.insert("CMAKE_C_FLAGS".to_owned(), "".to_owned());
        map.insert("odd key".to_owned(), "v".to_owned());
        log.write_value_map("cmakeVariables", &map);
        log.finish();
        let text = log.text().unwrap();
        assert_eq!(
            text,
            "\n---\nevents:\n  cmakeVariables:\n    CMAKE_C_FLAGS: \"\"\n    \"odd key\": \"v\"\n...\n"
        );
    }
}
