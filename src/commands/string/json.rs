//! `string(JSON ...)` ported from `cmStringCommand.cxx`'s HandleJSONCommand,
//! with a faithful port of the bundled JsonCpp (1.9.6) `OurReader` parser and
//! `BuiltStyledStreamWriter` serializer.

use std::collections::BTreeMap;
use std::io::Write;

use crate::interp::{CmdResult, Interp, err};

use super::{cm_join, str_to_ulong};

// ---------------------------------------------------------------------
// Json::Value (member keys use `CZString` ordering = memcmp + length, the
// same as byte-order on `Vec<u8>`)

#[derive(Clone, Debug)]
pub enum Json {
    Null,
    Int(i64),
    UInt(u64),
    Real(f64),
    Bool(bool),
    Str(Vec<u8>),
    Array(Vec<Json>),
    Object(BTreeMap<Vec<u8>, Json>),
}

impl Json {
    fn type_name(&self) -> &'static str {
        match self {
            Json::Null => "NULL",
            Json::Int(_) | Json::UInt(_) | Json::Real(_) => "NUMBER",
            Json::Str(_) => "STRING",
            Json::Bool(_) => "BOOLEAN",
            Json::Array(_) => "ARRAY",
            Json::Object(_) => "OBJECT",
        }
    }

    fn size(&self) -> usize {
        match self {
            Json::Array(items) => items.len(),
            Json::Object(map) => map.len(),
            _ => 0,
        }
    }

    /// `Json::Value::asString`.
    fn as_string(&self) -> Vec<u8> {
        match self {
            Json::Null => Vec::new(),
            Json::Str(s) => s.clone(),
            Json::Bool(b) => if *b {
                b"true".as_slice()
            } else {
                b"false".as_slice()
            }
            .to_vec(),
            Json::Int(v) => v.to_string().into_bytes(),
            Json::UInt(v) => v.to_string().into_bytes(),
            Json::Real(v) => double_to_string(*v).into_bytes(),
            _ => Vec::new(),
        }
    }

    fn json_eq(&self, other: &Json) -> bool {
        match (self, other) {
            (Json::Null, Json::Null) => true,
            (Json::Int(a), Json::Int(b)) => a == b,
            (Json::UInt(a), Json::UInt(b)) => a == b,
            (Json::Real(a), Json::Real(b)) => a == b,
            (Json::Bool(a), Json::Bool(b)) => a == b,
            (Json::Str(a), Json::Str(b)) => a == b,
            (Json::Array(a), Json::Array(b)) => {
                a.len() == b.len() && a.iter().zip(b.iter()).all(|(x, y)| x.json_eq(y))
            }
            (Json::Object(a), Json::Object(b)) => {
                a.len() == b.len()
                    && a.iter()
                        .zip(b.iter())
                        .all(|((ka, va), (kb, vb))| ka == kb && va.json_eq(vb))
            }
            _ => false,
        }
    }
}

fn object_get_mut<'a>(map: &'a mut BTreeMap<Vec<u8>, Json>, key: &[u8]) -> Option<&'a mut Json> {
    map.get_mut(key)
}

// ---------------------------------------------------------------------
// valueToString(double) -> "%.17g" with the ".0" fixup.

pub(crate) fn double_to_string(value: f64) -> String {
    if !value.is_finite() {
        // useSpecialFloats == false
        return if value.is_nan() {
            "null".to_owned()
        } else if value < 0.0 {
            "-1e+9999".to_owned()
        } else {
            "1e+9999".to_owned()
        };
    }
    let mut buffer = [0u8; 64];
    let len = unsafe {
        libc::snprintf(
            buffer.as_mut_ptr() as *mut libc::c_char,
            buffer.len(),
            c"%.17g".as_ptr(),
            value,
        )
    };
    let mut text = String::from_utf8_lossy(&buffer[..len as usize]).into_owned();
    // fixNumericLocale
    text = text.replace(',', ".");
    if !text.contains('.') && !text.contains('e') {
        text.push_str(".0");
    }
    text
}

// ---------------------------------------------------------------------
// valueToQuotedStringN (emitUTF8 = false)

fn utf8_to_codepoint(bytes: &[u8], pos: &mut usize) -> u32 {
    const REPLACEMENT: u32 = 0xFFFD;
    let first = u32::from(bytes[*pos]);
    let end = bytes.len();
    if first < 0x80 {
        return first;
    }
    if first < 0xE0 {
        if end - *pos < 2 {
            return REPLACEMENT;
        }
        let calculated = ((first & 0x1F) << 6) | (u32::from(bytes[*pos + 1]) & 0x3F);
        *pos += 1;
        return if calculated < 0x80 {
            REPLACEMENT
        } else {
            calculated
        };
    }
    if first < 0xF0 {
        if end - *pos < 3 {
            return REPLACEMENT;
        }
        let calculated = ((first & 0x0F) << 12)
            | ((u32::from(bytes[*pos + 1]) & 0x3F) << 6)
            | (u32::from(bytes[*pos + 2]) & 0x3F);
        *pos += 2;
        if (0xD800..=0xDFFF).contains(&calculated) {
            return REPLACEMENT;
        }
        return if calculated < 0x800 {
            REPLACEMENT
        } else {
            calculated
        };
    }
    if first < 0xF8 {
        if end - *pos < 4 {
            return REPLACEMENT;
        }
        let calculated = ((first & 0x07) << 18)
            | ((u32::from(bytes[*pos + 1]) & 0x3F) << 12)
            | ((u32::from(bytes[*pos + 2]) & 0x3F) << 6)
            | (u32::from(bytes[*pos + 3]) & 0x3F);
        *pos += 3;
        return if calculated < 0x10000 {
            REPLACEMENT
        } else {
            calculated
        };
    }
    REPLACEMENT
}

