//! `cmJSONState` and `cmJSONHelpers` (cmJSONState.cxx, cmJSONHelpers.h/cxx)
//! on top of a port of jsoncpp 1.9.6's `OurReader` configured by
//! `CharReaderBuilder::strictMode`, keeping each value's document offsets
//! for error locations.

use std::collections::BTreeMap;
use std::rc::Rc;

use crate::commands::string::json::double_to_string;

// ---------------------------------------------------------------------
// Json::Value (with offsets)

#[derive(Debug)]
pub enum Kind {
    Null,
    Int(i64),
    UInt(u64),
    Real(f64),
    Str(String),
    Bool(bool),
    Array(Vec<V>),
    Object(BTreeMap<String, V>),
}

#[derive(Debug)]
pub struct Value {
    pub kind: Kind,
    /// `getOffsetStart`.
    pub start: usize,
}

pub type V = Rc<Value>;

impl Value {
    pub fn is_null(&self) -> bool {
        matches!(self.kind, Kind::Null)
    }
    pub fn is_bool(&self) -> bool {
        matches!(self.kind, Kind::Bool(_))
    }
    pub fn is_string(&self) -> bool {
        matches!(self.kind, Kind::Str(_))
    }
    pub fn is_object(&self) -> bool {
        matches!(self.kind, Kind::Object(_))
    }
    pub fn is_array(&self) -> bool {
        matches!(self.kind, Kind::Array(_))
    }

    /// `Value::isInt`.
    pub fn is_int(&self) -> bool {
        match self.kind {
            Kind::Int(v) => v >= i64::from(i32::MIN) && v <= i64::from(i32::MAX),
            Kind::UInt(v) => v <= i32::MAX as u64,
            Kind::Real(v) => {
                v >= f64::from(i32::MIN) && v <= f64::from(i32::MAX) && v.fract() == 0.0
            }
            _ => false,
        }
    }

    /// `Value::isUInt`.
    pub fn is_uint(&self) -> bool {
        match self.kind {
            Kind::Int(v) => v >= 0 && v <= i64::from(u32::MAX),
            Kind::UInt(v) => v <= u64::from(u32::MAX),
            Kind::Real(v) => v >= 0.0 && v <= f64::from(u32::MAX) && v.fract() == 0.0,
            _ => false,
        }
    }

    pub fn as_int(&self) -> i32 {
        match self.kind {
            Kind::Int(v) => v as i32,
            Kind::UInt(v) => v as i32,
            Kind::Real(v) => v as i32,
            Kind::Bool(b) => i32::from(b),
            _ => 0,
        }
    }

    pub fn as_uint(&self) -> u32 {
        match self.kind {
            Kind::Int(v) => v as u32,
            Kind::UInt(v) => v as u32,
            Kind::Real(v) => v as u32,
            Kind::Bool(b) => u32::from(b),
            _ => 0,
        }
    }

    pub fn as_bool(&self) -> bool {
        match self.kind {
            Kind::Bool(b) => b,
            Kind::Int(v) => v != 0,
            Kind::UInt(v) => v != 0,
            Kind::Real(v) => v != 0.0,
            _ => false,
        }
    }

    /// `Value::isConvertibleTo(stringValue)`.
    pub fn is_convertible_to_string(&self) -> bool {
        !matches!(self.kind, Kind::Array(_) | Kind::Object(_))
    }

    /// `Value::asString` for convertible values.
    pub fn as_string(&self) -> String {
        match &self.kind {
            Kind::Null => String::new(),
            Kind::Str(s) => s.clone(),
            Kind::Bool(b) => if *b { "true" } else { "false" }.to_owned(),
            Kind::Int(v) => v.to_string(),
            Kind::UInt(v) => v.to_string(),
            Kind::Real(v) => double_to_string(*v),
            Kind::Array(_) | Kind::Object(_) => String::new(),
        }
    }

    pub fn is_member(&self, name: &str) -> bool {
        match &self.kind {
            Kind::Object(map) => map.contains_key(name),
            _ => false,
        }
    }

    /// `operator[]` on a const value: missing members read as null, which
    /// error reporting treats the same as no value.
    pub fn member(&self, name: &str) -> Option<V> {
        match &self.kind {
            Kind::Object(map) => map.get(name).cloned(),
            _ => None,
        }
    }

    /// `getMemberNames` (sorted).
    pub fn member_names(&self) -> Vec<String> {
        match &self.kind {
            Kind::Object(map) => map.keys().cloned().collect(),
            _ => Vec::new(),
        }
    }

    pub fn items(&self) -> &[V] {
        match &self.kind {
            Kind::Array(items) => items,
            _ => &[],
        }
    }
}

