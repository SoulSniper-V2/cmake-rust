//! The `string()` command, ported from `cmStringCommand.cxx` (plus
//! `cmStringReplaceHelper`, `cmTimestamp`, `cmUuid`, `cmCryptoHash`, and
//! `cmGeneratorExpression::Preprocess`).

mod json;
mod misc;
mod regex_replace;

use crate::interp::{CmdResult, Interp, err};
use crate::messages::MessageType;
use crate::policy::PolicyStatus;
use crate::state::{clear_matches, store_matches};
use crate::util;
use json::handle_json;
use misc::{handle_random, handle_timestamp, handle_uuid};

pub fn register(interp: &mut Interp) {
    interp.register("string", string_command);
}

/// C `atoi`: `strtol` truncated to `int`.
fn atoi(text: &str) -> i32 {
    util::strtol(text, 10).0 as i32
}

/// `cmIsSpace` (`isspace` over an unsigned char in the C locale).
fn is_space(byte: u8) -> bool {
    matches!(byte, b' ' | b'\t' | b'\n' | 0x0b | 0x0c | b'\r')
}

/// `cmStrToULong`: skip leading space, reject '-', `strtoul` base 10
/// requiring full consumption and no overflow.
fn str_to_ulong(text: &str) -> Option<u64> {
    let bytes = text.as_bytes();
    let mut index = 0;
    while index < bytes.len() && is_space(bytes[index]) {
        index += 1;
    }
    if bytes.get(index) == Some(&b'-') {
        return None;
    }
    if bytes.get(index) == Some(&b'+') {
        index += 1;
    }
    let start = index;
    let mut value: u64 = 0;
    let mut overflow = false;
    while index < bytes.len() && bytes[index].is_ascii_digit() {
        let digit = u64::from(bytes[index] - b'0');
        match value
            .checked_mul(10)
            .and_then(|value| value.checked_add(digit))
        {
            Some(next) => value = next,
            None => overflow = true,
        }
        index += 1;
    }
    if index == start || index != bytes.len() || overflow {
        return None;
    }
    Some(value)
}

/// `cmJoin` over a slice of the argument vector.
fn cm_join(args: &[String], separator: &str, initial: &str) -> String {
    let mut result = String::from(initial);
    for (index, item) in args.iter().enumerate() {
        if index > 0 {
            result.push_str(separator);
        }
        result.push_str(item);
    }
    result
}

fn string_command(interp: &mut Interp, args: &[String]) -> CmdResult {
    if args.is_empty() {
        return err("must be called with at least one argument.");
    }
    match args[0].as_str() {
        "REGEX" => handle_regex(interp, args),
        "REPLACE" => handle_replace(interp, args),
        "MD5" | "SHA1" | "SHA224" | "SHA256" | "SHA384" | "SHA512" | "SHA3_224" | "SHA3_256"
        | "SHA3_384" | "SHA3_512" => handle_hash(interp, args),
        "TOLOWER" => handle_case(interp, args, false),
        "TOUPPER" => handle_case(interp, args, true),
        "COMPARE" => handle_compare(interp, args),
        "ASCII" => handle_ascii(interp, args),
        "HEX" => handle_hex(interp, args),
        "CONFIGURE" => handle_configure(interp, args),
        "LENGTH" => handle_length(interp, args),
        "APPEND" => handle_append(interp, args),
        "PREPEND" => handle_prepend(interp, args),
        "CONCAT" => handle_concat(interp, args),
        "JOIN" => handle_join(interp, args),
        "SUBSTRING" => handle_substring(interp, args),
        "STRIP" => handle_strip(interp, args),
        "REPEAT" => handle_repeat(interp, args),
        "RANDOM" => handle_random(interp, args),
        "FIND" => handle_find(interp, args),
        "TIMESTAMP" => handle_timestamp(interp, args),
        "MAKE_C_IDENTIFIER" => handle_make_c_identifier(interp, args),
        "GENEX_STRIP" => handle_genex_strip(interp, args),
        "UUID" => handle_uuid(interp, args),
        "JSON" => handle_json(interp, args),
        other => err(format!("does not recognize sub-command {other}")),
    }
}

fn handle_hash(interp: &mut Interp, args: &[String]) -> CmdResult {
    if args.len() != 3 {
        return err(format!(
            "{} requires an output variable and an input string",
            args[0]
        ));
    }
    let out = misc::hash_string(&args[0], &args[2]);
    interp.set_def(args[1].clone(), out);
    Ok(())
}

