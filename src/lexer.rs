//! CMake language lexer and parser.
//!
//! Mirrors the token rules of Kitware's `cmListFileLexer.in.l` (flex,
//! longest-match, earlier-rule-wins) and the recursive-descent parser in
//! `cmListFileCache.cxx`, including legacy unquoted arguments, bracket
//! arguments/comments, quoted-argument line continuations, and the
//! argument-separation diagnostics.

use std::rc::Rc;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Delim {
    Unquoted,
    Quoted,
    Bracket,
}

#[derive(Clone, Debug)]
pub struct Arg {
    pub value: String,
    pub delim: Delim,
    pub line: usize,
}

#[derive(Clone, Debug)]
pub struct Command {
    /// Name as written in the source.
    pub name: String,
    /// Lower-cased name used for dispatch.
    pub lname: String,
    pub line: usize,
    pub line_end: usize,
    pub args: Vec<Arg>,
}

/// A parsed list file. Commands are shared so that function bodies can refer
/// to slices without copying.
#[derive(Debug)]
pub struct ListFile {
    pub path: String,
    pub commands: Vec<Command>,
}

#[derive(Debug)]
pub struct ParseDiag {
    pub line: usize,
    /// Message text (without location).
    pub message: String,
    /// When true, the diagnostic is an AUTHOR_WARNING and parsing continued.
    pub warning: bool,
    /// Command name for the diagnostic's context (`cmListFileContext::
    /// FromListFileFunction` for nesting errors).
    pub command: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Tok {
    Space,
    Newline,
    Identifier,
    ParenLeft,
    ParenRight,
    ArgUnquoted,
    ArgQuoted,
    ArgBracket,
    CommentBracket,
    BadCharacter,
    BadBracket,
    BadString,
}

impl Tok {
    fn describe(self) -> &'static str {
        match self {
            Tok::Space => "space",
            Tok::Newline => "newline",
            Tok::Identifier => "identifier",
            Tok::ParenLeft => "left paren",
            Tok::ParenRight => "right paren",
            Tok::ArgUnquoted => "unquoted argument",
            Tok::ArgQuoted => "quoted argument",
            Tok::ArgBracket => "bracket argument",
            Tok::CommentBracket => "bracket comment",
            Tok::BadCharacter => "bad character",
            Tok::BadBracket => "unterminated bracket",
            Tok::BadString => "unterminated string",
        }
    }
}

struct Token {
    kind: Tok,
    text: String,
    line: usize,
    column: usize,
}

struct Lexer<'a> {
    src: &'a [u8],
    pos: usize,
    line: usize,
    column: usize,
}

fn is_unquoted_plain(byte: u8) -> bool {
    !matches!(
        byte,
        b' ' | 0 | b'\t' | b'\r' | b'\n' | b'(' | b')' | b'#' | b'\\' | b'"' | b'[' | b'='
    )
}

impl<'a> Lexer<'a> {
    fn at(&self, index: usize) -> Option<u8> {
        self.src.get(index).copied()
    }

    /// Length of one `{UNQUOTED}` unit at `index`.
    fn unquoted_unit(&self, index: usize) -> usize {
        match self.at(index) {
            Some(b'\\') => match self.at(index + 1) {
                Some(next) if next != 0 && next != b'\n' => 2,
                _ => 0,
            },
            Some(byte) if is_unquoted_plain(byte) => 1,
            _ => 0,
        }
    }

