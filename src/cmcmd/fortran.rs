//! Dependency lexer, statement grammar, and actions from cmFortranLexer.in.l,
//! cmFortranParser.y, and cmFortranParserImpl.cxx. The Ninja scanner starts
//! flex in INITIAL (it does not select a source form from the extension).
//! Upstream lexer/parser portions derived from makedepf90 2.8.8, Copyright
//! (C) 2000-2006 Erik Edelmann, distributed under CMake's BSD-3-Clause license.

use std::collections::BTreeSet;
use std::path::Path;

#[derive(Debug, PartialEq)]
enum Token {
    Word(String),
    String(String),
    Keyword(&'static str),
    Directive(&'static str),
    Angle(String),
    Colon,
    Dcolon,
    Comma,
    Left,
    Right,
    Other,
    Eostmt,
}

struct Lexer {
    bytes: Vec<u8>,
    pos: usize,
}

impl Lexer {
    fn new(mut bytes: Vec<u8>) -> Self {
        if bytes.last() != Some(&b'\n') {
            bytes.push(b'\n');
        }
        Self { bytes, pos: 0 }
    }

    fn spaces(&self, mut at: usize) -> usize {
        while matches!(self.bytes.get(at), Some(b' ' | b'\t')) {
            at += 1;
        }
        at
    }

    fn continued(&self, at: usize) -> Option<usize> {
        let mut i = self.spaces(at);
        i += usize::from(self.bytes.get(i) == Some(&b'\r'));
        (self.bytes.get(i) == Some(&b'\n')).then_some(i + 1)
    }

    fn directive(&mut self, at: usize, prefix: u8) -> Option<Token> {
        let i = self.spaces(at);
        let rest = &self.bytes[i..];
        if prefix == b'#' {
            let is_line = rest
                .get(..4)
                .is_some_and(|s| s.eq_ignore_ascii_case(b"line"));
            let mut j = self.spaces(i + if is_line { 4 } else { 0 });
            if is_line || self.bytes.get(j).is_some_and(u8::is_ascii_digit) {
                while self.bytes.get(j).is_some_and(u8::is_ascii_digit) {
                    j += 1;
                }
                self.pos = self.spaces(j);
                return Some(Token::Directive("line"));
            }
        }
        for name in [
            "include", "define", "undef", "ifndef", "ifdef", "if", "elif", "else", "endif",
        ] {
            if prefix == b'?' && name != "include" {
                break;
            }
            if rest
                .get(..name.len())
                .is_some_and(|s| s.eq_ignore_ascii_case(name.as_bytes()))
            {
                self.pos = i + name.len();
                let j = self.spaces(self.pos);
                if prefix == b'#'
                    && name == "include"
                    && self.bytes.get(j) == Some(&b'<')
                    && let Some(len) = self.bytes[j + 1..].iter().position(|&c| c == b'>')
                    && len > 0
                {
                    self.pos = j + len + 2;
                    return Some(Token::Angle(
                        String::from_utf8_lossy(&self.bytes[j + 1..self.pos - 1]).into_owned(),
                    ));
                }
                return Some(Token::Directive(name));
            }
        }
        None
    }

    fn next(&mut self) -> Option<Token> {
        use Token::*;
        loop {
            let bol = self.pos == 0 || self.bytes[self.pos - 1] == b'\n';
            self.pos = self.spaces(self.pos);
            let start = self.pos;
            let c = *self.bytes.get(start)?;
            self.pos += 1;
            let token = match c {
                b'#' if bol => self.directive(self.pos, c).unwrap_or(Other),
                b'$' => self.directive(self.pos, c).unwrap_or(Other),
                b'?' if self.bytes.get(self.pos) == Some(&b'?') => {
                    self.directive(self.pos + 1, c).unwrap_or(Other)
                }
                b'\r' => continue,
                b'\n' | b';' => Eostmt,
                b'!' => {
                    while self.bytes[self.pos] != b'\n' {
                        self.pos += 1;
                    }
                    self.pos += 1;
                    Eostmt
                }
                b'\\' if self.continued(self.pos).is_some() => {
                    self.pos = self.continued(self.pos).unwrap();
                    continue;
                }
                b'&' => {
                    loop {
                        match self.bytes.get(self.pos) {
                            Some(b' ' | b'\t' | b'\n' | b'\r') => self.pos += 1,
                            Some(b'!') => {
                                while self.bytes[self.pos] != b'\n' {
                                    self.pos += 1;
                                }
                            }
                            _ => break,
                        }
                    }
                    self.pos += usize::from(self.bytes.get(self.pos) == Some(&b'&'));
                    continue;
                }
                b'"' | b'\'' => {
                    let mut text = Vec::new();
                    loop {
                        let ch = *self.bytes.get(self.pos)?;
                        // The string start conditions also match a fixed-form
                        // continuation. In INITIAL flex unputs column six.
                        if matches!(ch, b'\r' | b'\n') {
                            let next = self.pos
                                + if ch == b'\r' && self.bytes.get(self.pos + 1) == Some(&b'\n') {
                                    2
                                } else {
                                    1
                                };
                            if (ch == b'\n' || next == self.pos + 2)
                                && self.bytes.get(next..next + 5) == Some(b"     ")
                                && self
                                    .bytes
                                    .get(next + 5)
                                    .is_some_and(|c| !matches!(c, b' ' | b'\t' | b'\r' | b'\n'))
                            {
                                self.pos = next + 5;
                                continue;
                            }
                        }
                        if ch == b'\n' {
                            break Other;
                        }
                        self.pos += 1;
                        if ch == c {
                            break String(std::string::String::from_utf8_lossy(&text).into_owned());
                        }
                        if ch == b'&'
                            && let Some(after) = self.continued(self.pos)
                        {
                            self.pos = after;
                            let j = self.spaces(after);
                            if self.bytes.get(j) == Some(&b'&') {
                                self.pos = j + 1;
                            }
                        } else {
                            text.push(ch);
                        }
                    }
                }
                b',' => Comma,
                b':' if self.bytes.get(self.pos) == Some(&b':') => {
                    self.pos += 1;
                    Dcolon
                }
                b':' => Colon,
                b'(' => Left,
                b')' => Right,
                c if c.is_ascii_alphabetic() || c == b'_' => {
                    while self
                        .bytes
                        .get(self.pos)
                        .is_some_and(|b| b.is_ascii_alphanumeric() || *b == b'_')
                    {
                        self.pos += 1;
                    }
                    let word = &self.bytes[start..self.pos];
                    ["end", "include", "interface", "module", "submodule", "use"]
                        .into_iter()
                        .find(|kw| word.eq_ignore_ascii_case(kw.as_bytes()))
                        .map_or_else(
                            || Word(std::string::String::from_utf8_lossy(word).into_owned()),
                            Keyword,
                        )
                }
                _ => Other,
            };
            return Some(token);
        }
    }
}

#[derive(Default)]
pub(super) struct Parser {
    pub includes: BTreeSet<String>,
    pub provides: BTreeSet<String>,
    pub requires: BTreeSet<String>,
    /// `cmFortranSourceInfo::Intrinsics`: `use, intrinsic :: name`.
    pub intrinsics: BTreeSet<String>,
    search: Vec<String>,
    separator: String,
    extension: String,
    interface: bool,
    defines: BTreeSet<String>,
    skip_to_end: Vec<bool>,
    false_depth: usize,
}

impl Parser {
    pub fn new(search: Vec<String>, separator: &str, extension: &str) -> Self {
        Self {
            search,
            separator: separator.into(),
            extension: if extension.is_empty() {
                ".mod"
            } else {
                extension
            }
            .into(),
            ..Self::default()
        }
    }

    /// The preprocessor symbols defined before scanning
    /// (`cmFortranParser` constructed with `PPDefinitions`).
    pub fn with_definitions(mut self, definitions: &BTreeSet<String>) -> Self {
        self.defines.extend(definitions.iter().cloned());
        self
    }

    fn submodule(&self, parent: &str, child: &str) -> String {
        if self.separator.is_empty() {
            format!("{}{}", child.to_ascii_lowercase(), self.extension)
        } else {
            format!(
                "{}{}{}{}",
                parent.to_ascii_lowercase(),
                self.separator,
                child.to_ascii_lowercase(),
                self.extension
            )
        }
    }

    pub fn scan(&mut self, path: &str) -> Result<(), String> {
        // An include pushes a new lexer buffer without resetting parser state.
        let open = |p: &str| {
            std::fs::read(p)
                .map(Lexer::new)
                .map_err(|_| format!("-E cmake_ninja_depends failed to open {p}"))
        };
        let mut files = vec![(path.to_owned(), open(path)?)];
        let mut stmt = Vec::new();
        while let Some((path, lexer)) = files.last_mut() {
            match lexer.next() {
                Some(Token::Eostmt) => {
                    if let Some(include) = self.statement(&stmt, path) {
                        if files.len() >= 1024 {
                            return Err(format!("{include}:1: Fortran include nesting too deep"));
                        }
                        let next = open(&include)?;
                        self.includes.insert(include.clone());
                        files.push((include, next));
                    }
                    stmt.clear();
                }
                Some(token) => stmt.push(token),
                None => {
                    files.pop();
                }
            }
        }
        Ok(())
    }

    fn statement(&mut self, tokens: &[Token], file: &str) -> Option<String> {
        use Token::{
            Angle, Colon, Comma, Dcolon, Directive as D, Keyword as K, Left, Right, String as Str,
            Word,
        };
        // `other` in the grammar accepts misc_code, not preprocessor
        // directives other than line markers. Recover at the next EOSTMT.
        if tokens
            .iter()
            .skip(1)
            .any(|t| matches!(t, D(s) if *s != "line") || matches!(t, Angle(_)))
        {
            return None;
        }
        let active = self.false_depth == 0;
        let module = |name: &str| format!("{}.mod", name.to_ascii_lowercase());
        match tokens {
            [D("line"), Str(path), ..] if !path.starts_with('<') => {
                let path = path.replace("\\\\", "\\").replace('\\', "/");
                if Path::new(&path).is_file() {
                    self.includes.insert(path);
                }
            }
            [D(test @ ("ifdef" | "ifndef")), Word(name), ..] => {
                let take = self.defines.contains(name) == (*test == "ifdef");
                self.skip_to_end.push(active && take);
                if !active || !take {
                    self.false_depth += 1;
                }
            }
            [D("if"), ..] => self.skip_to_end.push(false),
            [D("elif"), ..] if active && self.skip_to_end.last() == Some(&true) => {
                self.false_depth = 1
            }
            [D("else"), ..] if self.false_depth <= 1 => {
                self.false_depth = usize::from(self.skip_to_end.last() == Some(&true))
            }
            [D("endif"), ..] => {
                self.skip_to_end.pop();
                self.false_depth = self.false_depth.saturating_sub(1);
            }
            _ if !active => {}
            [D("define"), Word(name), ..] => {
                self.defines.insert(name.clone());
            }
            [D("undef"), Word(name), ..] => {
                self.defines.remove(name);
            }
            [K("interface")] | [K("interface"), Word(_)] => self.interface = true,
            [K("end"), K("interface")] | [K("end"), K("interface"), Word(_)] => {
                self.interface = false
            }
            [K("module"), Word(name)] if !self.interface => {
                self.provides.insert(module(name));
            }
            [K("use"), Word(name), ..] | [K("use"), Dcolon, Word(name), ..] => {
                self.requires.insert(module(name));
            }
            [K("use"), Comma, Word(nature), Dcolon, Word(name), ..]
                if nature.eq_ignore_ascii_case("non_intrinsic") =>
            {
                self.requires.insert(module(name));
            }
            [K("use"), Comma, Word(nature), Dcolon, Word(name), ..]
                if nature.eq_ignore_ascii_case("intrinsic") =>
            {
                self.intrinsics.insert(module(name));
            }
            [K("submodule"), Left, Word(parent), Right, Word(child)] => {
                self.requires.insert(module(parent));
                self.provides.insert(self.submodule(parent, child));
            }
            [
                K("submodule"),
                Left,
                Word(parent),
                Colon,
                Word(child),
                Right,
                Word(nested),
            ] => {
                self.requires.insert(self.submodule(parent, child));
                self.provides.insert(self.submodule(parent, nested));
            }
            [K("include"), Str(name)] | [D("include"), Str(name), ..] | [Angle(name), ..] => {
                if Path::new(name).is_absolute() {
                    return Path::new(name).is_file().then(|| name.clone());
                }
                let dir = crate::util::filename_path(file);
                return std::iter::once(&dir)
                    .chain(&self.search)
                    .map(|dir| format!("{dir}/{name}"))
                    .find(|p| Path::new(p).is_file());
            }
            _ => {}
        }
        None
    }
}