// ---------------------------------------------------------------------
// OurReader with strictMode features:
// allowComments=false, allowTrailingCommas=false, strictRoot=true,
// allowDroppedNullPlaceholders=false, allowNumericKeys=false,
// allowSingleQuotes=false, stackLimit=1000, failIfExtra=true,
// rejectDupKeys=true, allowSpecialFloats=false.

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
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

#[derive(Clone, Copy, Debug)]
struct Token {
    tt: TokenType,
    start: usize,
    end: usize,
}

/// `OurFeatures` settings that differ between the readers CMake builds.
#[derive(Clone, Copy)]
pub struct Features {
    pub allow_comments: bool,
    pub allow_trailing_commas: bool,
    pub strict_root: bool,
    pub fail_if_extra: bool,
    pub reject_dup_keys: bool,
}

impl Features {
    /// `CharReaderBuilder::strictMode`.
    pub const STRICT: Features = Features {
        allow_comments: false,
        allow_trailing_commas: false,
        strict_root: true,
        fail_if_extra: true,
        reject_dup_keys: true,
    };
}

struct Reader<'a> {
    doc: &'a [u8],
    cur: usize,
    depth: usize,
    features: Features,
    /// (offset_start, message, extra offset)
    errors: Vec<(usize, String, Option<usize>)>,
}

impl<'a> Reader<'a> {
    fn get_next_char(&mut self) -> u8 {
        if self.cur == self.doc.len() {
            return 0;
        }
        let c = self.doc[self.cur];
        self.cur += 1;
        c
    }

    fn skip_spaces(&mut self) {
        while self.cur != self.doc.len() {
            match self.doc[self.cur] {
                b' ' | b'\t' | b'\r' | b'\n' => self.cur += 1,
                _ => break,
            }
        }
    }

    fn match_(&mut self, pattern: &[u8]) -> bool {
        if self.doc.len() - self.cur < pattern.len()
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
        let mut tt = match c {
            b'{' => TokenType::ObjectBegin,
            b'}' => TokenType::ObjectEnd,
            b'[' => TokenType::ArrayBegin,
            b']' => TokenType::ArrayEnd,
            b'"' => {
                ok = self.read_string();
                TokenType::String
            }
            b'\'' => {
                ok = false;
                TokenType::Error
            }
            b'/' => {
                ok = self.read_comment();
                TokenType::Comment
            }
            b'0'..=b'9' => {
                self.read_number(false);
                TokenType::Number
            }
            b'-' | b'+' if self.read_number(true) => TokenType::Number,
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
            b',' => TokenType::ArraySeparator,
            b':' => TokenType::MemberSeparator,
            0 => TokenType::EndOfStream,
            _ => {
                ok = false;
                TokenType::Error
            }
        };
        if !ok {
            tt = TokenType::Error;
        }
        (
            ok,
            Token {
                tt,
                start,
                end: self.cur,
            },
        )
    }

    /// `readTokenSkippingComments`.
    fn read_token_skipping(&mut self) -> (bool, Token) {
        let (mut ok, mut token) = self.read_token();
        if self.features.allow_comments {
            while ok && token.tt == TokenType::Comment {
                (ok, token) = self.read_token();
            }
        }
        (ok, token)
    }

    fn read_string(&mut self) -> bool {
        let mut c = 0u8;
        while self.cur != self.doc.len() {
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
            b'*' => {
                while self.cur + 1 < self.doc.len() {
                    let c = self.get_next_char();
                    if c == b'*' && self.doc[self.cur] == b'/' {
                        break;
                    }
                }
                self.get_next_char() == b'/'
            }
            b'/' => {
                while self.cur != self.doc.len() {
                    let c = self.get_next_char();
                    if c == b'\n' {
                        break;
                    }
                    if c == b'\r' {
                        if self.cur != self.doc.len() && self.doc[self.cur] == b'\n' {
                            self.get_next_char();
                        }
                        break;
                    }
                }
                true
            }
            _ => false,
        }
    }

    fn peek(&self, p: usize) -> u8 {
        if p < self.doc.len() { self.doc[p] } else { 0 }
    }

    fn read_number(&mut self, check_inf: bool) -> bool {
        let mut p = self.cur;
        if check_inf && p != self.doc.len() && self.doc[p] == b'I' {
            self.cur = p + 1;
            return false;
        }
        let mut c = b'0';
        while c.is_ascii_digit() {
            self.cur = p;
            c = self.peek(p);
            p += 1;
        }
        if c == b'.' {
            self.cur = p;
            c = self.peek(p);
            p += 1;
            while c.is_ascii_digit() {
                self.cur = p;
                c = self.peek(p);
                p += 1;
            }
        }
        if c == b'e' || c == b'E' {
            self.cur = p;
            c = self.peek(p);
            p += 1;
            if c == b'+' || c == b'-' {
                self.cur = p;
                c = self.peek(p);
                p += 1;
            }
            while c.is_ascii_digit() {
                self.cur = p;
                c = self.peek(p);
                p += 1;
            }
        }
        true
    }

