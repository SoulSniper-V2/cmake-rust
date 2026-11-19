use std::path::{Path, PathBuf};

use crate::{Result, fail};

#[derive(Clone, Debug)]
pub struct Location {
    pub path: PathBuf,
    pub line: usize,
}

impl std::fmt::Display for Location {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}:{}", self.path.display(), self.line)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Form {
    Bare,
    Quoted,
    Bracket,
}

#[derive(Clone, Debug)]
pub struct Argument {
    pub text: String,
    pub form: Form,
}

#[derive(Clone, Debug)]
pub struct Command {
    pub name: String,
    pub args: Vec<Argument>,
    pub location: Location,
}

pub fn parse(path: &Path, text: &str) -> Result<Vec<Command>> {
    let mut parser = Parser {
        text: text.strip_prefix('\u{feff}').unwrap_or(text),
        index: 0,
        line: 1,
        path,
    };
    let mut commands = Vec::new();
    while parser.skip_layout()? {
        let location = parser.location();
        let name = parser.take_while(|ch| ch.is_ascii_alphanumeric() || ch == '_');
        if name.is_empty() || name.starts_with(|ch: char| ch.is_ascii_digit()) {
            return fail(format!("{location}: expected a command name"));
        }
        parser.skip_layout()?;
        if parser.next() != Some('(') {
            return fail(format!("{location}: expected '(' after {name}"));
        }
        let mut args = Vec::new();
        let mut depth = 0;
        loop {
            parser.skip_layout()?;
            let arg = match parser.peek() {
                Some(')') if depth == 0 => {
                    parser.next();
                    break;
                }
                Some('(') => {
                    depth += 1;
                    parser.punctuation("(")
                }
                Some(')') => {
                    depth -= 1;
                    parser.punctuation(")")
                }
                Some('"') => {
                    parser.next();
                    Argument {
                        text: parser.quoted()?,
                        form: Form::Quoted,
                    }
                }
                Some('[') if parser.bracket_open().is_some() => Argument {
                    text: parser.bracket()?,
                    form: Form::Bracket,
                },
                Some(_) => Argument {
                    text: parser.bare()?,
                    form: Form::Bare,
                },
                None => return fail(format!("{location}: missing ')' for {name}")),
            };
            args.push(arg);
        }
        commands.push(Command {
            name: name.to_ascii_lowercase(),
            args,
            location,
        });
    }
    Ok(commands)
}

struct Parser<'a> {
    text: &'a str,
    index: usize,
    line: usize,
    path: &'a Path,
}

impl<'a> Parser<'a> {
    fn location(&self) -> Location {
        Location {
            path: self.path.to_owned(),
            line: self.line,
        }
    }

    fn peek(&self) -> Option<char> {
        self.text[self.index..].chars().next()
    }

    fn next(&mut self) -> Option<char> {
        let ch = self.peek()?;
        self.index += ch.len_utf8();
        self.line += usize::from(ch == '\n');
        Some(ch)
    }

    fn take_while(&mut self, predicate: impl Fn(char) -> bool) -> &'a str {
        let start = self.index;
        while self.peek().is_some_and(&predicate) {
            self.next();
        }
        &self.text[start..self.index]
    }

    fn skip_layout(&mut self) -> Result<bool> {
        loop {
            self.take_while(char::is_whitespace);
            if self.peek() != Some('#') {
                return Ok(self.peek().is_some());
            }
            self.next();
            if self.bracket_open().is_some() {
                self.bracket()?;
            } else {
                self.take_while(|ch| ch != '\n');
            }
        }
    }

    fn punctuation(&mut self, text: &str) -> Argument {
        self.next();
        Argument {
            text: text.to_owned(),
            form: Form::Bare,
        }
    }

    fn bracket_open(&self) -> Option<usize> {
        let remaining = self.text[self.index..].strip_prefix('[')?;
        let equals = remaining.bytes().take_while(|byte| *byte == b'=').count();
        remaining[equals..].starts_with('[').then_some(equals)
    }

    fn bracket(&mut self) -> Result<String> {
        let location = self.location();
        let equals = self.bracket_open().expect("checked bracket opener");
        self.index += equals + 2;
        if self.text[self.index..].starts_with("\r\n") {
            self.next();
        }
        if self.peek() == Some('\n') {
            self.next();
        }
        let terminator = format!("]{}]", "=".repeat(equals));
        let remaining = &self.text[self.index..];
        let Some(end) = remaining.find(&terminator) else {
            return fail(format!("{location}: missing {terminator}"));
        };
        self.index += end + terminator.len();
        self.line += remaining[..end]
            .bytes()
            .filter(|byte| *byte == b'\n')
            .count();
        Ok(remaining[..end].to_owned())
    }

    fn quoted(&mut self) -> Result<String> {
        let location = self.location();
        let mut result = String::new();
        while let Some(ch) = self.next() {
            match ch {
                '"' => return Ok(result),
                '\\' => {
                    result.push(ch);
                    if let Some(escaped) = self.next() {
                        result.push(escaped);
                    }
                }
                _ => result.push(ch),
            }
        }
        fail(format!("{location}: unterminated quoted argument"))
    }

    fn bare(&mut self) -> Result<String> {
        let mut result = String::new();
        while let Some(ch) = self.peek() {
            if ch.is_whitespace() || matches!(ch, '(' | ')' | '#') {
                break;
            }
            self.next();
            result.push(ch);
            if ch == '\\' {
                let Some(escaped) = self.next() else {
                    return fail(format!("{}: dangling escape", self.location()));
                };
                result.push(escaped);
            }
        }
        Ok(result)
    }
}

pub fn split_list(value: &str) -> Vec<String> {
    let mut items = Vec::new();
    let mut item = String::new();
    let mut chars = value.chars().peekable();
    let mut brackets = 0;
    while let Some(ch) = chars.next() {
        match ch {
            '\\' if chars.peek() == Some(&';') => {
                chars.next();
                item.push(';');
            }
            ';' if brackets == 0 => items.push(std::mem::take(&mut item)),
            '[' => {
                brackets += 1;
                item.push(ch);
            }
            ']' => {
                brackets -= 1;
                item.push(ch);
            }
            _ => item.push(ch),
        }
    }
    items.push(item);
    items
}
