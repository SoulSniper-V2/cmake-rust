//! cmGeneratorExpressionNode's list operations on cmList strings.

use std::collections::HashSet;

use crate::cmregex::{Regex, ReplacePart, parse_replacement};
use crate::commands::list::{CaseSensitivity, CompareMethod, OrderMode, SortConfig};
use crate::policy::PolicyStatus;
use crate::util;

use super::{GenexHost, cm_list};

pub(super) fn dedup(mut values: Vec<String>) -> Vec<String> {
    let mut seen = HashSet::new();
    values.retain(|value| seen.insert(value.clone()));
    values
}

pub(super) fn filter(
    mut values: Vec<String>,
    op: &str,
    pattern: &str,
) -> Result<Vec<String>, String> {
    let regex = Regex::new(pattern)?;
    values.retain(|value| regex.find(value).is_some() == (op == "INCLUDE"));
    Ok(values)
}

pub(super) fn check_count(
    node: &str,
    op: &str,
    count: usize,
    required: usize,
    exact: bool,
) -> Result<(), String> {
    if count >= required && (!exact || count == required) {
        return Ok(());
    }
    let count_text = match required {
        1 => "one parameter",
        2 => "two parameters",
        3 => "three parameters",
        4 => "four parameters",
        _ => unreachable!(),
    };
    let limit = if exact { "exactly" } else { "at least" };
    Err(format!(
        "$<{node}:{op}> expression requires {limit} {count_text}."
    ))
}

fn number(value: &str) -> Result<i64, String> {
    value
        .trim_start_matches(|c: char| c.is_ascii_whitespace() || c == '\u{b}')
        .parse()
        .map_err(|_| format!("index: \"{value}\" is not a valid index"))
}

fn index(pos: i64, len: usize, insert: bool) -> Result<usize, String> {
    let adjusted = if pos < 0 { pos + len as i64 } else { pos };
    let end = len.saturating_sub(usize::from(!insert));
    if (insert || len != 0) && adjusted >= 0 && (adjusted as usize) <= end {
        return Ok(adjusted as usize);
    }
    let range = if len == 0 {
        "0, 0".to_owned()
    } else {
        format!("-{len}, {end}")
    };
    Err(format!("index: {pos} out of range ({range})"))
}