    fn add_error(&mut self, message: impl Into<String>, token: &Token) -> bool {
        self.errors.push((token.start, message.into(), None));
        false
    }

    fn add_error_extra(&mut self, message: impl Into<String>, token: &Token, extra: usize) -> bool {
        self.errors.push((token.start, message.into(), Some(extra)));
        false
    }

    fn recover_from_error(&mut self, skip_until: TokenType) -> bool {
        let count = self.errors.len();
        loop {
            let (ok, skip) = self.read_token();
            if !ok {
                self.errors.truncate(count);
            }
            if skip.tt == skip_until || skip.tt == TokenType::EndOfStream {
                break;
            }
        }
        self.errors.truncate(count);
        false
    }

    fn add_error_and_recover(
        &mut self,
        message: impl Into<String>,
        token: &Token,
        skip_until: TokenType,
    ) -> bool {
        self.add_error(message, token);
        self.recover_from_error(skip_until)
    }

    /// `OurReader::readValue`; fills `slot` like `currentValue()`.
    fn read_value(&mut self, slot: &mut Value) -> bool {
        self.depth += 1;
        if self.depth > 1000 {
            let _ = std::io::Write::write_all(
                &mut std::io::stderr(),
                b"terminate called after throwing an instance of 'Json::RuntimeError'\n  what():  Exceeded stackLimit in readValue().\n",
            );
            std::process::abort();
        }
        let (_, token) = self.read_token_skipping();
        let ok = match token.tt {
            TokenType::ObjectBegin => self.read_object(&token, slot),
            TokenType::ArrayBegin => self.read_array(&token, slot),
            TokenType::Number => self.decode_number(&token, slot),
            TokenType::String => match self.decode_string(&token) {
                Some(s) => {
                    slot.kind = Kind::Str(s);
                    slot.start = token.start;
                    true
                }
                None => false,
            },
            TokenType::True | TokenType::False | TokenType::Null => {
                slot.kind = match token.tt {
                    TokenType::True => Kind::Bool(true),
                    TokenType::False => Kind::Bool(false),
                    _ => Kind::Null,
                };
                slot.start = token.start;
                true
            }
            _ => {
                slot.start = token.start;
                self.add_error("Syntax error: value, object or array expected.", &token)
            }
        };
        self.depth -= 1;
        ok
    }

    fn read_object(&mut self, token: &Token, slot: &mut Value) -> bool {
        slot.kind = Kind::Object(BTreeMap::new());
        slot.start = token.start;
        let mut name = String::new();
        let token_name = loop {
            let (ok, token_name) = self.read_token_skipping();
            if !ok {
                break token_name;
            }
            if token_name.tt == TokenType::ObjectEnd
                && (name.is_empty() || self.features.allow_trailing_commas)
            {
                return true;
            }
            name.clear();
            if token_name.tt == TokenType::String {
                match self.decode_string(&token_name) {
                    Some(decoded) => name = decoded,
                    None => return self.recover_from_error(TokenType::ObjectEnd),
                }
            } else {
                break token_name;
            }
            let Kind::Object(map) = &mut slot.kind else {
                unreachable!()
            };
            if self.features.reject_dup_keys && map.contains_key(&name) {
                let msg = format!("Duplicate key: '{name}'");
                return self.add_error_and_recover(msg, &token_name, TokenType::ObjectEnd);
            }
            let (ok, colon) = self.read_token();
            if !ok || colon.tt != TokenType::MemberSeparator {
                return self.add_error_and_recover(
                    "Missing ':' after object member name",
                    &colon,
                    TokenType::ObjectEnd,
                );
            }
            let mut value = Value {
                kind: Kind::Null,
                start: 0,
            };
            let ok = self.read_value(&mut value);
            if let Kind::Object(map) = &mut slot.kind {
                map.insert(name.clone(), Rc::new(value));
            }
            if !ok {
                return self.recover_from_error(TokenType::ObjectEnd);
            }
            let (ok, comma) = self.read_token_skipping();
            if !ok || (comma.tt != TokenType::ObjectEnd && comma.tt != TokenType::ArraySeparator) {
                return self.add_error_and_recover(
                    "Missing ',' or '}' in object declaration",
                    &comma,
                    TokenType::ObjectEnd,
                );
            }
            if comma.tt == TokenType::ObjectEnd {
                return true;
            }
        };
        self.add_error_and_recover(
            "Missing '}' or object member name",
            &token_name,
            TokenType::ObjectEnd,
        )
    }