    /// Length of one `{MAKEVAR}` unit: `$(` [A-Za-z0-9_]* `)`.
    fn makevar_unit(&self, index: usize) -> usize {
        if self.at(index) != Some(b'$') || self.at(index + 1) != Some(b'(') {
            return 0;
        }
        let mut end = index + 2;
        while self
            .at(end)
            .is_some_and(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
        {
            end += 1;
        }
        if self.at(end) == Some(b')') {
            end + 1 - index
        } else {
            0
        }
    }

    /// Length of one `{LEGACY}` unit.
    fn legacy_unit(&self, index: usize) -> usize {
        let makevar = self.makevar_unit(index);
        let unquoted = self.unquoted_unit(index);
        let mut best = makevar.max(unquoted);
        if self.at(index) == Some(b'"') {
            let mut end = index + 1;
            loop {
                let step = self.makevar_unit(end).max(self.unquoted_unit(end));
                if step > 0 {
                    end += step;
                } else if matches!(self.at(end), Some(b' ' | b'\t' | b'[' | b'=')) {
                    end += 1;
                } else {
                    break;
                }
            }
            if self.at(end) == Some(b'"') {
                best = best.max(end + 1 - index);
            }
        }
        best
    }

    /// Longest match of the plain unquoted-argument rule.
    fn match_unquoted(&self) -> usize {
        let start = self.pos;
        let mut end;
        let first = self.unquoted_unit(start);
        if first > 0 {
            end = start + first;
        } else if self.at(start) == Some(b'=') {
            end = start + 1;
        } else if self.at(start) == Some(b'[') {
            let mut probe = start + 1;
            while self.at(probe) == Some(b'=') {
                probe += 1;
            }
            let unit = self.unquoted_unit(probe);
            if unit == 0 {
                return 0;
            }
            end = probe + unit;
        } else {
            return 0;
        }
        loop {
            let unit = self.unquoted_unit(end);
            if unit > 0 {
                end += unit;
            } else if matches!(self.at(end), Some(b'[' | b'=')) {
                end += 1;
            } else {
                return end - start;
            }
        }
    }

    /// Longest match of the legacy unquoted-argument rule.
    fn match_legacy(&self) -> usize {
        let start = self.pos;
        let mut end;
        let first = self.makevar_unit(start).max(self.unquoted_unit(start));
        if first > 0 {
            end = start + first;
        } else if self.at(start) == Some(b'=') {
            end = start + 1;
        } else if self.at(start) == Some(b'[') {
            let mut probe = start + 1;
            while self.at(probe) == Some(b'=') {
                probe += 1;
            }
            let unit = self.legacy_unit(probe);
            if unit == 0 {
                return 0;
            }
            end = probe + unit;
        } else {
            return 0;
        }
        loop {
            let unit = self.legacy_unit(end);
            if unit > 0 {
                end += unit;
            } else if matches!(self.at(end), Some(b'[' | b'=')) {
                end += 1;
            } else {
                return end - start;
            }
        }
    }

    fn match_identifier(&self) -> usize {
        match self.at(self.pos) {
            Some(byte) if byte.is_ascii_alphabetic() || byte == b'_' => {}
            _ => return 0,
        }
        let mut end = self.pos + 1;
        while self
            .at(end)
            .is_some_and(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
        {
            end += 1;
        }
        end - self.pos
    }

    /// Matches `#?\[=*\[\n?`; returns (length, equals count).
    fn match_bracket_open(&self) -> Option<(usize, usize)> {
        let mut index = self.pos;
        if self.at(index) == Some(b'#') {
            index += 1;
        }
        if self.at(index) != Some(b'[') {
            return None;
        }
        index += 1;
        let mut equals = 0;
        while self.at(index) == Some(b'=') {
            index += 1;
            equals += 1;
        }
        if self.at(index) != Some(b'[') {
            return None;
        }
        index += 1;
        if self.at(index) == Some(b'\n') {
            index += 1;
        }
        Some((index - self.pos, equals))
    }

    fn text(&self, start: usize, end: usize) -> String {
        String::from_utf8_lossy(&self.src[start..end]).into_owned()
    }

    fn advance(&mut self, length: usize) {
        for &byte in &self.src[self.pos..self.pos + length] {
            if byte == b'\n' {
                self.line += 1;
                self.column = 1;
            } else {
                self.column += 1;
            }
        }
        self.pos += length;
    }

    fn token(&mut self, kind: Tok, length: usize) -> Token {
        let token = Token {
            kind,
            text: self.text(self.pos, self.pos + length),
            line: self.line,
            column: self.column,
        };
        self.advance(length);
        token
    }

    fn next(&mut self) -> Option<Token> {
        loop {
            let byte = self.at(self.pos)?;
            let line = self.line;
            let column = self.column;
            if byte == b'\n' {
                return Some(self.token(Tok::Newline, 1));
            }
            // Bracket argument or comment (longer than `#` or `[`).
            if let Some((open, equals)) = self.match_bracket_open() {
                let comment = byte == b'#';
                // Compare against competing rules: the bracket opener is
                // always at least as long as an unquoted match starting with
                // `[` because unquoted cannot continue with `[` right after
                // `[=*`.
                self.advance(open);
                let close: Vec<u8> = std::iter::once(b']')
                    .chain(std::iter::repeat_n(b'=', equals))
                    .chain(std::iter::once(b']'))
                    .collect();
                let rest = &self.src[self.pos..];
                let found = rest.windows(close.len()).position(|window| window == close);
                let Some(found) = found else {
                    let text = self.text(self.pos, self.src.len());
                    self.advance(self.src.len() - self.pos);
                    return Some(Token {
                        kind: Tok::BadBracket,
                        text,
                        line,
                        column,
                    });
                };
                let text = self.text(self.pos, self.pos + found);
                self.advance(found + close.len());
                return Some(Token {
                    kind: if comment {
                        Tok::CommentBracket
                    } else {
                        Tok::ArgBracket
                    },
                    text,
                    line,
                    column,
                });
            }
            if byte == b'#' {
                while self.at(self.pos).is_some_and(|byte| byte != b'\n' && byte != 0) {
                    self.advance(1);
                }
                continue;
            }
            if byte == b'(' {
                return Some(self.token(Tok::ParenLeft, 1));
            }
            if byte == b')' {
                return Some(self.token(Tok::ParenRight, 1));
            }
            if byte == b'"' {
                self.advance(1);
                let mut value = Vec::new();
                loop {
                    match self.at(self.pos) {
                        None | Some(0) => {
                            return Some(Token {
                                kind: Tok::BadString,
                                text: String::from_utf8_lossy(&value).into_owned(),
                                line,
                                column,
                            });
                        }
                        Some(b'"') => {
                            self.advance(1);
                            return Some(Token {
                                kind: Tok::ArgQuoted,
                                text: String::from_utf8_lossy(&value).into_owned(),
                                line,
                                column,
                            });
                        }
                        Some(b'\\') => match self.at(self.pos + 1) {
                            Some(b'\n') => self.advance(2),
                            Some(next) if next != 0 => {
                                value.extend_from_slice(&[b'\\', next]);
                                self.advance(2);
                            }
                            _ => {
                                value.push(b'\\');
                                self.advance(1);
                            }
                        },
                        Some(other) => {
                            value.push(other);
                            self.advance(1);
                        }
                    }
                }
            }
            if matches!(byte, b' ' | b'\t' | b'\r') {
                let mut length = 0;
                while matches!(self.at(self.pos + length), Some(b' ' | b'\t' | b'\r')) {
                    length += 1;
                }
                return Some(self.token(Tok::Space, length));
            }
            let identifier = self.match_identifier();
            let unquoted = self.match_unquoted().max(self.match_legacy());
            if identifier > 0 && identifier >= unquoted {
                return Some(self.token(Tok::Identifier, identifier));
            }
            if unquoted > 0 {
                return Some(self.token(Tok::ArgUnquoted, unquoted));
            }
            if byte == b'[' {
                return Some(self.token(Tok::ArgUnquoted, 1));
            }
            let length = std::str::from_utf8(&self.src[self.pos..])
                .ok()
                .and_then(|text| text.chars().next())
                .map_or(1, char::len_utf8);
            return Some(self.token(Tok::BadCharacter, length));
        }
    }
}

/// Parses a list file. Returns the commands and any diagnostics; when the
/// final diagnostic is not a warning, parsing failed.
pub fn parse(path: &str, text: &str) -> (Option<Vec<Command>>, Vec<ParseDiag>) {
    let bytes = text.as_bytes();
    let bytes = bytes.strip_prefix(b"\xef\xbb\xbf").unwrap_or(bytes);
    let mut lexer = Lexer {
        src: bytes,
        pos: 0,
        line: 1,
        column: 1,
    };
    let mut diags = Vec::new();
    let mut commands = Vec::new();
    let mut have_newline = true;
    let _ = path;
    while let Some(token) = lexer.next() {
        match token.kind {
            Tok::Space => {}
            Tok::Newline => have_newline = true,
            Tok::CommentBracket => have_newline = false,
            Tok::Identifier if have_newline => {
                have_newline = false;
                match parse_function(&mut lexer, token.text, token.line, &mut diags) {
                    Some(command) => commands.push(command),
                    None => return (None, diags),
                }
            }
            Tok::Identifier => {
                diags.push(ParseDiag {
                    line: token.line,
                    message: format!(
                        "Parse error.  Expected a newline, got {} with text \"{}\".",
                        token.kind.describe(),
                        token.text
                    ),
                    warning: false,
                    command: None,
                });
                return (None, diags);
            }
            other => {
                diags.push(ParseDiag {
                    line: token.line,
                    message: format!(
                        "Parse error.  Expected a command name, got {} with text \"{}\".",
                        other.describe(),
                        token.text
                    ),
                    warning: false,
                    command: None,
                });
                return (None, diags);
            }
        }
    }
    if let Some((line, name)) = check_nesting(&commands) {
        diags.push(ParseDiag {
            line,
            message: "Flow control statements are not properly nested.".into(),
            warning: false,
            command: Some(name),
        });
        return (None, diags);
    }
    (Some(commands), diags)
}

#[derive(PartialEq)]
enum Separation {
    Okay,
    Warning,
    Error,
}

fn parse_function(
    lexer: &mut Lexer<'_>,
    name: String,
    line: usize,
    diags: &mut Vec<ParseDiag>,
) -> Option<Command> {
    let token = loop {
        match lexer.next() {
            Some(token) if token.kind == Tok::Space => {}
            other => break other,
        }
    };
    let Some(token) = token else {
        diags.push(ParseDiag {
            line: lexer.line,
            message: "Unexpected end of file.\nParse error.  Function missing opening \"(\"."
                .into(),
            warning: false,
            command: None,
        });
        return None;
    };
    if token.kind != Tok::ParenLeft {
        diags.push(ParseDiag {
            line: token.line,
            message: format!(
                "Parse error.  Expected \"(\", got {} with text \"{}\".",
                token.kind.describe(),
                token.text
            ),
            warning: false,
            command: None,
        });
        return None;
    }
    let mut args = Vec::new();
    let mut depth = 0usize;
    let mut separation = Separation::Okay;
    let add = |args: &mut Vec<Arg>,
               diags: &mut Vec<ParseDiag>,
               separation: &Separation,
               token: &Token,
               delim: Delim|
     -> bool {
        args.push(Arg {
            value: token.text.clone(),
            delim,
            line: token.line,
        });
        if *separation == Separation::Okay {
            return true;
        }
        let error = *separation == Separation::Error || delim == Delim::Bracket;
        diags.push(ParseDiag {
            line: token.line,
            message: format!(
                "Syntax {} in cmake code at column {}\nArgument not separated from preceding token by whitespace.",
                if error { "Error" } else { "Warning" },
                token.column
            ),
            warning: !error,
            command: None,
        });
        !error
    };
    while let Some(token) = lexer.next() {
        match token.kind {
            Tok::Space | Tok::Newline => separation = Separation::Okay,
            Tok::ParenLeft => {
                depth += 1;
                separation = Separation::Okay;
                if !add(&mut args, diags, &separation, &token, Delim::Unquoted) {
                    return None;
                }
            }
            Tok::ParenRight => {
                if depth == 0 {
                    let lname = name.to_ascii_lowercase();
                    return Some(Command {
                        name,
                        lname,
                        line,
                        line_end: token.line,
                        args,
                    });
                }
                depth -= 1;
                separation = Separation::Okay;
                if !add(&mut args, diags, &separation, &token, Delim::Unquoted) {
                    return None;
                }
                separation = Separation::Warning;
            }
            Tok::Identifier | Tok::ArgUnquoted => {
                if !add(&mut args, diags, &separation, &token, Delim::Unquoted) {
                    return None;
                }
                separation = Separation::Warning;
            }
            Tok::ArgQuoted => {
                if !add(&mut args, diags, &separation, &token, Delim::Quoted) {
                    return None;
                }
                separation = Separation::Warning;
            }
            Tok::ArgBracket => {
                if !add(&mut args, diags, &separation, &token, Delim::Bracket) {
                    return None;
                }
                separation = Separation::Error;
            }
            Tok::CommentBracket => separation = Separation::Error,
            other => {
                diags.push(ParseDiag {
                    line: token.line,
                    message: format!(
                        "Parse error.  Function missing ending \")\".  Instead found {} with text \"{}\".",
                        other.describe(),
                        token.text
                    ),
                    warning: false,
                    command: None,
                });
                return None;
            }
        }
    }
    diags.push(ParseDiag {
        line,
        message: "Parse error.  Function missing ending \")\".  End of file reached.".into(),
        warning: false,
        command: None,
    });
    None
}

/// Mirrors `cmListFileParser::CheckNesting`: returns the line of the first
/// badly nested flow-control command.
/// `cmListFileParser::CheckNesting`: the (line, original name) of the
/// offending command, or of the innermost unclosed block.
fn check_nesting(commands: &[Command]) -> Option<(usize, String)> {
    #[derive(PartialEq, Clone, Copy)]
    enum Kind {
        If,
        Else,
        While,
        Foreach,
        Function,
        Macro,
        Block,
    }
    let mut stack: Vec<(Kind, &Command)> = Vec::new();
    let here = |command: &Command| Some((command.line, command.name.clone()));
    for command in commands {
        let top = stack.last().map(|(kind, _)| *kind);
        let close = |stack: &mut Vec<(Kind, &Command)>, kinds: &[Kind]| {
            if top.is_some_and(|kind| kinds.contains(&kind)) {
                stack.pop();
                true
            } else {
                false
            }
        };
        match command.lname.as_str() {
            "if" => stack.push((Kind::If, command)),
            "elseif" | "else" => {
                if top != Some(Kind::If) {
                    return here(command);
                }
                let kind = if command.lname == "else" { Kind::Else } else { Kind::If };
                *stack.last_mut().expect("top") = (kind, command);
            }
            "endif" => {
                if !close(&mut stack, &[Kind::If, Kind::Else]) {
                    return here(command);
                }
            }
            "while" => stack.push((Kind::While, command)),
            "endwhile" => {
                if !close(&mut stack, &[Kind::While]) {
                    return here(command);
                }
            }
            "foreach" => stack.push((Kind::Foreach, command)),
            "endforeach" => {
                if !close(&mut stack, &[Kind::Foreach]) {
                    return here(command);
                }
            }
            "function" => stack.push((Kind::Function, command)),
            "endfunction" => {
                if !close(&mut stack, &[Kind::Function]) {
                    return here(command);
                }
            }
            "macro" => stack.push((Kind::Macro, command)),
            "endmacro" => {
                if !close(&mut stack, &[Kind::Macro]) {
                    return here(command);
                }
            }
            "block" => stack.push((Kind::Block, command)),
            "endblock" => {
                if !close(&mut stack, &[Kind::Block]) {
                    return here(command);
                }
            }
            _ => {}
        }
    }
    stack.last().and_then(|(_, command)| here(command))
}

pub type SharedCommands = Rc<Vec<Command>>;

#[cfg(test)]
mod tests {
    use super::*;

    fn args(text: &str) -> Vec<(String, Delim)> {
        let (commands, diags) = parse("t", text);
        assert!(diags.iter().all(|diag| diag.warning), "{diags:?}");
        commands.unwrap()[0]
            .args
            .iter()
            .map(|arg| (arg.value.clone(), arg.delim))
            .collect()
    }

    #[test]
    fn legacy_and_brackets() {
        assert_eq!(
            args("f(a\"b c\"d [[x]] [=[y]]=] \"q\\\"\\\nz\" $(V)w [ a[[b]])\n"),
            vec![
                ("a\"b c\"d".into(), Delim::Unquoted),
                ("x".into(), Delim::Bracket),
                ("y]".into(), Delim::Bracket),
                ("q\\\"z".into(), Delim::Quoted),
                ("$(V)w".into(), Delim::Unquoted),
                ("[".into(), Delim::Unquoted),
                ("a[[b]]".into(), Delim::Unquoted),
            ]
        );
    }

    #[test]
    fn nesting() {
        let (commands, diags) = parse("t", "if(a)\nendforeach()\n");
        assert!(commands.is_none());
        assert_eq!(diags[0].line, 2);
    }
}