pub(super) fn evaluate(
    args: &[String],
    anchor: PolicyStatus,
    host: &mut dyn GenexHost,
) -> Result<String, String> {
    let [op, input, tail @ ..] = args else {
        unreachable!()
    };
    let (required, exact) = match op.as_str() {
        "LENGTH" | "POP_BACK" | "POP_FRONT" | "REMOVE_DUPLICATES" | "REVERSE" => (1, true),
        "JOIN" | "FIND" => (2, true),
        "SUBLIST" | "FILTER" => (3, true),
        "GET" | "APPEND" | "PREPEND" | "REMOVE_ITEM" | "REMOVE_AT" | "TRANSFORM" => (2, false),
        "INSERT" => (3, false),
        "SORT" => (1, false),
        _ => return Err(format!("{op}: invalid option.")),
    };
    check_count("LIST", op, args.len() - 1, required, exact)?;
    if op == "APPEND" || op == "PREPEND" {
        let added = tail.join(";");
        return Ok(if input.is_empty() {
            added
        } else if op == "APPEND" {
            format!("{input};{added}")
        } else {
            format!("{added};{input}")
        });
    }
    let mut values = cm_list(input, true);
    let len = values.len();
    match op.as_str() {
        "LENGTH" => return Ok(len.to_string()),
        "JOIN" => return Ok(values.join(&tail[0])),
        "FIND" => {
            return Ok(values
                .iter()
                .position(|v| v == &tail[0])
                .map_or_else(|| "-1".to_owned(), |i| i.to_string()));
        }
        "GET" | "REMOVE_AT" => {
            if op == "GET" && values.is_empty() {
                return Err("given empty list".to_owned());
            }
            let indexes = tail
                .iter()
                .flat_map(|arg| cm_list(arg, false))
                .map(|arg| number(&arg))
                .collect::<Result<Vec<_>, _>>()?;
            let indexes = indexes
                .into_iter()
                .map(|i| index(i, len, false))
                .collect::<Result<Vec<_>, _>>()?;
            if op == "GET" {
                return Ok(util::join_list(
                    &indexes.iter().map(|&i| &values[i]).collect::<Vec<_>>(),
                ));
            }
            let remove: HashSet<_> = indexes.into_iter().collect();
            values = values
                .into_iter()
                .enumerate()
                .filter_map(|(i, item)| (!remove.contains(&i)).then_some(item))
                .collect();
        }
        "SUBLIST" => {
            if values.is_empty() {
                return Ok(String::new());
            }
            let start = number(&tail[0])?;
            let count = number(&tail[1])?;
            if start < 0 {
                return Err(format!(
                    "begin index: {start} is out of range 0 - {}",
                    len - 1
                ));
            }
            if count < -1 {
                return Err(format!("length: {count} should be -1 or greater"));
            }
            if start as usize >= len {
                return Err(format!(
                    "begin index: {start} is out of range 0 - {}",
                    len - 1
                ));
            }
            let end = if count == -1 {
                len
            } else {
                (start as usize).saturating_add(count as usize).min(len)
            };
            return Ok(values[start as usize..end].join(";"));
        }
        "INSERT" => {
            let at = index(number(&tail[0])?, len, true)?;
            values.splice(at..at, tail[1..].iter().cloned());
        }
        "POP_BACK" => {
            values.pop();
        }
        "POP_FRONT" => {
            if !values.is_empty() {
                values.remove(0);
            }
        }
        "REMOVE_DUPLICATES" => values = dedup(values),
        "REMOVE_ITEM" => {
            let remove: HashSet<_> = tail.iter().flat_map(|arg| cm_list(arg, false)).collect();
            values.retain(|value| !remove.contains(value));
        }
        "FILTER" => {
            let mode = &tail[0];
            if !matches!(mode.as_str(), "INCLUDE" | "EXCLUDE") {
                return Err(format!(
                    "sub-command FILTER does not recognize operator \"{mode}\". It must be either INCLUDE or EXCLUDE."
                ));
            }
            values = filter(values, mode, &tail[1]).map_err(|_| {
                format!(
                    "sub-command FILTER, failed to compile regex \"{}\".",
                    tail[1]
                )
            })?;
        }
        "TRANSFORM" => transform(&mut values, tail, anchor, host)?,
        "REVERSE" => values.reverse(),
        "SORT" => sort(&mut values, tail)?,
        _ => unreachable!(),
    }
    Ok(values.join(";"))
}

fn sort(values: &mut [String], args: &[String]) -> Result<(), String> {
    let mut compare = None;
    let mut case = None;
    let mut order = None;
    for option in args {
        let (key, value) = option.split_once(':').unwrap_or((option, ""));
        let (setting, choices): (_, &[_]) = match key {
            "COMPARE" if option.contains(':') => {
                (&mut compare, &["STRING", "NATURAL", "FILE_BASENAME"])
            }
            "CASE" if option.contains(':') => (&mut case, &["SENSITIVE", "INSENSITIVE"]),
            "ORDER" if option.contains(':') => (&mut order, &["ASCENDING", "DESCENDING"]),
            _ => return Err(format!("sub-command SORT, option \"{option}\" is invalid.")),
        };
        if setting.is_some() {
            return Err(format!(
                "sub-command SORT, {key} option has been specified multiple times."
            ));
        }
        if !choices.contains(&value) {
            return Err(format!(
                "sub-command SORT, an invalid {key} option has been specified: \"{value}\"."
            ));
        }
        *setting = Some(value);
    }
    SortConfig {
        compare: match compare {
            Some("FILE_BASENAME") => CompareMethod::FileBasename,
            Some("NATURAL") => CompareMethod::Natural,
            _ => CompareMethod::String,
        },
        case: if case == Some("INSENSITIVE") {
            CaseSensitivity::Insensitive
        } else {
            CaseSensitivity::Sensitive
        },
        order: if order == Some("DESCENDING") {
            OrderMode::Descending
        } else {
            OrderMode::Ascending
        },
    }
    .sort(values);
    Ok(())
}