fn append_hex(result: &mut Vec<u8>, ch: u32) {
    result.extend_from_slice(b"\\u");
    for byte in [(ch >> 8) & 0xff, ch & 0xff] {
        result.push(b"0123456789abcdef"[(byte >> 4) as usize]);
        result.push(b"0123456789abcdef"[(byte & 0xf) as usize]);
    }
}

fn quoted_string(value: &[u8]) -> Vec<u8> {
    if !value
        .iter()
        .any(|&c| c == b'\\' || c == b'"' || !(0x20..=0x7f).contains(&c))
    {
        let mut out = Vec::with_capacity(value.len() + 2);
        out.push(b'"');
        out.extend_from_slice(value);
        out.push(b'"');
        return out;
    }
    let mut result = Vec::with_capacity(value.len() * 2 + 3);
    result.push(b'"');
    let mut i = 0usize;
    while i < value.len() {
        match value[i] {
            b'"' => result.extend_from_slice(b"\\\""),
            b'\\' => result.extend_from_slice(b"\\\\"),
            0x08 => result.extend_from_slice(b"\\b"),
            0x0c => result.extend_from_slice(b"\\f"),
            b'\n' => result.extend_from_slice(b"\\n"),
            b'\r' => result.extend_from_slice(b"\\r"),
            b'\t' => result.extend_from_slice(b"\\t"),
            _ => {
                let codepoint = utf8_to_codepoint(value, &mut i);
                if codepoint < 0x20 {
                    append_hex(&mut result, codepoint);
                } else if codepoint < 0x80 {
                    result.push(codepoint as u8);
                } else if codepoint < 0x10000 {
                    append_hex(&mut result, codepoint);
                } else {
                    let cp = codepoint - 0x10000;
                    append_hex(&mut result, 0xd800 + ((cp >> 10) & 0x3ff));
                    append_hex(&mut result, 0xdc00 + (cp & 0x3ff));
                }
            }
        }
        i += 1;
    }
    result.push(b'"');
    result
}

// ---------------------------------------------------------------------
// BuiltStyledStreamWriter (indentation="  ", cs=None, colonSymbol=" : ",
// nullSymbol="null", endingLineFeed="", useSpecialFloats=false,
// emitUTF8=false, precision=17, significant)

const INDENTATION: &[u8] = b"  ";
const RIGHT_MARGIN: usize = 74;

struct Writer {
    indentation: &'static [u8],
    multiline_arrays: bool,
    out: Vec<u8>,
    child_values: Vec<Vec<u8>>,
    indent_string: Vec<u8>,
    add_child_values: bool,
    indented: bool,
}

impl Writer {
    fn write(root: &Json) -> Vec<u8> {
        Self::write_indented(root, INDENTATION, false)
    }

    fn write_indented(root: &Json, indentation: &'static [u8], multiline_arrays: bool) -> Vec<u8> {
        let mut writer = Writer {
            indentation,
            multiline_arrays,
            out: Vec::new(),
            child_values: Vec::new(),
            indent_string: Vec::new(),
            add_child_values: false,
            indented: true,
        };
        writer.write_value(root);
        writer.out
    }

    fn write_value(&mut self, value: &Json) {
        match value {
            Json::Null => self.push_value(b"null"),
            Json::Int(v) => {
                let s = v.to_string().into_bytes();
                self.push_value(&s);
            }
            Json::UInt(v) => {
                let s = v.to_string().into_bytes();
                self.push_value(&s);
            }
            Json::Real(v) => {
                let s = double_to_string(*v).into_bytes();
                self.push_value(&s);
            }
            Json::Str(s) => {
                let q = quoted_string(s);
                self.push_value(&q);
            }
            Json::Bool(b) => self.push_value(if *b { b"true" } else { b"false" }),
            Json::Array(items) => self.write_array_value(items),
            Json::Object(map) => {
                if map.is_empty() {
                    self.push_value(b"{}");
                } else {
                    self.write_with_indent(b"{");
                    self.indent();
                    let mut iter = map.iter().peekable();
                    loop {
                        let (name, child) = iter.next().unwrap();
                        let q = quoted_string(name);
                        self.write_with_indent(&q);
                        self.out.extend_from_slice(b" : ");
                        self.write_value(child);
                        if iter.peek().is_none() {
                            break;
                        }
                        self.out.push(b',');
                    }
                    self.unindent();
                    self.write_with_indent(b"}");
                }
            }
        }
    }

    fn write_array_value(&mut self, items: &[Json]) {
        let size = items.len();
        if size == 0 {
            self.push_value(b"[]");
            return;
        }
        if self.multiline_arrays || self.is_multiline_array(items) {
            self.write_with_indent(b"[");
            self.indent();
            let has_child_value = !self.child_values.is_empty();
            for (index, child) in items.iter().enumerate() {
                if has_child_value {
                    let cv = self.child_values[index].clone();
                    self.write_with_indent(&cv);
                } else {
                    if !self.indented {
                        self.write_indent();
                    }
                    self.indented = true;
                    self.write_value(child);
                    self.indented = false;
                }
                if index + 1 == size {
                    break;
                }
                self.out.push(b',');
            }
            self.unindent();
            self.write_with_indent(b"]");
        } else {
            self.out.push(b'[');
            self.out.push(b' ');
            let child_values = std::mem::take(&mut self.child_values);
            for (index, child) in child_values.iter().enumerate() {
                if index > 0 {
                    self.out.extend_from_slice(b", ");
                }
                self.out.extend_from_slice(child);
            }
            self.child_values = child_values;
            self.out.push(b' ');
            self.out.push(b']');
        }
    }