    fn read_array(&mut self, token: &Token, slot: &mut Value) -> bool {
        slot.kind = Kind::Array(Vec::new());
        slot.start = token.start;
        let mut index = 0;
        loop {
            self.skip_spaces();
            if self.cur != self.doc.len()
                && self.doc[self.cur] == b']'
                && (index == 0 || self.features.allow_trailing_commas)
            {
                let _ = self.read_token();
                return true;
            }
            index += 1;
            let mut value = Value {
                kind: Kind::Null,
                start: 0,
            };
            let ok = self.read_value(&mut value);
            if let Kind::Array(items) = &mut slot.kind {
                items.push(Rc::new(value));
            }
            if !ok {
                return self.recover_from_error(TokenType::ArrayEnd);
            }
            let (ok, current) = self.read_token_skipping();
            if !ok || (current.tt != TokenType::ArraySeparator && current.tt != TokenType::ArrayEnd)
            {
                return self.add_error_and_recover(
                    "Missing ',' or ']' in array declaration",
                    &current,
                    TokenType::ArrayEnd,
                );
            }
            if current.tt == TokenType::ArrayEnd {
                return true;
            }
        }
    }

    fn decode_number(&mut self, token: &Token, slot: &mut Value) -> bool {
        let text = &self.doc[token.start..token.end];
        let is_negative = text.first() == Some(&b'-');
        let digits = if is_negative { &text[1..] } else { text };
        if digits.iter().all(u8::is_ascii_digit) {
            const POSITIVE_THRESHOLD: u64 = u64::MAX / 10;
            const POSITIVE_LAST: u64 = u64::MAX % 10;
            const NEGATIVE_THRESHOLD: u64 = (-(i64::MIN / 10)) as u64;
            const NEGATIVE_LAST: u64 = (-(i64::MIN % 10)) as u64;
            let (threshold, max_last) = if is_negative {
                (NEGATIVE_THRESHOLD, NEGATIVE_LAST)
            } else {
                (POSITIVE_THRESHOLD, POSITIVE_LAST)
            };
            let mut value: u64 = 0;
            let mut as_double = false;
            for (i, &c) in digits.iter().enumerate() {
                let digit = u64::from(c - b'0');
                if value >= threshold
                    && (value > threshold || i + 1 != digits.len() || digit > max_last)
                {
                    as_double = true;
                    break;
                }
                value = value * 10 + digit;
            }
            if !as_double {
                slot.kind = if is_negative {
                    let last = value % 10;
                    Kind::Int(-((value / 10) as i64) * 10 - last as i64)
                } else if value <= i64::MAX as u64 {
                    Kind::Int(value as i64)
                } else {
                    Kind::UInt(value)
                };
                slot.start = token.start;
                return true;
            }
        }
        match std::str::from_utf8(text)
            .ok()
            .and_then(|s| s.parse::<f64>().ok())
        {
            Some(value) => {
                slot.kind = Kind::Real(value);
                slot.start = token.start;
                true
            }
            None => {
                let msg = format!("'{}' is not a number.", String::from_utf8_lossy(text));
                self.add_error(msg, token)
            }
        }
    }

    fn decode_string(&mut self, token: &Token) -> Option<String> {
        let mut decoded: Vec<u8> = Vec::new();
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
                self.add_error_extra("Empty escape sequence in string", token, current);
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
                    let cp = self.decode_unicode_codepoint(token, &mut current, end)?;
                    let mut buf = [0u8; 4];
                    match char::from_u32(cp) {
                        Some(ch) => decoded.extend_from_slice(ch.encode_utf8(&mut buf).as_bytes()),
                        None => {
                            // Lone surrogates encode as three-byte sequences.
                            decoded.extend_from_slice(&[
                                0xE0 | (0xf & (cp >> 12)) as u8,
                                0x80 | (0x3f & (cp >> 6)) as u8,
                                0x80 | (0x3f & cp) as u8,
                            ]);
                        }
                    }
                }
                _ => {
                    self.add_error_extra("Bad escape sequence in string", token, current);
                    return None;
                }
            }
        }
        Some(String::from_utf8_lossy(&decoded).into_owned())
    }

    fn decode_unicode_codepoint(
        &mut self,
        token: &Token,
        current: &mut usize,
        end: usize,
    ) -> Option<u32> {
        let mut unicode = self.decode_unicode_escape(token, current, end)?;
        if (0xD800..=0xDBFF).contains(&unicode) {
            if end - *current < 6 {
                self.add_error_extra(
                    "additional six characters expected to parse unicode surrogate pair.",
                    token,
                    *current,
                );
                return None;
            }
            if self.doc[*current] == b'\\' && self.doc[*current + 1] == b'u' {
                *current += 2;
                let pair = self.decode_unicode_escape(token, current, end)?;
                unicode = 0x10000 + ((unicode & 0x3FF) << 10) + (pair & 0x3FF);
            } else {
                self.add_error_extra(
                    "expecting another \\u token to begin the second half of a unicode surrogate pair",
                    token,
                    *current,
                );
                return None;
            }
        }
        Some(unicode)
    }

    fn decode_unicode_escape(
        &mut self,
        token: &Token,
        current: &mut usize,
        end: usize,
    ) -> Option<u32> {
        if end - *current < 4 {
            self.add_error_extra(
                "Bad unicode escape sequence in string: four digits expected.",
                token,
                *current,
            );
            return None;
        }
        let mut unicode = 0u32;
        for _ in 0..4 {
            let c = self.doc[*current];
            *current += 1;
            unicode *= 16;
            match c {
                b'0'..=b'9' => unicode += u32::from(c - b'0'),
                b'a'..=b'f' => unicode += u32::from(c - b'a' + 10),
                b'A'..=b'F' => unicode += u32::from(c - b'A' + 10),
                _ => {
                    self.add_error_extra(
                        "Bad unicode escape sequence in string: hexadecimal digit expected.",
                        token,
                        *current,
                    );
                    return None;
                }
            }
        }
        Some(unicode)
    }
}