fn handle_case(interp: &mut Interp, args: &[String], upper: bool) -> CmdResult {
    if args.len() < 3 {
        return err("no output variable specified");
    }
    let output = if upper {
        args[1].to_ascii_uppercase()
    } else {
        args[1].to_ascii_lowercase()
    };
    interp.set_def(args[2].clone(), output);
    Ok(())
}

fn handle_ascii(interp: &mut Interp, args: &[String]) -> CmdResult {
    if args.len() < 3 {
        return err("No output variable specified");
    }
    let outvar = args.last().unwrap();
    let mut output = Vec::new();
    for arg in &args[1..args.len() - 1] {
        let ch = atoi(arg);
        if ch > 0 && ch < 256 {
            output.push(ch as u8);
        } else {
            return err(format!("Character with code {arg} does not exist."));
        }
    }
    interp.set_def(
        outvar.clone(),
        String::from_utf8_lossy(&output).into_owned(),
    );
    Ok(())
}

fn handle_hex(interp: &mut Interp, args: &[String]) -> CmdResult {
    if args.len() != 3 {
        return err("Incorrect number of arguments");
    }
    let mut output = String::with_capacity(args[1].len() * 2);
    for byte in args[1].as_bytes() {
        output.push_str(&format!("{byte:02x}"));
    }
    interp.set_def(args[2].clone(), output);
    Ok(())
}

/// `cmMakefile::cmDefineRegex` / `cmDefine01Regex`, hand-expanded because the
/// patterns are fixed: `#([ \t]*)cmakedefine[ \t]+([A-Za-z_0-9]*)` (and
/// `cmakedefine01`), `find` semantics = leftmost match over the line.
/// Returns (indent, name, match_start_of_#) on success.
fn find_cmakedefine(line: &str, define01: bool) -> Option<(String, String)> {
    let bytes = line.as_bytes();
    let keyword: &[u8] = if define01 {
        b"cmakedefine01"
    } else {
        b"cmakedefine"
    };
    let mut pos = 0;
    while let Some(found) = line[pos..].find('#') {
        let hash = pos + found;
        let mut cursor = hash + 1;
        let indent_start = cursor;
        while cursor < bytes.len() && matches!(bytes[cursor], b' ' | b'\t') {
            cursor += 1;
        }
        let indent = &line[indent_start..cursor];
        if line.as_bytes()[cursor..].starts_with(keyword) {
            cursor += keyword.len();
            if cursor < bytes.len() && matches!(bytes[cursor], b' ' | b'\t') {
                while cursor < bytes.len() && matches!(bytes[cursor], b' ' | b'\t') {
                    cursor += 1;
                }
                let name_start = cursor;
                while cursor < bytes.len()
                    && (bytes[cursor].is_ascii_alphanumeric() || bytes[cursor] == b'_')
                {
                    cursor += 1;
                }
                return Some((indent.to_owned(), line[name_start..cursor].to_owned()));
            }
        }
        pos = hash + 1;
    }
    None
}

/// `cmMakefile::ConfigureString`: per-line `#cmakedefine` handling.
fn configure_string(
    interp: &mut Interp,
    input: &str,
    _at_only: bool,
    _escape_quotes: bool,
) -> String {
    let mut output = String::new();
    for line in input.split_inclusive('\n') {
        let (text, newline) = match line.strip_suffix('\n') {
            Some(text) => (text, true),
            None => (line, false),
        };
        let mut line_out;
        if let Some((indent, name)) = find_cmakedefine(text, false) {
            let def = interp.get_def(&name).map(str::to_owned);
            if !util::is_off_opt(def.as_deref()) {
                line_out = text.replace(
                    &format!("#{indent}cmakedefine"),
                    &format!("#{indent}define"),
                );
            } else {
                line_out = format!("/* #undef {name} */");
            }
        } else if let Some((indent, name)) = find_cmakedefine(text, true) {
            let def = interp.get_def(&name).map(str::to_owned);
            line_out = text.replace(
                &format!("#{indent}cmakedefine01"),
                &format!("#{indent}define"),
            );
            line_out.push_str(if !util::is_off_opt(def.as_deref()) {
                " 1"
            } else {
                " 0"
            });
        } else {
            line_out = text.to_owned();
        }
        output.push_str(&line_out);
        if newline {
            output.push('\n');
        }
    }
    output
}