    fn is_multiline_array(&mut self, items: &[Json]) -> bool {
        let size = items.len();
        let mut is_multi_line = size * 3 >= RIGHT_MARGIN;
        self.child_values.clear();
        if !is_multi_line {
            for child in items {
                if matches!(child, Json::Array(_) | Json::Object(_)) && child.size() != 0 {
                    is_multi_line = true;
                    break;
                }
            }
        }
        if !is_multi_line {
            self.add_child_values = true;
            let mut line_length = 4 + (size - 1) * 2;
            for child in items {
                self.write_value(child);
                line_length += self.child_values.last().map_or(0, Vec::len);
            }
            self.add_child_values = false;
            is_multi_line = line_length >= RIGHT_MARGIN;
        }
        is_multi_line
    }

    fn push_value(&mut self, value: &[u8]) {
        if self.add_child_values {
            self.child_values.push(value.to_vec());
        } else {
            self.out.extend_from_slice(value);
        }
    }

    fn write_indent(&mut self) {
        self.out.push(b'\n');
        let indent = self.indent_string.clone();
        self.out.extend_from_slice(&indent);
    }

    fn write_with_indent(&mut self, value: &[u8]) {
        if !self.indented {
            self.write_indent();
        }
        self.out.extend_from_slice(value);
        self.indented = false;
    }

    fn indent(&mut self) {
        self.indent_string.extend_from_slice(self.indentation);
    }

    fn unindent(&mut self) {
        let new_len = self.indent_string.len() - self.indentation.len();
        self.indent_string.truncate(new_len);
    }
}

// ---------------------------------------------------------------------
// OurReader

#[derive(Clone, Copy, PartialEq, Eq)]
enum TokenType {
    EndOfStream,
    ObjectBegin,
    ObjectEnd,
    ArrayBegin,
    ArrayEnd,
    String,
    Number,
    True,
    False,
    Null,
    ArraySeparator,
    MemberSeparator,
    Comment,
    Error,
}

#[derive(Clone, Copy)]
struct Token {
    tt: TokenType,
    start: usize,
    #[allow(dead_code)]
    end: usize,
}

struct JsonError {
    message: String,
    error_path: Option<Vec<String>>,
}

/// Mimic an uncaught `Json::RuntimeError` (`throwRuntimeError`): the
/// process prints the libstdc++ terminate banner and aborts.
fn runtime_error_abort(message: &str) -> ! {
    let _ = writeln!(
        std::io::stderr(),
        "terminate called after throwing an instance of 'Json::RuntimeError'"
    );
    let _ = writeln!(std::io::stderr(), "  what():  {message}");
    std::process::abort();
}

struct Reader<'a> {
    doc: &'a [u8],
    begin: usize,
    end: usize,
    cur: usize,
    errors: Vec<(Token, String, Option<usize>)>,
}