/// The result of `OurCharReader::parse`: the (possibly partial) root and
/// the errors as (offset_start, message, extra offset).
pub struct Parsed {
    pub root: Value,
    pub errors: Vec<(usize, String, Option<usize>)>,
    /// The document after the skipped byte order mark.
    doc_start: usize,
}

/// `OurCharReader::parse` with the given features (skipBom is on).
pub fn parse_with(doc: &[u8], features: Features) -> Parsed {
    let doc_start = if doc.starts_with(b"\xEF\xBB\xBF") {
        3
    } else {
        0
    };
    let mut reader = Reader {
        doc,
        cur: doc_start,
        depth: 0,
        features,
        errors: Vec::new(),
    };
    let mut root = Value {
        kind: Kind::Null,
        start: 0,
    };
    reader.read_value(&mut root);
    let (_, token) = reader.read_token_skipping();
    if features.fail_if_extra && token.tt != TokenType::EndOfStream {
        reader.add_error("Extra non-whitespace after JSON value.", &token);
    } else if features.strict_root && !root.is_array() && !root.is_object() {
        let token = Token {
            tt: TokenType::Error,
            start: doc_start,
            end: doc.len(),
        };
        reader.add_error(
            "A valid JSON document must be either an array or an object value.",
            &token,
        );
    }
    Parsed {
        root,
        errors: reader.errors,
        doc_start,
    }
}

impl Parsed {
    /// `OurReader::getLocationLineAndColumn`.
    fn location(doc: &[u8], begin: usize, location: usize) -> String {
        let mut current = begin;
        let mut last_line_start = current;
        let mut line = 0;
        while current < location && current != doc.len() {
            let c = doc[current];
            current += 1;
            if c == b'\r' {
                if current != doc.len() && doc[current] == b'\n' {
                    current += 1;
                }
                last_line_start = current;
                line += 1;
            } else if c == b'\n' {
                last_line_start = current;
                line += 1;
            }
        }
        format!(
            "Line {}, Column {}",
            line + 1,
            location.saturating_sub(last_line_start) + 1
        )
    }

    /// `OurReader::getFormattedErrorMessages`.
    pub fn formatted_errors(&self, doc: &[u8]) -> String {
        let mut out = String::new();
        for (start, message, extra) in &self.errors {
            out += &format!(
                "* {}\n  {message}\n",
                Self::location(doc, self.doc_start, *start)
            );
            if let Some(extra) = extra {
                out += &format!(
                    "See {} for detail.\n",
                    Self::location(doc, self.doc_start, *extra)
                );
            }
        }
        out
    }
}

/// strictMode parse with structured errors (offset_start, message).
fn parse(doc: &[u8]) -> (Value, Vec<(usize, String)>) {
    let parsed = parse_with(doc, Features::STRICT);
    let errors = parsed
        .errors
        .into_iter()
        .map(|(start, message, _)| (start, message))
        .collect();
    (parsed.root, errors)
}

// ---------------------------------------------------------------------
// cmJSONState

struct StateError {
    /// (line, column) or (-1, -1).
    location: (i32, i32),
    message: String,
}

#[derive(Default)]
pub struct State {
    pub parse_stack: Vec<(String, Option<V>)>,
    errors: Vec<StateError>,
    doc: Vec<u8>,
    pub allow_comments: bool,
    filename: String,
}