fn transform(
    values: &mut [String],
    args: &[String],
    anchor: PolicyStatus,
    host: &mut dyn GenexHost,
) -> Result<(), String> {
    if values.is_empty() {
        return Ok(());
    }
    let action = args[0].as_str();
    let arity = match action {
        "APPEND" | "PREPEND" => 1,
        "TOUPPER" | "TOLOWER" | "STRIP" => 0,
        "REPLACE" => 2,
        _ => return Err(format!(" sub-command TRANSFORM, {action} invalid action.")),
    };
    if args.len() - 1 < arity {
        return Err(format!(
            "sub-command TRANSFORM, action {action} expects {arity} argument(s)."
        ));
    }
    let action_args = &args[1..1 + arity];
    let indexes = select(values, &args[1 + arity..])?;
    let replace = if action == "REPLACE" {
        Some(Replacement::new(&action_args[0], &action_args[1], anchor)?)
    } else {
        None
    };
    for index in indexes {
        let value = &mut values[index];
        match action {
            "APPEND" => value.push_str(&action_args[0]),
            "PREPEND" => value.insert_str(0, &action_args[0]),
            "TOUPPER" => value.make_ascii_uppercase(),
            "TOLOWER" => value.make_ascii_lowercase(),
            "STRIP" => *value = crate::util::trim_whitespace(value).to_owned(),
            "REPLACE" => *value = replace.as_ref().unwrap().replace(value, host)?,
            _ => unreachable!(),
        }
    }
    Ok(())
}

fn select(values: &[String], args: &[String]) -> Result<Vec<usize>, String> {
    let len = values.len();
    let Some((selector, tail)) = args.split_first() else {
        return Ok((0..len).collect());
    };
    let normalize = |pos: i64| {
        let pos = if pos < 0 { pos + len as i64 } else { pos };
        if pos < 0 || pos as usize >= len {
            return Err(format!(
                "sub-command TRANSFORM, selector {selector}, index: {pos} out of range (-{len}, {}).",
                len - 1
            ));
        }
        Ok(pos as usize)
    };
    let mut rest = &tail[tail.len()..];
    // TransformSelectorFor: range validation happens in Validate, after the
    // parser has consumed the selector arguments and rejected leftovers.
    let mut for_selector: Option<(i64, i64, i64)> = None;
    let mut indexes = match selector.as_str() {
        "REGEX" => {
            let Some((pattern, remaining)) = tail.split_first() else {
                return Err(
                    "sub-command TRANSFORM, selector REGEX expects 'regular expression' argument."
                        .to_owned(),
                );
            };
            rest = remaining;
            let re = Regex::new(pattern).map_err(|_| {
                format!(
                    "sub-command TRANSFORM, selector REGEX failed to compile regex \"{pattern}\"."
                )
            })?;
            values
                .iter()
                .enumerate()
                .filter_map(|(i, value)| re.find(value).is_some().then_some(i))
                .collect()
        }
        "AT" => {
            let positions = tail
                .iter()
                .flat_map(|arg| cm_list(arg, false))
                .map(|arg| {
                    number(&arg).map_err(|_| {
                        format!("sub-command TRANSFORM, selector AT: '{arg}': unexpected argument.")
                    })
                })
                .collect::<Result<Vec<_>, _>>()?;
            if positions.is_empty() {
                return Err(
                    "sub-command TRANSFORM, selector AT expects at least one numeric value."
                        .to_owned(),
                );
            }
            positions
                .into_iter()
                .map(normalize)
                .collect::<Result<_, _>>()?
        }
        "FOR" => {
            if tail.len() < 2 {
                return Err(
                    "sub-command TRANSFORM, selector FOR expects, at least, two arguments."
                        .to_owned(),
                );
            }
            let required = |arg: &str| {
                number(arg).map_err(|_| {
                    "sub-command TRANSFORM, selector FOR expects, at least, two numeric values."
                        .to_owned()
                })
            };
            let start = required(&tail[0])?;
            let stop = required(&tail[1])?;
            // Try to read a third numeric value for step; a non-numeric
            // value sets step = -1 (cmGeneratorExpressionNode TRANSFORM).
            let step = tail.get(2).map_or(1, |arg| number(arg).unwrap_or(-1));
            if step <= 0 {
                return Err("sub-command TRANSFORM, selector FOR expects positive numeric value for <step>.".to_owned());
            }
            rest = &tail[tail.len().min(3)..];
            for_selector = Some((start, stop, step));
            Vec::new()
        }
        _ => return Err(unexpected(args)),
    };
    if let Some(next) = rest.first() {
        if matches!(next.as_str(), "AT" | "FOR" | "REGEX") {
            return Err(format!(
                "sub-command TRANSFORM, selector already specified ({selector})."
            ));
        }
        return Err(unexpected(rest));
    }
    if let Some((start, stop, step)) = for_selector {
        let start = normalize(start)?;
        let stop = normalize(stop)?;
        if start > stop {
            return Err(format!(
                "sub-command TRANSFORM, selector FOR expects <start> to be no greater than <stop> ({start} > {stop})"
            ));
        }
        indexes = (start..=stop).step_by(step as usize).collect();
    }
    Ok(indexes)
}