impl<'a> Reader<'a> {
    fn new(doc: &'a [u8]) -> Reader<'a> {
        // skipBom
        let begin = if doc.len() >= 3 && &doc[..3] == b"\xEF\xBB\xBF" {
            3
        } else {
            0
        };
        Reader {
            doc,
            begin,
            end: doc.len(),
            cur: begin,
            errors: Vec::new(),
        }
    }

    fn parse(&mut self) -> Option<Json> {
        let result = self.read_value(1);
        // A trailing token is read for failIfExtra, which is disabled.
        let _ = self.read_token_skipping();
        result
    }

    fn get_next_char(&mut self) -> u8 {
        if self.cur == self.end {
            return 0;
        }
        let c = self.doc[self.cur];
        self.cur += 1;
        c
    }

    fn skip_spaces(&mut self) {
        while self.cur != self.end {
            match self.doc[self.cur] {
                b' ' | b'\t' | b'\r' | b'\n' => self.cur += 1,
                _ => break,
            }
        }
    }

    fn match_(&mut self, pattern: &[u8]) -> bool {
        if self.end - self.cur < pattern.len()
            || &self.doc[self.cur..self.cur + pattern.len()] != pattern
        {
            return false;
        }
        self.cur += pattern.len();
        true
    }

    fn read_token(&mut self) -> (bool, Token) {
        self.skip_spaces();
        let start = self.cur;
        let c = self.get_next_char();
        let mut ok = true;
        let tt = match c {
            b'{' => TokenType::ObjectBegin,
            b'}' => TokenType::ObjectEnd,
            b'[' => TokenType::ArrayBegin,
            b']' => TokenType::ArrayEnd,
            b'"' if self.read_string() => TokenType::String,
            // allowSingleQuotes = false
            b'"' | b'\'' => {
                ok = false;
                TokenType::Error
            }
            b'/' if self.read_comment() => TokenType::Comment,
            b'/' => {
                ok = false;
                TokenType::Error
            }
            b'0'..=b'9' => {
                self.read_number(false);
                TokenType::Number
            }
            b'-' => {
                if self.read_number(true) {
                    TokenType::Number
                } else {
                    // tokenNegInf; allowSpecialFloats = false
                    ok = false;
                    TokenType::Error
                }
            }
            b'+' => {
                if self.read_number(true) {
                    TokenType::Number
                } else {
                    // tokenPosInf; allowSpecialFloats = false
                    ok = false;
                    TokenType::Error
                }
            }
            b't' => {
                ok = self.match_(b"rue");
                TokenType::True
            }
            b'f' => {
                ok = self.match_(b"alse");
                TokenType::False
            }
            b'n' => {
                ok = self.match_(b"ull");
                TokenType::Null
            }
            // tokenNaN / tokenPosInf; allowSpecialFloats = false
            b'N' | b'I' => {
                ok = false;
                TokenType::Error
            }
            b',' => TokenType::ArraySeparator,
            b':' => TokenType::MemberSeparator,
            0 => TokenType::EndOfStream,
            _ => {
                ok = false;
                TokenType::Error
            }
        };
        let tt = if ok { tt } else { TokenType::Error };
        (
            ok,
            Token {
                tt,
                start,
                end: self.cur,
            },
        )
    }

    fn read_token_skipping(&mut self) -> (bool, Token) {
        // allowComments = true
        loop {
            let (ok, token) = self.read_token();
            if !ok || token.tt != TokenType::Comment {
                return (ok, token);
            }
        }
    }

    fn read_string(&mut self) -> bool {
        let mut c = 0u8;
        while self.cur != self.end {
            c = self.get_next_char();
            if c == b'\\' {
                self.get_next_char();
            } else if c == b'"' {
                break;
            }
        }
        c == b'"'
    }

    fn read_comment(&mut self) -> bool {
        match self.get_next_char() {
            b'*' => self.read_c_style_comment(),
            b'/' => self.read_cpp_style_comment(),
            _ => false,
        }
    }

    fn read_c_style_comment(&mut self) -> bool {
        while self.cur + 1 < self.end {
            let c = self.get_next_char();
            if c == b'*' && self.doc[self.cur] == b'/' {
                break;
            }
        }
        self.get_next_char() == b'/'
    }

    fn read_cpp_style_comment(&mut self) -> bool {
        while self.cur != self.end {
            let c = self.get_next_char();
            if c == b'\n' {
                break;
            }
            if c == b'\r' {
                if self.cur != self.end && self.doc[self.cur] == b'\n' {
                    self.get_next_char();
                }
                break;
            }
        }
        true
    }

    fn read_number(&mut self, check_inf: bool) -> bool {
        let mut p = self.cur;
        if check_inf && p != self.end && self.doc[p] == b'I' {
            p += 1;
            self.cur = p;
            return false;
        }
        let mut c = b'0'; // stopgap for already consumed character
        while c.is_ascii_digit() {
            self.cur = p;
            c = if p < self.end { self.doc[p] } else { 0 };
            p += 1;
        }
        if c == b'.' {
            self.cur = p;
            c = if p < self.end { self.doc[p] } else { 0 };
            p += 1;
            while c.is_ascii_digit() {
                self.cur = p;
                c = if p < self.end { self.doc[p] } else { 0 };
                p += 1;
            }
        }
        if c == b'e' || c == b'E' {
            self.cur = p;
            c = if p < self.end { self.doc[p] } else { 0 };
            p += 1;
            if c == b'+' || c == b'-' {
                self.cur = p;
                c = if p < self.end { self.doc[p] } else { 0 };
                p += 1;
            }
            while c.is_ascii_digit() {
                self.cur = p;
                c = if p < self.end { self.doc[p] } else { 0 };
                p += 1;
            }
        }
        true
    }

    fn read_value(&mut self, depth: usize) -> Option<Json> {
        // nodes_.size() > stackLimit_ (1000)
        if depth > 1000 {
            runtime_error_abort("Exceeded stackLimit in readValue().");
        }
        let (_, token) = self.read_token_skipping();
        match token.tt {
            TokenType::ObjectBegin => self.read_object(depth),
            TokenType::ArrayBegin => self.read_array(depth),
            TokenType::Number => self.decode_number(&token),
            TokenType::String => self.decode_string(&token).map(Json::Str),
            TokenType::True => Some(Json::Bool(true)),
            TokenType::False => Some(Json::Bool(false)),
            TokenType::Null => Some(Json::Null),
            _ => {
                self.add_error(
                    "Syntax error: value, object or array expected.",
                    &token,
                    None,
                );
                None
            }
        }
    }

    fn read_object(&mut self, depth: usize) -> Option<Json> {
        let mut map: BTreeMap<Vec<u8>, Json> = BTreeMap::new();
        loop {
            let (ok, token_name) = self.read_token_skipping();
            if !ok {
                self.add_error_and_recover(
                    "Missing '}' or object member name",
                    &token_name,
                    TokenType::ObjectEnd,
                );
                return None;
            }
            // `tokenObjectEnd && (name.empty() || allowTrailingCommas)`:
            // allowTrailingCommas is enabled so '}' always closes.
            if token_name.tt == TokenType::ObjectEnd {
                return Some(Json::Object(map));
            }
            let name = if token_name.tt == TokenType::String {
                match self.decode_string(&token_name) {
                    Some(decoded) => decoded,
                    None => {
                        self.recover_from_error(TokenType::ObjectEnd);
                        return None;
                    }
                }
            } else {
                // allowNumericKeys = false
                self.add_error_and_recover(
                    "Missing '}' or object member name",
                    &token_name,
                    TokenType::ObjectEnd,
                );
                return None;
            };
            if name.len() >= (1usize << 30) {
                runtime_error_abort("keylength >= 2^30");
            }
            let (ok, colon) = self.read_token();
            if !ok || colon.tt != TokenType::MemberSeparator {
                self.add_error_and_recover(
                    "Missing ':' after object member name",
                    &colon,
                    TokenType::ObjectEnd,
                );
                return None;
            }
            let Some(value) = self.read_value(depth + 1) else {
                self.recover_from_error(TokenType::ObjectEnd);
                return None;
            };
            // std::map keeps the first equivalent key's name on insert.
            if let Some(slot) = map.get_mut(&name) {
                *slot = value;
            } else {
                map.insert(name, value);
            }
            let (ok, comma) = self.read_token_skipping();
            if !ok || (comma.tt != TokenType::ObjectEnd && comma.tt != TokenType::ArraySeparator) {
                self.add_error_and_recover(
                    "Missing ',' or '}' in object declaration",
                    &comma,
                    TokenType::ObjectEnd,
                );
                return None;
            }
            if comma.tt == TokenType::ObjectEnd {
                return Some(Json::Object(map));
            }
        }
    }

    fn read_array(&mut self, depth: usize) -> Option<Json> {
        let mut items: Vec<Json> = Vec::new();
        loop {
            self.skip_spaces();
            // ']' closes at index 0 and (with allowTrailingCommas and without
            // allowDroppedNullPlaceholders) after a value.
            if self.cur != self.end && self.doc[self.cur] == b']' {
                // readToken(endArray) consumes ']'
                self.cur += 1;
                return Some(Json::Array(items));
            }
            let Some(value) = self.read_value(depth + 1) else {
                self.recover_from_error(TokenType::ArrayEnd);
                return None;
            };
            items.push(value);
            let (ok, token) = self.read_token_skipping();
            if !ok || (token.tt != TokenType::ArraySeparator && token.tt != TokenType::ArrayEnd) {
                self.add_error_and_recover(
                    "Missing ',' or ']' in array declaration",
                    &token,
                    TokenType::ArrayEnd,
                );
                return None;
            }
            if token.tt == TokenType::ArrayEnd {
                return Some(Json::Array(items));
            }
        }
    }

    fn decode_number(&mut self, token: &Token) -> Option<Json> {
        let text = &self.doc[token.start..token.end];
        let is_negative = text.first() == Some(&b'-');
        let digits = if is_negative { &text[1..] } else { text };
        if digits.iter().all(u8::is_ascii_digit) {
            const POSITIVE_THRESHOLD: u64 = u64::MAX / 10;
            const POSITIVE_LAST: u64 = u64::MAX % 10;
            const NEGATIVE_THRESHOLD: u64 = (-(i64::MIN / 10)) as u64;
            const NEGATIVE_LAST: u64 = (-(i64::MIN % 10)) as u64;
            let (threshold, max_last_digit) = if is_negative {
                (NEGATIVE_THRESHOLD, NEGATIVE_LAST)
            } else {
                (POSITIVE_THRESHOLD, POSITIVE_LAST)
            };
            let mut value: u64 = 0;
            let mut as_double = false;
            for (i, &c) in digits.iter().enumerate() {
                let digit = u64::from(c - b'0');
                if value >= threshold
                    && (value > threshold || i + 1 != digits.len() || digit > max_last_digit)
                {
                    as_double = true;
                    break;
                }
                value = value * 10 + digit;
            }
            if !as_double {
                return Some(if is_negative {
                    // -(LargestInt(value / 10) * 10) - last_digit
                    let last_digit = value % 10;
                    Json::Int(-((value / 10) as i64) * 10 - last_digit as i64)
                } else if value <= i64::MAX as u64 {
                    Json::Int(value as i64)
                } else {
                    Json::UInt(value)
                });
            }
        }
        self.decode_double(token)
    }

    fn decode_double(&mut self, token: &Token) -> Option<Json> {
        let text = &self.doc[token.start..token.end];
        // iostream `>> double`: overflow yields infinities, garbage fails.
        match std::str::from_utf8(text)
            .ok()
            .and_then(|s| s.parse::<f64>().ok())
        {
            Some(value) => Some(Json::Real(value)),
            None => {
                self.add_error(
                    &format!("'{}' is not a number.", crate::bytes::decode(text)),
                    token,
                    None,
                );
                None
            }
        }
    }

    fn decode_string(&mut self, token: &Token) -> Option<Vec<u8>> {
        let mut decoded: Vec<u8> = Vec::with_capacity(token.end.saturating_sub(token.start + 2));
        let mut current = token.start + 1;
        let end = token.end.saturating_sub(1);
        while current != end {
            let c = self.doc[current];
            current += 1;
            if c == b'"' {
                break;
            }
            if c != b'\\' {
                decoded.push(c);
                continue;
            }
            if current == end {
                self.add_error("Empty escape sequence in string", token, Some(current));
                return None;
            }
            let escape = self.doc[current];
            current += 1;
            match escape {
                b'"' => decoded.push(b'"'),
                b'/' => decoded.push(b'/'),
                b'\\' => decoded.push(b'\\'),
                b'b' => decoded.push(0x08),
                b'f' => decoded.push(0x0c),
                b'n' => decoded.push(b'\n'),
                b'r' => decoded.push(b'\r'),
                b't' => decoded.push(b'\t'),
                b'u' => {
                    let unicode = self.decode_unicode_codepoint(token, &mut current, end)?;
                    decoded.extend_from_slice(&codepoint_to_utf8(unicode));
                }
                _ => {
                    self.add_error("Bad escape sequence in string", token, Some(current));
                    return None;
                }
            }
        }
        Some(decoded)
    }

    fn decode_unicode_codepoint(
        &mut self,
        token: &Token,
        current: &mut usize,
        end: usize,
    ) -> Option<u32> {
        let mut unicode = self.decode_unicode_escape_sequence(token, current, end)?;
        if (0xD800..=0xDBFF).contains(&unicode) {
            if end - *current < 6 {
                self.add_error(
                    "additional six characters expected to parse unicode surrogate pair.",
                    token,
                    Some(*current),
                );
                return None;
            }
            if self.doc[*current] == b'\\' && self.doc[*current + 1] == b'u' {
                *current += 2;
                let pair = self.decode_unicode_escape_sequence(token, current, end)?;
                unicode = 0x10000 + ((unicode & 0x3FF) << 10) + (pair & 0x3FF);
            } else {
                self.add_error(
                    "expecting another \\u token to begin the second half of a unicode \
                     surrogate pair",
                    token,
                    Some(*current),
                );
                return None;
            }
        }
        Some(unicode)
    }

    fn decode_unicode_escape_sequence(
        &mut self,
        token: &Token,
        current: &mut usize,
        end: usize,
    ) -> Option<u32> {
        if end - *current < 4 {
            self.add_error(
                "Bad unicode escape sequence in string: four digits expected.",
                token,
                Some(*current),
            );
            return None;
        }
        let mut unicode: u32 = 0;
        for _ in 0..4 {
            let c = self.doc[*current];
            *current += 1;
            unicode *= 16;
            match c {
                b'0'..=b'9' => unicode += u32::from(c - b'0'),
                b'a'..=b'f' => unicode += u32::from(c - b'a' + 10),
                b'A'..=b'F' => unicode += u32::from(c - b'A' + 10),
                _ => {
                    self.add_error(
                        "Bad unicode escape sequence in string: hexadecimal digit \
                         expected.",
                        token,
                        Some(*current),
                    );
                    return None;
                }
            }
        }
        Some(unicode)
    }

    fn add_error(&mut self, message: &str, token: &Token, extra: Option<usize>) {
        self.errors.push((*token, message.to_owned(), extra));
    }

    fn recover_from_error(&mut self, skip_until: TokenType) {
        let error_count = self.errors.len();
        loop {
            let (ok, skip) = self.read_token();
            if !ok {
                self.errors.truncate(error_count);
            }
            if skip.tt == skip_until || skip.tt == TokenType::EndOfStream {
                break;
            }
        }
        self.errors.truncate(error_count);
    }

    fn add_error_and_recover(&mut self, message: &str, token: &Token, skip_until: TokenType) {
        self.add_error(message, token, None);
        self.recover_from_error(skip_until);
    }

    fn location_line_and_column(&self, location: usize) -> (i32, i32) {
        let mut current = self.begin;
        let mut last_line_start = current;
        let mut line = 0i32;
        while current < location && current != self.end {
            let c = self.doc[current];
            current += 1;
            if c == b'\r' {
                if current != self.end && self.doc[current] == b'\n' {
                    current += 1;
                }
                last_line_start = current;
                line += 1;
            } else if c == b'\n' {
                last_line_start = current;
                line += 1;
            }
        }
        // column & line start at 1
        (line + 1, (location - last_line_start) as i32 + 1)
    }

    fn formatted_error_messages(&self) -> String {
        let mut formatted = String::new();
        for (token, message, extra) in &self.errors {
            let (line, column) = self.location_line_and_column(token.start);
            formatted += &format!("* Line {line}, Column {column}\n");
            formatted += &format!("  {message}\n");
            if let Some(extra) = extra {
                let (line, column) = self.location_line_and_column(*extra);
                formatted += &format!("See Line {line}, Column {column} for detail.\n");
            }
        }
        formatted
    }
}

fn codepoint_to_utf8(cp: u32) -> Vec<u8> {
    if cp <= 0x7f {
        vec![cp as u8]
    } else if cp <= 0x7FF {
        vec![0xC0 | (0x1f & (cp >> 6)) as u8, 0x80 | (0x3f & cp) as u8]
    } else if cp <= 0xFFFF {
        vec![
            0xE0 | (0xf & (cp >> 12)) as u8,
            0x80 | (0x3f & (cp >> 6)) as u8,
            0x80 | (0x3f & cp) as u8,
        ]
    } else if cp <= 0x10FFFF {
        vec![
            0xF0 | (0x7 & (cp >> 18)) as u8,
            0x80 | (0x3f & (cp >> 12)) as u8,
            0x80 | (0x3f & (cp >> 6)) as u8,
            0x80 | (0x3f & cp) as u8,
        ]
    } else {
        Vec::new()
    }
}

/// Parse a document with the jsoncpp-compatible reader. On failure the error
/// message is `getFormattedErrorMessages`-compatible.
pub(crate) fn parse_jsoncpp(doc: &[u8]) -> Result<Json, String> {
    let mut reader = Reader::new(doc);
    match reader.parse() {
        Some(json) => Ok(json),
        None => Err(reader.formatted_error_messages()),
    }
}

fn read_json(jsonstr: &str) -> Result<Json, JsonError> {
    let encoded = crate::bytes::encode(jsonstr);
    let mut reader = Reader::new(&encoded);
    match reader.parse() {
        Some(json) => Ok(json),
        None => Err(JsonError {
            message: format!(
                "failed parsing json string: {}",
                reader.formatted_error_messages()
            ),
            error_path: None,
        }),
    }
}

/// `operator<<(std::ostream&, Json::Value const&)` with the builder's default
/// `commentStyle = "All"`: every nonempty array goes on multiple lines.
pub fn write_json_ostream(value: &Json) -> String {
    crate::bytes::decode(&Writer::write_indented(value, b"\t", true))
}

/// Serialize generator metadata with the same writer as jsoncpp's operator<<.
pub(crate) fn stream_value(value: &serde_json::Value) -> String {
    fn convert(v: &serde_json::Value) -> Json {
        use serde_json::Value;
        match v {
            Value::Null => Json::Null,
            Value::Bool(b) => Json::Bool(*b),
            Value::Number(n) => n
                .as_i64()
                .map(Json::Int)
                .or_else(|| n.as_u64().map(Json::UInt))
                .unwrap_or_else(|| Json::Real(n.as_f64().unwrap())),
            Value::String(s) => Json::Str(crate::bytes::encode(s).into_owned()),
            Value::Array(a) => Json::Array(a.iter().map(convert).collect()),
            Value::Object(o) => Json::Object(
                o.iter()
                    .map(|(k, v)| (crate::bytes::encode(k).into_owned(), convert(v)))
                    .collect(),
            ),
        }
    }
    // StreamWriterBuilder's default commentStyle=All sets every nonempty
    // array on multiple lines, including single-element path lists.
    crate::bytes::decode(&Writer::write_indented(&convert(value), b"\t", true))
}

/// `Json::StreamWriterBuilder` with `indentation = "  "` and
/// `commentStyle = "None"`, as used by
/// `cmExportPackageInfoGenerator::WritePackageInfo`: two-space indent,
/// `"key" : value`, and no trailing newline (StreamWriter does not emit
/// one; `cmGeneratedFileStream` does not add one either).
pub fn write_json_indent2(value: &Json) -> String {
    crate::bytes::decode(&Writer::write_indented(value, b"  ", false))
}

fn write_json(value: &Json) -> String {
    crate::bytes::decode(&Writer::write(value))
}

/// `Json::writeString` with `indentation = ""`: BuiltStyledStreamWriter
/// writes no newlines and uses ":" and "," without spaces.
pub fn write_json_compact(value: &Json) -> String {
    fn write(value: &Json, out: &mut Vec<u8>) {
        match value {
            Json::Null => out.extend_from_slice(b"null"),
            Json::Int(v) => out.extend_from_slice(v.to_string().as_bytes()),
            Json::UInt(v) => out.extend_from_slice(v.to_string().as_bytes()),
            Json::Real(v) => out.extend_from_slice(double_to_string(*v).as_bytes()),
            Json::Str(s) => out.extend_from_slice(&quoted_string(s)),
            Json::Bool(b) => out.extend_from_slice(if *b { b"true" } else { b"false" }),
            Json::Array(items) => {
                if items.is_empty() {
                    out.extend_from_slice(b"[]");
                    return;
                }
                out.push(b'[');
                for (i, item) in items.iter().enumerate() {
                    if i > 0 {
                        out.push(b',');
                    }
                    write(item, out);
                }
                out.push(b']');
            }
            Json::Object(map) => {
                if map.is_empty() {
                    out.extend_from_slice(b"{}");
                    return;
                }
                out.push(b'{');
                for (i, (name, child)) in map.iter().enumerate() {
                    if i > 0 {
                        out.push(b',');
                    }
                    out.extend_from_slice(&quoted_string(name));
                    out.push(b':');
                    write(child, out);
                }
                out.push(b'}');
            }
        }
    }
    let mut out = Vec::new();
    write(value, &mut out);
    crate::bytes::decode(&out)
}

// ---------------------------------------------------------------------
// HandleJSONCommand

struct JsonArgs<'a> {
    items: &'a [String],
    lo: usize,
    hi: usize,
}