impl State {
    /// `cmJSONState(jsonFile, root)`.
    pub fn read(filename: &str) -> (State, Option<V>) {
        let mut state = State {
            filename: filename.to_owned(),
            ..State::default()
        };
        let Ok(mut doc) = std::fs::read(filename) else {
            state.add_error(format!("File not found: {filename}"));
            return (state, None);
        };
        // cmsys::FStream::ReadBOM
        if doc.starts_with(b"\xEF\xBB\xBF") {
            doc.drain(..3);
        }
        if doc.is_empty() {
            state.add_error("A JSON document cannot be empty");
            return (state, None);
        }
        state.doc = doc;
        let (root, errors) = parse(&state.doc);
        for (offset, message) in errors {
            state.add_error_at_offset(message, offset);
        }
        (state, Some(Rc::new(root)))
    }

    pub fn has_errors(&self) -> bool {
        !self.errors.is_empty()
    }

    pub fn add_error(&mut self, message: impl Into<String>) {
        self.errors.push(StateError {
            location: (-1, -1),
            message: message.into(),
        });
    }

    pub fn add_error_at_value(&mut self, message: impl Into<String>, value: Option<&V>) {
        match value {
            Some(value) if !value.is_null() => self.add_error_at_offset(message, value.start),
            _ => self.add_error(message),
        }
    }

    fn add_error_at_offset(&mut self, message: impl Into<String>, offset: usize) {
        if self.doc.is_empty() {
            self.add_error(message);
        } else {
            let location = self.locate(offset);
            self.errors.push(StateError {
                location,
                message: message.into(),
            });
        }
    }

    fn locate(&self, offset: usize) -> (i32, i32) {
        let mut line = 1;
        let mut col = 1;
        let last = offset.min(self.doc.len());
        let mut i = 0;
        while i != last {
            match self.doc[i] {
                b'\r' if i + 1 != last && self.doc.get(i + 1) == Some(&b'\n') => {}
                b'\r' | b'\n' => {
                    col = 1;
                    line += 1;
                }
                _ => col += 1,
            }
            i += 1;
        }
        (line, col)
    }

    fn json_context(&self, (line, column): (i32, i32)) -> String {
        let mut current: &[u8] = &[];
        let mut lines = self.doc.split(|&b| b == b'\n');
        for _ in 0..line {
            if let Some(next) = lines.next() {
                current = next;
            }
        }
        format!(
            "{}\n{}^",
            String::from_utf8_lossy(current),
            " ".repeat((column - 1).max(0) as usize)
        )
    }

    /// `GetErrorMessage(showContext = true)`.
    pub fn error_message(&self) -> String {
        let name = crate::util::filename_name(&self.filename);
        let mut message = String::new();
        for error in &self.errors {
            let (line, _) = error.location;
            if !name.is_empty() && line > 0 {
                message += &format!("{name}:{line}: ");
            }
            message += &error.message;
            message.push('\n');
            if line > 0 {
                message += &self.json_context(error.location);
                message.push('\n');
            }
        }
        message.pop();
        message
    }

    pub fn key(&self) -> String {
        self.parse_stack
            .last()
            .map(|(k, _)| k.clone())
            .unwrap_or_default()
    }

    pub fn key_after(&self, key: &str) -> String {
        let mut it = self.parse_stack.iter();
        while let Some((k, _)) = it.next() {
            if k == key {
                return it.next().map(|(k, _)| k.clone()).unwrap_or_default();
            }
        }
        String::new()
    }

    pub fn push_stack(&mut self, key: impl Into<String>, value: Option<V>) {
        self.parse_stack.push((key.into(), value));
    }

    pub fn pop_stack(&mut self) {
        self.parse_stack.pop();
    }
}

// ---------------------------------------------------------------------
// cmJSONHelpers

pub type Helper<T> = Rc<dyn Fn(&mut T, Option<&V>, &mut State) -> bool>;
pub type ErrorGen = Rc<dyn Fn(Option<&V>, &mut State)>;
pub type ObjectErrorGen = Rc<dyn Fn(ObjectError, &[String]) -> ErrorGen>;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ObjectError {
    RequiredMissing,
    InvalidObject,
    ExtraField,
    MissingRequired,
}

pub fn error_gen(f: impl Fn(Option<&V>, &mut State) + 'static) -> ErrorGen {
    Rc::new(f)
}

pub fn helper<T>(f: impl Fn(&mut T, Option<&V>, &mut State) -> bool + 'static) -> Helper<T> {
    Rc::new(f)
}

/// `JsonErrors::EXPECTED_TYPE`.
pub fn expected_type(kind: &'static str) -> ErrorGen {
    error_gen(move |value, state| {
        let key = state.key();
        if key.is_empty() {
            state.add_error_at_value(format!("Expected {kind}"), value);
            return;
        }
        let mut message = format!("\"{key}\" expected {kind}");
        if let Some(value) = value
            && value.is_convertible_to_string()
        {
            message += &format!(", got: {}", value.as_string());
        }
        state.add_error_at_value(message, value);
    })
}

