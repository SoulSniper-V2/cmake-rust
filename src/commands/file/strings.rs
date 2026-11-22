//! file(STRINGS), including cmHexFileConverter input records.

use std::collections::VecDeque;
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Cursor, Read};

use crate::cmregex::{Match, Regex};
use crate::interp::{CmdErr, CmdResult, Interp, err};
use crate::messages::MessageType;
use crate::policy::{PolicyStatus, policy_warning};
use crate::state::{clear_matches, store_matches};

use super::{err_name_var, source_path};
use crate::commands::configure_file::bom;

#[derive(Default)]
struct Options {
    input: Option<usize>,
    output: Option<usize>,
    count: usize,
    min: usize,
    max: usize,
    regex: Option<Regex>,
    newline: bool,
    raw: bool,
    encoding: String,
    store: bool,
}

pub(super) fn run(interp: &mut Interp, args: &[String]) -> CmdResult {
    if args.len() < 3 {
        return err_name_var("STRINGS");
    }
    let path = source_path(interp, &args[1]);
    let mut options = Options {
        store: true,
        ..Options::default()
    };
    let mut pending = "";
    for arg in &args[3..] {
        match arg.as_str() {
            "LIMIT_INPUT" | "LIMIT_OUTPUT" | "LIMIT_COUNT" | "LENGTH_MINIMUM"
            | "LENGTH_MAXIMUM" | "REGEX" | "ENCODING" => pending = arg,
            "NEWLINE_CONSUME" => {
                options.newline = true;
                pending = "";
            }
            "NO_HEX_CONVERSION" => {
                options.raw = true;
                pending = "";
            }
            _ => {
                match pending {
                    "REGEX" => match Regex::new(arg) {
                        Ok(regex) => {
                            options.regex = Some(regex);
                            let policy = interp.policy_status("CMP0159");
                            options.store = policy == PolicyStatus::New;
                            if policy == PolicyStatus::Warn
                                && interp.policy_optional_warning(
                                    interp.get_def("CMAKE_POLICY_WARNING_CMP0159"),
                                )
                            {
                                interp.issue(MessageType::AuthorWarning, &format!(
                                    "{}\nFor compatibility, CMake is leaving CMAKE_MATCH_<n> unchanged.",
                                    policy_warning(159)
                                ));
                            }
                        }
                        Err(_) => {
                            return err(format!(
                                "STRINGS option REGEX value \"{arg}\" could not be compiled."
                            ));
                        }
                    },
                    "ENCODING" => {
                        if !matches!(
                            arg.as_str(),
                            "UTF-8" | "UTF-16LE" | "UTF-16BE" | "UTF-32LE" | "UTF-32BE"
                        ) {
                            return err(format!(
                                "STRINGS option ENCODING \"{arg}\" not recognized."
                            ));
                        }
                        options.encoding.clone_from(arg);
                    }
                    "" => return err(format!("STRINGS given unknown argument \"{arg}\"")),
                    _ => {
                        let Some(num) = parse_int(arg).filter(|num| *num >= 0) else {
                            return err(format!(
                                "STRINGS option {pending} value \"{arg}\" is not an unsigned integer."
                            ));
                        };
                        let num = num as usize;
                        match pending {
                            "LIMIT_INPUT" => options.input = Some(num),
                            "LIMIT_OUTPUT" => options.output = Some(num),
                            "LIMIT_COUNT" => options.count = num,
                            "LENGTH_MINIMUM" => options.min = num,
                            _ => options.max = num,
                        }
                    }
                }
                pending = "";
            }
        }
    }

    let file = File::open(&path)
        .map_err(|_| CmdErr::Error(format!("STRINGS file \"{path}\" cannot be read.")))?;
    let mut reader = BufReader::new(file);
    let binary = format!(
        "{}/CMakeFiles/FileCommandStringsBinaryFile",
        interp.current_binary_dir()
    );
    let reader: Box<dyn BufRead> = if !options.raw && first_hex_record(&mut reader) {
        let mut data = Vec::new();
        let read = reader.read_to_end(&mut data);
        let converted = read.ok().and_then(|_| hex_records(&data));
        if let Some(converted) = converted.filter(|converted| fs::write(&binary, converted).is_ok())
        {
            Box::new(Cursor::new(converted))
        } else {
            Box::new(Cursor::new(data))
        }
    } else {
        Box::new(reader)
    };
    let mut input = Input {
        reader,
        putback: VecDeque::new(),
        position: 0,
    };
    let encoding = input.read_bom();
    let encoding = if options.encoding.is_empty() {
        encoding
    } else {
        &options.encoding
    };
    let limit = options.input.unwrap_or(usize::MAX);
    let mut collector = Collector {
        options: &options,
        values: Vec::new(),
        size: 0,
    };
    let mut current = Vec::new();
    let mut full = false;
    while input.position < limit && collector.more() {
        let Some(code) = input.codepoint(encoding) else {
            break;
        };
        if code == u32::from(b'\r') {
            continue;
        }
        let ascii = (32..=126).contains(&code)
            || code == u32::from(b'\t')
            || code == u32::from(b'\n') && options.newline;
        if ascii {
            current.push(code as u8);
        } else {
            match input.utf8_tail(code, encoding) {
                Ok((bytes, length)) => current.extend_from_slice(&bytes[..length]),
                Err(delimiter) => {
                    let newline = delimiter == Some(b'\n') && !options.newline;
                    full = !collector.push(interp, &mut current, newline);
                }
            }
        }
        if options.max > 0 && current.len() == options.max {
            full = !collector.push(interp, &mut current, false);
        }
        if full {
            break;
        }
    }
    if !full && collector.more() {
        collector.push(interp, &mut current, false);
    }
    interp.set_def(&args[2], collector.values.join(";"));
    Ok(())
}