impl<'a> JsonArgs<'a> {
    fn is_empty(&self) -> bool {
        self.lo >= self.hi
    }
    fn first(&self) -> &String {
        &self.items[self.lo]
    }
    fn pop_front(&mut self, error: &str) -> Result<&'a String, JsonError> {
        if self.is_empty() {
            return Err(JsonError {
                message: error.to_owned(),
                error_path: None,
            });
        }
        let v = &self.items[self.lo];
        self.lo += 1;
        Ok(v)
    }
    fn pop_back(&mut self, error: &str) -> Result<&'a String, JsonError> {
        if self.is_empty() {
            return Err(JsonError {
                message: error.to_owned(),
                error_path: None,
            });
        }
        self.hi -= 1;
        Ok(&self.items[self.hi])
    }
    fn remaining(&self) -> &[String] {
        &self.items[self.lo..self.hi]
    }
    /// `Args{args.begin(), args.end() + 1}`: the remaining args plus the one
    /// just popped off the back.
    fn extended(&self) -> &[String] {
        &self.items[self.lo..self.hi + 1]
    }
}

fn parse_index(text: &str, progress: &[String], max: u32) -> Result<u32, JsonError> {
    let Some(lindex) = str_to_ulong(text) else {
        return Err(JsonError {
            message: format!("expected an array index, got: '{text}'"),
            error_path: Some(progress.to_vec()),
        });
    };
    // `static_cast<Json::ArrayIndex>` truncates to 32 bits.
    let index = lindex as u32;
    if index >= max {
        return Err(JsonError {
            message: format!("expected an index less than {max} got '{text}'"),
            error_path: Some(progress.to_vec()),
        });
    }
    Ok(index)
}