pub fn invalid_string() -> ErrorGen {
    expected_type("a string")
}
pub fn invalid_bool() -> ErrorGen {
    expected_type("a bool")
}
pub fn invalid_int() -> ErrorGen {
    expected_type("an integer")
}

/// `JsonErrors::INVALID_NAMED_OBJECT`.
pub fn invalid_named_object(
    name_gen: impl Fn(Option<&V>, &State) -> String + 'static,
) -> ObjectErrorGen {
    let name_gen = Rc::new(name_gen);
    Rc::new(
        move |error_type: ObjectError, extra: &[String]| -> ErrorGen {
            let name_gen = name_gen.clone();
            let extra = extra.to_vec();
            error_gen(move |value, state| {
                let name = name_gen(value, state);
                match error_type {
                    ObjectError::RequiredMissing => {
                        state.add_error_at_value(format!("Invalid Required {name}"), value)
                    }
                    ObjectError::InvalidObject => {
                        state.add_error_at_value(format!("Invalid {name}"), value)
                    }
                    ObjectError::ExtraField => {
                        for member in &extra {
                            let message = format!("Invalid extra field \"{member}\" in {name}");
                            match value {
                                Some(value) => {
                                    let member_value = value.member(member);
                                    state.add_error_at_value(message, member_value.as_ref())
                                }
                                None => state.add_error(message),
                            }
                        }
                    }
                    ObjectError::MissingRequired => {
                        let key = state.key();
                        state.add_error_at_value(
                            format!("Missing required field \"{key}\" in {name}"),
                            value,
                        )
                    }
                }
            })
        },
    )
}

/// `JsonErrors::INVALID_OBJECT`.
pub fn invalid_object() -> ObjectErrorGen {
    invalid_named_object(|_, _| "Object".to_owned())
}

/// `JsonErrors::INVALID_NAMED_OBJECT_KEY`.
pub fn invalid_named_object_key() -> ObjectErrorGen {
    invalid_named_object(|_, state| {
        for (key, _) in state.parse_stack.iter().rev() {
            if key.starts_with("$vector_item_") {
                continue;
            }
            return format!("\"{key}\"");
        }
        "root".to_owned()
    })
}

struct Member<T> {
    name: &'static str,
    func: Helper<T>,
    required: bool,
}

/// `cmJSONHelperBuilder::Object<T>`.
pub struct Object<T> {
    members: Vec<Member<T>>,
    any_required: bool,
    error: ObjectErrorGen,
    allow_extra: bool,
}

impl<T: 'static> Object<T> {
    pub fn new(error: ObjectErrorGen, allow_extra: bool) -> Self {
        Object {
            members: Vec::new(),
            any_required: false,
            error,
            allow_extra,
        }
    }

    /// `Bind(name, &T::member, func, required)`.
    pub fn bind<M: 'static>(
        self,
        name: &'static str,
        get: fn(&mut T) -> &mut M,
        func: Helper<M>,
        required: bool,
    ) -> Self {
        self.bind_fn(
            name,
            helper(move |out: &mut T, value, state| func(get(out), value, state)),
            required,
        )
    }

    /// `Bind<M>(name, nullptr, func, required)`.
    pub fn bind_dummy<M: Default + 'static>(
        self,
        name: &'static str,
        func: Helper<M>,
        required: bool,
    ) -> Self {
        self.bind_fn(
            name,
            helper(move |_: &mut T, value, state| {
                let mut dummy = M::default();
                func(&mut dummy, value, state)
            }),
            required,
        )
    }

    /// `Bind(name, func, required)` with a function of the whole object.
    pub fn bind_fn(mut self, name: &'static str, func: Helper<T>, required: bool) -> Self {
        self.members.push(Member {
            name,
            func,
            required,
        });
        self.any_required = self.any_required || required;
        self
    }

    pub fn build(self) -> Helper<T> {
        Rc::new(
            move |out: &mut T, value: Option<&V>, state: &mut State| -> bool {
                let mut extra: Vec<String> = Vec::new();
                if value.is_none() && self.any_required {
                    (self.error)(ObjectError::RequiredMissing, &extra)(value, state);
                    return false;
                }
                if let Some(v) = value
                    && !v.is_object()
                {
                    (self.error)(ObjectError::InvalidObject, &extra)(value, state);
                    return false;
                }
                if let Some(v) = value {
                    extra = v.member_names();
                }
                if state.allow_comments {
                    extra.retain(|name| name != "$comment");
                }
                let mut success = true;
                for m in &self.members {
                    state.push_stack(m.name, value.cloned());
                    let member = value.and_then(|v| v.member(m.name));
                    if let Some(member) = member {
                        if !(m.func)(out, Some(&member), state) {
                            success = false;
                        }
                        if let Some(pos) = extra.iter().position(|n| n == m.name) {
                            extra.remove(pos);
                        }
                    } else if !m.required {
                        if !(m.func)(out, None, state) {
                            success = false;
                        }
                    } else {
                        (self.error)(ObjectError::MissingRequired, &extra)(value, state);
                        success = false;
                    }
                    state.pop_stack();
                }
                if !self.allow_extra && !extra.is_empty() {
                    (self.error)(ObjectError::ExtraField, &extra)(value, state);
                    success = false;
                }
                success
            },
        )
    }
}