struct Collector<'a> {
    options: &'a Options,
    values: Vec<String>,
    size: usize,
}

impl Collector<'_> {
    fn more(&self) -> bool {
        self.options.count == 0 || self.values.len() < self.options.count
    }

    fn push(&mut self, interp: &mut Interp, current: &mut Vec<u8>, allow_empty: bool) -> bool {
        let current = std::mem::take(current);
        if current.len() < self.options.min || (!allow_empty && current.is_empty()) {
            return true;
        }
        let value = String::from_utf8_lossy(&current);
        let found = if let Some(regex) = &self.options.regex {
            let Some(found) = regex.find(&value) else {
                return true;
            };
            found
        } else {
            Match::default()
        };
        if self.options.store {
            clear_matches(interp);
            store_matches(interp, &value, &found);
        }
        self.size += current.len() + 1;
        if self.options.output.is_some_and(|limit| self.size >= limit) {
            return false;
        }
        self.values.push(value.replace(';', "\\;"));
        true
    }
}

fn parse_int(value: &str) -> Option<i32> {
    let trimmed = value.trim_start();
    let first = usize::from(trimmed.starts_with(['+', '-']));
    let digits = trimmed[first..]
        .bytes()
        .take_while(u8::is_ascii_digit)
        .count();
    if digits == 0 {
        return None;
    }
    trimmed[..first + digits]
        .parse::<i64>()
        .ok()
        .map(|value| value as i32)
}

struct Input {
    reader: Box<dyn BufRead>,
    putback: VecDeque<u8>,
    position: usize,
}

impl Input {
    fn get(&mut self) -> Option<u8> {
        let byte = if let Some(byte) = self.putback.pop_front() {
            byte
        } else {
            let byte = *self.reader.fill_buf().ok()?.first()?;
            self.reader.consume(1);
            byte
        };
        self.position += 1;
        Some(byte)
    }

    fn unread(&mut self, bytes: &[u8]) {
        for &byte in bytes.iter().rev() {
            self.putback.push_front(byte);
        }
        self.position -= bytes.len();
    }