fn resolve_path<'a>(json: &'a mut Json, path: &[String]) -> Result<&'a mut Json, JsonError> {
    let mut search: &mut Json = json;
    for (i, field) in path.iter().enumerate() {
        let progress = &path[..=i];
        match search {
            Json::Array(items) => {
                let index = parse_index(field, progress, items.len() as u32)?;
                search = &mut items[index as usize];
            }
            Json::Object(map) => {
                if !map.contains_key(crate::bytes::encode(field).as_ref()) {
                    return Err(JsonError {
                        message: format!("member '{}' not found", cm_join(progress, " ", "")),
                        error_path: Some(progress.to_vec()),
                    });
                }
                search = object_get_mut(map, crate::bytes::encode(field).as_ref()).unwrap();
            }
            other => {
                return Err(JsonError {
                    message: format!(
                        "invalid path '{}', need element of OBJECT or ARRAY type to \
                         lookup '{}' got {}",
                        cm_join(progress, " ", ""),
                        field,
                        other.type_name()
                    ),
                    error_path: Some(progress.to_vec()),
                });
            }
        }
    }
    Ok(search)
}

fn set_def_bytes(interp: &mut Interp, name: &str, value: Vec<u8>) {
    interp.set_def(name.to_owned(), crate::bytes::decode(&value));
}