pub fn string_helper(error: ErrorGen, defval: &str) -> Helper<String> {
    let defval = defval.to_owned();
    helper(move |out: &mut String, value, state| {
        let Some(value) = value else {
            *out = defval.clone();
            return true;
        };
        if !value.is_string() {
            error(Some(value), state);
            return false;
        }
        *out = value.as_string();
        true
    })
}

pub fn int_helper(error: ErrorGen, defval: i32) -> Helper<i32> {
    helper(move |out: &mut i32, value, state| {
        let Some(value) = value else {
            *out = defval;
            return true;
        };
        if !value.is_int() {
            error(Some(value), state);
            return false;
        }
        *out = value.as_int();
        true
    })
}

pub fn uint_helper(error: ErrorGen, defval: u32) -> Helper<u32> {
    helper(move |out: &mut u32, value, state| {
        let Some(value) = value else {
            *out = defval;
            return true;
        };
        if !value.is_uint() {
            error(Some(value), state);
            return false;
        }
        *out = value.as_uint();
        true
    })
}

pub fn bool_helper(error: ErrorGen, defval: bool) -> Helper<bool> {
    helper(move |out: &mut bool, value, state| {
        let Some(value) = value else {
            *out = defval;
            return true;
        };
        if !value.is_bool() {
            error(Some(value), state);
            return false;
        }
        *out = value.as_bool();
        true
    })
}

/// `cmJSONHelperBuilder::Vector<T>`.
pub fn vector_helper<T: Default + 'static>(error: ErrorGen, func: Helper<T>) -> Helper<Vec<T>> {
    helper(move |out: &mut Vec<T>, value, state| {
        let Some(value) = value else {
            out.clear();
            return true;
        };
        if !value.is_array() {
            error(Some(value), state);
            return false;
        }
        out.clear();
        let mut success = true;
        for (index, item) in value.items().iter().enumerate() {
            state.push_stack(format!("$vector_item_{index}"), Some(item.clone()));
            let mut t = T::default();
            if !func(&mut t, Some(item), state) {
                success = false;
            }
            out.push(t);
            state.pop_stack();
        }
        success
    })
}

/// `cmJSONHelperBuilder::Map<T>`.
pub fn map_helper<T: Default + 'static>(
    error: ErrorGen,
    func: Helper<T>,
) -> Helper<BTreeMap<String, T>> {
    helper(move |out: &mut BTreeMap<String, T>, value, state| {
        let Some(value) = value else {
            out.clear();
            return true;
        };
        if !value.is_object() {
            error(Some(value), state);
            return false;
        }
        out.clear();
        let mut success = true;
        for key in value.member_names() {
            let member = value.member(&key);
            state.push_stack(key.clone(), member.clone());
            let mut t = T::default();
            success = func(&mut t, member.as_ref(), state) && success;
            out.entry(key).or_insert(t);
            state.pop_stack();
        }
        success
    })
}

/// `cmJSONHelperBuilder::Optional<T>`.
pub fn optional_helper<T: Default + 'static>(func: Helper<T>) -> Helper<Option<T>> {
    helper(move |out: &mut Option<T>, value, state| {
        let Some(value) = value else {
            *out = None;
            return true;
        };
        let inner = out.insert(T::default());
        func(inner, Some(value), state)
    })
}

/// `cmJSONHelperBuilder::Required<T>`.
pub fn required_helper<T: 'static>(error: ErrorGen, func: Helper<T>) -> Helper<T> {
    helper(move |out: &mut T, value, state| {
        if value.is_none() {
            error(value, state);
            return false;
        }
        func(out, value, state)
    })
}

/// `cmJSONHelperBuilder::Checked<T>`.
pub fn checked_helper<T: 'static>(
    error: ErrorGen,
    func: Helper<T>,
    predicate: impl Fn(&T) -> bool + 'static,
) -> Helper<T> {
    helper(move |out: &mut T, value, state| {
        let mut result = func(out, value, state);
        if result && !predicate(out) {
            error(value, state);
            result = false;
        }
        result
    })
}