fn unexpected(args: &[String]) -> String {
    format!(
        "sub-command TRANSFORM, '{}': unexpected argument(s).",
        args.join(", ")
    )
}

struct Replacement<'a> {
    regex: Regex,
    pattern: &'a str,
    source: &'a str,
    parts: Vec<ReplacePart>,
    anchor: PolicyStatus,
}

impl<'a> Replacement<'a> {
    fn new(pattern: &'a str, source: &'a str, anchor: PolicyStatus) -> Result<Self, String> {
        let error = |message: &str| format!("sub-command TRANSFORM, action REPLACE: {message}.");
        let regex = Regex::new(pattern)
            .map_err(|_| error(&format!("Failed to compile regex \"{pattern}\"")))?;
        let parts = parse_replacement(source).map_err(|message| error(&message))?;
        Ok(Self {
            regex,
            pattern,
            source,
            parts,
            anchor,
        })
    }

    fn replace(&self, input: &str, host: &mut dyn GenexHost) -> Result<String, String> {
        let input_bytes = crate::bytes::encode(input);
        let mut result = Vec::new();
        let mut base = 0;
        let mut nonempty_at_offset = false;
        let bol_at_offset = self.anchor != PolicyStatus::New;
        while let Some(found) =
            self.regex
                .find_bytes_with(&input_bytes, base, bol_at_offset, nonempty_at_offset)
        {
            let (start, end) = found.groups[0].unwrap();
            host.record_regex_match(input, &found.groups);
            result.extend_from_slice(&input_bytes[base..start]);
            for part in &self.parts {
                match part {
                    ReplacePart::Literal(value) => {
                        result.extend_from_slice(&crate::bytes::encode(value))
                    }
                    ReplacePart::Capture(i) => {
                        if *i > self.regex.num_groups() {
                            return Err(format!(
                                "sub-command TRANSFORM, action REPLACE: replace expression \"{}\" contains an out-of-range escape for regex \"{}\".",
                                self.source, self.pattern
                            ));
                        }
                        if let Some((start, end)) = found.groups[*i] {
                            result.extend_from_slice(&input_bytes[start..end]);
                        }
                    }
                }
            }
            base = end;
            if start == input_bytes.len() {
                break;
            }
            nonempty_at_offset = start == end;
        }
        result.extend_from_slice(&input_bytes[base..]);
        Ok(crate::bytes::decode(&result))
    }
}