pub fn handle_json(interp: &mut Interp, args: &[String]) -> CmdResult {
    let mut args = JsonArgs {
        items: &args[1..],
        lo: 0,
        hi: args.len() - 1,
    };

    let mut error_variable: Option<String> = None;
    let mut output_variable: Option<String> = None;

    let result: Result<(), JsonError> = (|interp: &mut Interp| {
        output_variable = Some(args.pop_front("missing out-var argument")?.clone());

        if !args.is_empty() && args.first() == "ERROR_VARIABLE" {
            let _ = args.pop_front("");
            let var = args.pop_front("missing error-var argument")?.clone();
            interp.set_def(var.clone(), "NOTFOUND");
            error_variable = Some(var);
        }

        let mode = args.pop_front("missing mode argument")?.clone();
        if !matches!(
            mode.as_str(),
            "GET" | "TYPE" | "MEMBER" | "LENGTH" | "REMOVE" | "SET" | "EQUAL"
        ) {
            return Err(JsonError {
                message: format!(
                    "got an invalid mode '{mode}', expected one of GET, TYPE, MEMBER, \
                     LENGTH, REMOVE, SET, EQUAL"
                ),
                error_path: None,
            });
        }

        let jsonstr = args.pop_front("missing json string argument")?.clone();
        let mut json = read_json(&jsonstr)?;

        match mode.as_str() {
            "GET" => {
                let value = resolve_path(&mut json, args.remaining())?;
                match value {
                    Json::Object(_) | Json::Array(_) => {
                        let text = write_json(value);
                        interp.set_def(output_variable.clone().unwrap(), text);
                    }
                    Json::Bool(b) => {
                        interp.set_def(
                            output_variable.clone().unwrap(),
                            if *b { "ON" } else { "OFF" },
                        );
                    }
                    other => {
                        let s = other.as_string();
                        set_def_bytes(interp, &output_variable.clone().unwrap(), s);
                    }
                }
            }
            "TYPE" => {
                let value = resolve_path(&mut json, args.remaining())?;
                interp.set_def(output_variable.clone().unwrap(), value.type_name());
            }
            "MEMBER" => {
                let index_str = args.pop_back("missing member index")?.clone();
                let ext = args.extended().to_vec();
                let value = resolve_path(&mut json, args.remaining())?;
                let Json::Object(map) = value else {
                    return Err(JsonError {
                        message: format!(
                            "MEMBER needs to be called with an element of type \
                             OBJECT, got {}",
                            value.type_name()
                        ),
                        error_path: Some(args.remaining().to_vec()),
                    });
                };
                let index = parse_index(&index_str, &ext, map.len() as u32)?;
                let name = map.keys().nth(index as usize).unwrap();
                set_def_bytes(interp, &output_variable.clone().unwrap(), name.clone());
            }
            "LENGTH" => {
                let value = resolve_path(&mut json, args.remaining())?;
                if !matches!(value, Json::Array(_) | Json::Object(_)) {
                    return Err(JsonError {
                        message: format!(
                            "LENGTH needs to be called with an element of type ARRAY \
                             or OBJECT, got {}",
                            value.type_name()
                        ),
                        error_path: Some(args.remaining().to_vec()),
                    });
                }
                interp.set_def(output_variable.clone().unwrap(), value.size().to_string());
            }
            "REMOVE" => {
                let to_remove = args.pop_back("missing member or index to remove")?.clone();
                let ext = args.extended().to_vec();
                let value = resolve_path(&mut json, args.remaining())?;
                match value {
                    Json::Array(items) => {
                        let index = parse_index(&to_remove, &ext, items.len() as u32)? as usize;
                        if index < items.len() {
                            items.remove(index);
                        }
                    }
                    Json::Object(map) => {
                        map.remove(crate::bytes::encode(&to_remove).as_ref());
                    }
                    other => {
                        return Err(JsonError {
                            message: format!(
                                "REMOVE needs to be called with an element of type \
                                 ARRAY or OBJECT, got {}",
                                other.type_name()
                            ),
                            error_path: Some(args.remaining().to_vec()),
                        });
                    }
                }
                interp.set_def(output_variable.clone().unwrap(), write_json(&json));
            }
            "SET" => {
                let new_value_str = args.pop_back("missing new value remove")?.clone();
                let to_add = args.pop_back("missing member name to add")?.clone();
                let ext = args.extended().to_vec();
                let value = resolve_path(&mut json, args.remaining())?;
                let new_value = read_json(&new_value_str)?;
                match value {
                    Json::Object(map) => {
                        let key = crate::bytes::encode(&to_add).into_owned();
                        if let Some(slot) = map.get_mut(&key) {
                            *slot = new_value;
                        } else {
                            map.insert(key, new_value);
                        }
                    }
                    Json::Array(items) => {
                        let index = parse_index(&to_add, &ext, u32::MAX)? as usize;
                        if index < items.len() {
                            items[index] = new_value;
                        } else {
                            items.push(new_value);
                        }
                    }
                    other => {
                        return Err(JsonError {
                            message: format!(
                                "SET needs to be called with an element of type OBJECT \
                                 or ARRAY, got {}",
                                other.type_name()
                            ),
                            error_path: None,
                        });
                    }
                }
                interp.set_def(output_variable.clone().unwrap(), write_json(&json));
            }
            "EQUAL" => {
                let jsonstr2 = args
                    .pop_front("missing second json string argument")?
                    .clone();
                let json2 = read_json(&jsonstr2)?;
                interp.set_def(
                    output_variable.clone().unwrap(),
                    if json.json_eq(&json2) { "ON" } else { "OFF" },
                );
            }
            _ => unreachable!(),
        }
        Ok(())
    })(interp);

    if let Err(e) = result {
        if let (Some(out), Some(path)) = (&output_variable, &e.error_path) {
            interp.set_def(out.clone(), format!("{}-NOTFOUND", cm_join(path, "-", "")));
        } else if let Some(out) = &output_variable {
            interp.set_def(out.clone(), "NOTFOUND");
        }

        if let Some(var) = &error_variable {
            interp.set_def(var.clone(), e.message);
        } else {
            return err(format!("sub-command JSON {}.", e.message));
        }
    }
    Ok(())
}