fn handle_configure(interp: &mut Interp, args: &[String]) -> CmdResult {
    if args.len() < 2 {
        return err("No input string specified.");
    }
    if args.len() < 3 {
        return err("No output variable specified.");
    }
    let mut escape_quotes = false;
    let mut at_only = false;
    for arg in &args[3..] {
        match arg.as_str() {
            "@ONLY" => at_only = true,
            "ESCAPE_QUOTES" => escape_quotes = true,
            other => {
                return err(format!("Unrecognized argument \"{other}\""));
            }
        }
    }
    let configured = configure_string(interp, &args[1], at_only, escape_quotes);
    let output = match interp.expand_configure(&configured, at_only, escape_quotes) {
        Ok(output) => output,
        Err(message) => {
            interp.issue(MessageType::FatalError, &message);
            interp.set_fatal();
            return Err(crate::interp::CmdErr::Reported);
        }
    };
    interp.set_def(args[2].clone(), output);
    Ok(())
}

fn handle_regex(interp: &mut Interp, args: &[String]) -> CmdResult {
    if args.len() < 2 {
        return err("sub-command REGEX requires a mode to be specified.");
    }
    match args[1].as_str() {
        "MATCH" => {
            if args.len() < 5 {
                return err(
                    "sub-command REGEX, mode MATCH needs at least 5 arguments total to command.",
                );
            }
            regex_match(interp, args)
        }
        "MATCHALL" => {
            if args.len() < 5 {
                return err(
                    "sub-command REGEX, mode MATCHALL needs at least 5 arguments total to command.",
                );
            }
            regex_match_all(interp, args)
        }
        "REPLACE" => {
            if args.len() < 6 {
                return err(
                    "sub-command REGEX, mode REPLACE needs at least 6 arguments total to command.",
                );
            }
            regex_replace::regex_replace(interp, args)
        }
        mode => err(format!("sub-command REGEX does not recognize mode {mode}")),
    }
}

/// CMP0186: OLD means `^` also matches at the offset of repeated searches.
fn old_bol(interp: &Interp) -> bool {
    interp.policy_status("CMP0186") != PolicyStatus::New
}

/// One step of the repeated-search loop shared by MATCHALL and REPLACE.
/// `nonempty_at_offset` rejects an empty match starting exactly at `base`
/// (kwsys NONEMPTY_AT_OFFSET); when rejected we resume scanning at `base + 1`
/// where `^` can no longer match (regbol stays at `base`).
fn regex_find_next(
    re: &crate::cmregex::Regex,
    input: &str,
    base: usize,
    nonempty_at_offset: bool,
    old_policy: bool,
) -> Option<crate::cmregex::Match> {
    re.find_with(input, base, old_policy, nonempty_at_offset)
}

fn regex_match(interp: &mut Interp, args: &[String]) -> CmdResult {
    let regex = &args[2];
    let outvar = &args[3];

    clear_matches(interp);
    let re = match crate::cmregex::Regex::new(regex) {
        Ok(re) => re,
        Err(_) => {
            return err(format!(
                "sub-command REGEX, mode MATCH failed to compile regex \"{regex}\"."
            ));
        }
    };

    let input = cm_join(&args[4..], "", "");
    let mut output = String::new();
    if let Some(found) = re.find_at(&input, 0, false) {
        store_matches(interp, &input, &found);
        if let Some((start, end)) = found.groups[0] {
            output = input.get(start..end).unwrap_or("").to_owned();
        }
    }
    interp.set_def(outvar.clone(), output);
    Ok(())
}

fn regex_match_all(interp: &mut Interp, args: &[String]) -> CmdResult {
    let regex = &args[2];
    let outvar = &args[3];

    clear_matches(interp);
    let re = match crate::cmregex::Regex::new(regex) {
        Ok(re) => re,
        Err(_) => {
            return err(format!(
                "sub-command REGEX, mode MATCHALL failed to compile regex \"{regex}\"."
            ));
        }
    };

    let input = cm_join(&args[4..], "", "");

    let old_policy = old_bol(interp);

    let mut output = String::new();
    let mut base = 0usize;
    let mut nonempty_at_offset = false;
    while let Some(found) = regex_find_next(&re, &input, base, nonempty_at_offset, old_policy) {
        clear_matches(interp);
        store_matches(interp, &input, &found);
        if !output.is_empty() || nonempty_at_offset {
            output.push(';');
        }
        let (start, end) = found.groups[0].unwrap_or((base, base));
        output.push_str(input.get(start..end).unwrap_or(""));
        base = end;
        if start == input.len() {
            break;
        }
        nonempty_at_offset = start == end;
    }

    interp.set_def(outvar.clone(), output);
    Ok(())
}