    fn read_bom(&mut self) -> &'static str {
        let mut data = [0; 4];
        let mut size = 0;
        for next in &mut data {
            let Some(byte) = self.get() else {
                break;
            };
            *next = byte;
            size += 1;
        }
        let (skip, encoding) = bom(&data[..size]);
        self.unread(&data[skip..size]);
        encoding
    }

    fn codepoint(&mut self, encoding: &str) -> Option<u32> {
        let width = match encoding {
            "UTF-16LE" | "UTF-16BE" => 2,
            "UTF-32LE" | "UTF-32BE" => 4,
            _ => 1,
        };
        let mut value = u32::from(self.get()?);
        for _ in 1..width {
            let Some(byte) = self.get() else {
                break;
            };
            value = (value << 8) | u32::from(byte);
        }
        Some(match encoding {
            "UTF-16LE" => (value as u16).swap_bytes() as u32,
            "UTF-32LE" => value.swap_bytes(),
            _ => value,
        })
    }

    fn utf8_tail(&mut self, code: u32, encoding: &str) -> Result<([u8; 4], usize), Option<u8>> {
        let width = match (encoding, code) {
            ("UTF-8", 0xc0..=0xdf) => 2,
            ("UTF-8", 0xe0..=0xef) => 3,
            ("UTF-8", 0xf0..=0xf7) => 4,
            _ => return Err(u8::try_from(code).ok()),
        };
        let mut sequence = [code as u8, 0, 0, 0];
        for i in 1..width {
            let next = self.get();
            if let Some(byte) = next.filter(|byte| byte & 0xc0 == 0x80) {
                sequence[i] = byte;
            } else {
                if let Some(byte) = next {
                    sequence[i] = byte;
                    self.unread(&sequence[1..=i]);
                }
                return Err(next);
            }
        }
        Ok((sequence, width))
    }
}

fn first_hex_record(input: &mut impl BufRead) -> bool {
    input.fill_buf().ok().is_some_and(|bytes| {
        let first = bytes
            .split(|byte| *byte == b'\n')
            .next()
            .unwrap_or_default();
        hex_records(first).is_some()
    })
}

fn hex_records(data: &[u8]) -> Option<Vec<u8>> {
    let mut lines = data
        .strip_suffix(b"\n")
        .unwrap_or(data)
        .split(|byte| *byte == b'\n');
    let trim = |line: &[u8]| usize::from(line.ends_with(b"\r"));
    let first = lines.next()?;
    let first = &first[..first.len() - trim(first)];
    let intel = match first.first()? {
        b':' => true,
        b'S' => false,
        _ => return None,
    };
    let valid_length = |line: &[u8]| {
        if intel {
            (11..=523).contains(&line.len())
        } else {
            (10..=526).contains(&line.len())
        }
    };
    if !valid_length(first) || !first[1..].iter().all(u8::is_ascii_hexdigit) {
        return None;
    }
    let mut output = Vec::new();
    for line in std::iter::once(first).chain(lines) {
        let line = &line[..line.len() - trim(line)];
        if !valid_length(line) {
            return None;
        }
        let begin = if intel {
            if line.len() % 2 != 1 || line[0] != b':' || line[7] != b'0' {
                return None;
            }
            match line[8] {
                b'0' | b'1' => 9,
                b'2'..=b'5' => continue,
                _ => return None,
            }
        } else {
            if line.len() % 2 != 0 || line[0] != b'S' {
                return None;
            }
            match line[1] {
                b'1' => 8,
                b'2' => 10,
                b'3' => 12,
                b'5' | b'7'..=b'9' => continue,
                _ => return None,
            }
        };
        for pair in line
            .get(begin..line.len().saturating_sub(2))
            .unwrap_or_default()
            .chunks_exact(2)
        {
            let text = std::str::from_utf8(pair).ok()?;
            output.push(u8::from_str_radix(text, 16).ok()?);
        }
    }
    Some(output)
}