fn handle_find(interp: &mut Interp, args: &[String]) -> CmdResult {
    if args.len() < 4 || args.len() > 5 {
        return err("sub-command FIND requires 3 or 4 parameters.");
    }
    let reverse = args.len() == 5 && args[4] == "REVERSE";
    if args.len() == 5 && args[4] != "REVERSE" {
        return err("sub-command FIND: unknown last parameter");
    }
    let haystack = &args[1];
    let needle = &args[2];
    let outvar = &args[3];
    if outvar == "REVERSE" {
        return err(
            "sub-command FIND does not allow one to select REVERSE as the output variable.  \
             Maybe you missed the actual output variable?",
        );
    }
    let pos = if reverse {
        haystack.rfind(needle)
    } else {
        haystack.find(needle)
    };
    match pos {
        Some(pos) => interp.set_def(outvar.clone(), pos.to_string()),
        None => interp.set_def(outvar.clone(), "-1"),
    }
    Ok(())
}

fn handle_compare(interp: &mut Interp, args: &[String]) -> CmdResult {
    if args.len() < 2 {
        return err("sub-command COMPARE requires a mode to be specified.");
    }
    let mode = args[1].as_str();
    if matches!(
        mode,
        "EQUAL" | "NOTEQUAL" | "LESS" | "LESS_EQUAL" | "GREATER" | "GREATER_EQUAL"
    ) {
        if args.len() < 5 {
            return err(format!(
                "sub-command COMPARE, mode {mode} needs at least 5 arguments total to command."
            ));
        }
        let (left, right, outvar) = (&args[2], &args[3], &args[4]);
        let result = match mode {
            "LESS" => left < right,
            "LESS_EQUAL" => left <= right,
            "GREATER" => left > right,
            "GREATER_EQUAL" => left >= right,
            "EQUAL" => left == right,
            _ => left != right,
        };
        interp.set_def(outvar.clone(), if result { "1" } else { "0" });
        return Ok(());
    }
    err(format!(
        "sub-command COMPARE does not recognize mode {mode}"
    ))
}

fn handle_replace(interp: &mut Interp, args: &[String]) -> CmdResult {
    if args.len() < 5 {
        return err("sub-command REPLACE requires at least four arguments.");
    }
    let match_expression = args[1].as_str();
    let replace_expression = args[2].as_str();
    let variable_name = &args[3];
    let mut input = cm_join(&args[4..], "", "");
    if !match_expression.is_empty() {
        input = input.replace(match_expression, replace_expression);
    }
    interp.set_def(variable_name.clone(), input);
    Ok(())
}

fn handle_substring(interp: &mut Interp, args: &[String]) -> CmdResult {
    if args.len() != 5 {
        return err("sub-command SUBSTRING requires four arguments.");
    }
    let value = args[1].as_bytes();
    let begin = atoi(&args[2]);
    let end = atoi(&args[3]);
    let variable_name = &args[4];

    let length = value.len();
    if begin < 0 || begin as usize > length {
        return err(format!("begin index: {begin} is out of range 0 - {length}"));
    }
    if end < -1 {
        return err(format!("end index: {end} should be -1 or greater"));
    }
    let begin = begin as usize;
    let tail = &value[begin..];
    let count = if end == -1 {
        tail.len()
    } else {
        (end as usize).min(tail.len())
    };
    interp.set_def(
        variable_name.clone(),
        String::from_utf8_lossy(&tail[..count]).into_owned(),
    );
    Ok(())
}

fn handle_length(interp: &mut Interp, args: &[String]) -> CmdResult {
    if args.len() != 3 {
        return err("sub-command LENGTH requires two arguments.");
    }
    interp.set_def(args[2].clone(), args[1].len().to_string());
    Ok(())
}

fn handle_append(interp: &mut Interp, args: &[String]) -> CmdResult {
    if args.len() < 2 {
        return err("sub-command APPEND requires at least one argument.");
    }
    if args.len() < 3 {
        return Ok(());
    }
    let variable = &args[1];
    let old = interp.get_def(variable).unwrap_or_default().to_owned();
    let value = cm_join(&args[2..], "", &old);
    interp.set_def(variable.clone(), value);
    Ok(())
}

fn handle_prepend(interp: &mut Interp, args: &[String]) -> CmdResult {
    if args.len() < 2 {
        return err("sub-command PREPEND requires at least one argument.");
    }
    if args.len() < 3 {
        return Ok(());
    }
    let variable = &args[1];
    let mut value = cm_join(&args[2..], "", "");
    if let Some(old) = interp.get_def(variable) {
        value.push_str(old);
    }
    interp.set_def(variable.clone(), value);
    Ok(())
}

fn join_impl(interp: &mut Interp, args: &[String], glue: &str, var_idx: usize) -> CmdResult {
    let variable = &args[var_idx];
    let value = cm_join(&args[var_idx + 1..], glue, "");
    interp.set_def(variable.clone(), value);
    Ok(())
}

fn handle_concat(interp: &mut Interp, args: &[String]) -> CmdResult {
    if args.len() < 2 {
        return err("sub-command CONCAT requires at least one argument.");
    }
    join_impl(interp, args, "", 1)
}

fn handle_join(interp: &mut Interp, args: &[String]) -> CmdResult {
    if args.len() < 3 {
        return err("sub-command JOIN requires at least two arguments.");
    }
    join_impl(interp, args, &args[1].clone(), 2)
}

fn handle_make_c_identifier(interp: &mut Interp, args: &[String]) -> CmdResult {
    if args.len() != 3 {
        return err("sub-command MAKE_C_IDENTIFIER requires two arguments.");
    }
    let input = args[1].as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(input.len() + 1);
    if input.first().is_some_and(u8::is_ascii_digit) {
        out.push(b'_');
    }
    for &byte in input {
        if byte.is_ascii_alphanumeric() || byte == b'_' {
            out.push(byte);
        } else {
            out.push(b'_');
        }
    }
    interp.set_def(args[2].clone(), String::from_utf8_lossy(&out).into_owned());
    Ok(())
}

fn handle_genex_strip(interp: &mut Interp, args: &[String]) -> CmdResult {
    if args.len() != 3 {
        return err("sub-command GENEX_STRIP requires two arguments.");
    }
    let result = misc::strip_all_generator_expressions(&args[1]);
    interp.set_def(args[2].clone(), result);
    Ok(())
}

fn handle_strip(interp: &mut Interp, args: &[String]) -> CmdResult {
    if args.len() != 3 {
        return err("sub-command STRIP requires two arguments.");
    }
    let bytes = args[1].as_bytes();
    let mut start = bytes.len() + 1;
    let mut end = 0usize;
    for (index, &byte) in bytes.iter().enumerate() {
        if !is_space(byte) {
            if start > bytes.len() {
                start = index;
            }
            end = index;
        }
    }
    let (start, length) = if start > bytes.len() {
        (0, 0)
    } else {
        (start, end - start + 1)
    };
    interp.set_def(
        args[2].clone(),
        String::from_utf8_lossy(&bytes[start..start + length]).into_owned(),
    );
    Ok(())
}

fn handle_repeat(interp: &mut Interp, args: &[String]) -> CmdResult {
    // Upstream issues FATAL_ERROR and returns true for these failures.
    if args.len() != 4 {
        interp.issue(
            MessageType::FatalError,
            "sub-command REPEAT requires three arguments.",
        );
        interp.set_fatal();
        return Ok(());
    }
    let Some(times) = str_to_ulong(&args[2]) else {
        interp.issue(
            MessageType::FatalError,
            "repeat count is not a positive number.",
        );
        interp.set_fatal();
        return Ok(());
    };
    let value = args[1].as_bytes();
    let result: Vec<u8> = match value.len() {
        0 => Vec::new(),
        1 => vec![value[0]; times as usize],
        size => {
            let mut result = vec![0u8; size.saturating_mul(times as usize)];
            for i in 0..times as usize {
                result[i * size..(i + 1) * size].copy_from_slice(value);
            }
            result
        }
    };
    interp.set_def(
        args[3].clone(),
        String::from_utf8_lossy(&result).into_owned(),
    );
    Ok(())
}
