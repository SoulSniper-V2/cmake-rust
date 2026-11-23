//! Argument handling for `foreach()` and `block()`.

use crate::interp::Interp;
use crate::messages::MessageType;
use crate::util;

pub struct ForeachPlan {
    pub vars: Vec<String>,
    /// Per iteration, the value of each loop variable; `None` removes the
    /// definition (an exhausted ZIP_LISTS list).
    pub iterations: Vec<Vec<Option<String>>>,
}

/// Computes the loop variables and per-iteration values for `foreach`
/// (port of `cmForEachCommand`). Issues errors itself and returns None on
/// failure.
pub fn foreach_plan(interp: &mut Interp, args: &[String]) -> Option<ForeachPlan> {
    if args.is_empty() {
        interp.issue(
            MessageType::FatalError,
            "foreach called with incorrect number of arguments",
        );
        interp.set_fatal_if_script();
        return None;
    }
    // cmForEachCommand looks for "IN" before considering RANGE, so
    // `foreach(x RANGE IN ...)` is IN-mode, not RANGE-mode.
    if let Some(position) = args.iter().position(|arg| arg == "IN") {
        return foreach_in_mode(interp, &args[..position], &args[position + 1..]);
    }
    // RANGE
    if args.len() >= 2 && args[1] == "RANGE" {
        return foreach_range(interp, args);
    }
    Some(ForeachPlan {
        vars: vec![args[0].clone()],
        iterations: args[1..]
            .iter()
            .map(|item| vec![Some(item.clone())])
            .collect(),
    })
}

/// `std::stoi` result as used by `TryParseInteger` in cmForEachCommand.cxx.
enum Stoi {
    Ok(i32),
    /// `std::invalid_argument`: no digits after optional whitespace/sign.
    Invalid,
    /// `std::out_of_range`: the value does not fit in `int`.
    OutOfRange,
}

/// `std::stoi(str)`: skips leading `isspace` characters (C locale), accepts
/// one optional sign, then requires at least one decimal digit. Trailing
/// characters are ignored (`"12x"` parses as 12). The parsed value must fit
/// in `int`.
fn stoi(text: &str) -> Stoi {
    let bytes = text.as_bytes();
    let mut index = 0;
    while index < bytes.len() && matches!(bytes[index], b' ' | b'\t' | b'\n' | 0x0b | 0x0c | b'\r')
    {
        index += 1;
    }
    let negative = match bytes.get(index) {
        Some(b'-') => {
            index += 1;
            true
        }
        Some(b'+') => {
            index += 1;
            false
        }
        _ => false,
    };
    let digits = index;
    let mut magnitude: u64 = 0;
    while index < bytes.len() && bytes[index].is_ascii_digit() {
        magnitude = magnitude
            .saturating_mul(10)
            .saturating_add(u64::from(bytes[index] - b'0'));
        index += 1;
    }
    if index == digits {
        return Stoi::Invalid;
    }
    // strtol accumulates into long and std::stoi range-checks against int;
    // any magnitude beyond the int bounds — including u64 saturation on
    // absurdly long inputs — is out_of_range either way.
    let value = if negative {
        -(magnitude as i128)
    } else {
        magnitude as i128
    };
    if value < i128::from(i32::MIN) || value > i128::from(i32::MAX) {
        Stoi::OutOfRange
    } else {
        Stoi::Ok(value as i32)
    }
}

/// The `args[1] == "RANGE"` branch of `cmForEachCommand`, including its
/// `TryParseInteger` diagnostics.
fn foreach_range(interp: &mut Interp, args: &[String]) -> Option<ForeachPlan> {
    // TryParseInteger: on failure the command error is `<name> <text>`
    // (cmMakefile::ExecuteCommand prefixes the command name) and the fatal
    // flag is set.
    let parse = |interp: &mut Interp, text: &str| -> Option<i32> {
        match stoi(text) {
            Stoi::Ok(value) => Some(value),
            Stoi::Invalid => {
                interp.issue(
                    MessageType::FatalError,
                    &format!("foreach Invalid integer: '{text}'"),
                );
                interp.set_fatal();
                None
            }
            Stoi::OutOfRange => {
                interp.issue(
                    MessageType::FatalError,
                    &format!("foreach Integer out of range: '{text}'"),
                );
                interp.set_fatal();
                None
            }
        }
    };
    // Only the 3/4/5-argument forms parse integers. Any other count leaves
    // start = stop = step = 0, which degenerates to a single "0" iteration.
    let mut start = 0i32;
    let mut stop = 0i32;
    let mut step = 0i32;
    match args.len() {
        3 => stop = parse(interp, &args[2])?,
        4 => {
            start = parse(interp, &args[2])?;
            stop = parse(interp, &args[3])?;
        }
        5 => {
            start = parse(interp, &args[2])?;
            stop = parse(interp, &args[3])?;
            step = parse(interp, &args[4])?;
        }
        _ => {}
    }
    let step = if step == 0 {
        if start > stop { -1 } else { 1 }
    } else {
        step
    };
    if (start > stop && step > 0) || (start < stop && step < 0) || step == 0 {
        interp.issue(
            MessageType::FatalError,
            &format!(
                "foreach called with incorrect range specification: start {start}, stop {stop}, step {step}"
            ),
        );
        interp.set_fatal();
        return None;
    }
    let mut iterations = Vec::new();
    let mut value = i64::from(start);
    let stop = i64::from(stop);
    let step = i64::from(step);
    loop {
        if (step > 0 && value > stop) || (step < 0 && value < stop) {
            break;
        }
        iterations.push(vec![Some(value.to_string())]);
        value += step;
    }
    Some(ForeachPlan {
        vars: vec![args[0].clone()],
        iterations,
    })
}

/// `HandleInMode` + `ReplayItems`/`ReplayZipLists` in cmForEachCommand.cxx.
fn foreach_in_mode(interp: &mut Interp, vars: &[String], rest: &[String]) -> Option<ForeachPlan> {
    #[derive(PartialEq)]
    enum Doing {
        None,
        Lists,
        Items,
        ZipLists,
    }
    // HandleInMode reports FATAL_ERROR but returns success without a
    // blocker; no fatal flag is set in any mode.
    let fatal = |interp: &mut Interp, text: &str| {
        interp.issue(MessageType::FatalError, text);
        None
    };
    let vars_count = vars.len();
    let mut doing = Doing::None;
    let mut zip = false;
    let mut args: Vec<String> = Vec::new();
    for arg in rest {
        if arg == "LISTS" || arg == "ITEMS" {
            if doing == Doing::ZipLists {
                return fatal(interp, "ZIP_LISTS can not be used with LISTS or ITEMS");
            }
            if vars_count != 1 {
                return fatal(
                    interp,
                    "ITEMS or LISTS require exactly one iteration variable",
                );
            }
            doing = if arg == "LISTS" {
                Doing::Lists
            } else {
                Doing::Items
            };
        } else if arg == "ZIP_LISTS" {
            if doing != Doing::None {
                return fatal(interp, "ZIP_LISTS can not be used with LISTS or ITEMS");
            }
            doing = Doing::ZipLists;
            zip = true;
        } else if doing == Doing::Lists {
            let value = interp.get_def(arg).unwrap_or_default().to_owned();
            if !value.is_empty() {
                util::expand_list_into(&value, true, &mut args);
            }
        } else if doing == Doing::Items || doing == Doing::ZipLists {
            args.push(arg.clone());
        } else {
            return fatal(interp, &format!("Unknown argument:\n  {arg}\n"));
        }
    }
    if doing == Doing::ZipLists && vars_count > 1 && vars_count != args.len() {
        return fatal(
            interp,
            &format!(
                "Expected {vars_count} list variables, but given {}",
                args.len()
            ),
        );
    }
    if !zip {
        // ReplayItems uses `Args.front()` as the loop variable and
        // `Args[1..]` — any additional variable names followed by the items —
        // as its values (the `IterationVarsCount == 1` assert is debug-only;
        // extra names reach here only via `foreach(a b IN)` with an empty
        // tail, which iterates `a` over `"b"`).
        let values: Vec<String> = vars.iter().skip(1).cloned().chain(args).collect();
        return Some(ForeachPlan {
            vars: vars.iter().take(1).cloned().collect(),
            iterations: values.into_iter().map(|item| vec![Some(item)]).collect(),
        });
    }
    let lists: Vec<Vec<String>> = args
        .iter()
        .map(|name| {
            let value = interp.get_def(name).unwrap_or_default();
            if value.is_empty() {
                Vec::new()
            } else {
                util::expand_list(value, true)
            }
        })
        .collect();
    let longest = lists.iter().map(Vec::len).max().unwrap_or(0);
    let names: Vec<String> = if vars_count > 1 {
        vars.to_vec()
    } else {
        // ReplayZipLists derives `var_N` names from `Args.front()` — the
        // first variable name, or the first list name when `IN` came first
        // (`foreach(IN ZIP_LISTS ...)`).
        let prefix = vars
            .first()
            .or_else(|| args.first())
            .cloned()
            .unwrap_or_default();
        (0..lists.len())
            .map(|index| format!("{prefix}_{index}"))
            .collect()
    };
    let iterations = (0..longest)
        .map(|index| lists.iter().map(|list| list.get(index).cloned()).collect())
        .collect();
    Some(ForeachPlan {
        vars: names,
        iterations,
    })
}

pub struct BlockOptions {
    pub variables: bool,
    pub policies: bool,
    pub propagate: Vec<String>,
}

/// Parses `block([SCOPE_FOR [POLICIES] [VARIABLES]] [PROPAGATE <var>...])`.
///
/// Mirrors `cmBlockCommand`'s cmArgumentParser use: `SCOPE_FOR` is a
/// non-empty multi-value keyword (unrecognized scopes are reported as
/// `SCOPE_FOR unsupported scope`), `PROPAGATE` a possibly-empty list.
pub fn parse_block(args: &[String]) -> Result<BlockOptions, String> {
    let mut options = BlockOptions {
        variables: true,
        policies: true,
        propagate: Vec::new(),
    };
    let mut index = 0;
    let mut scope_for: Option<Vec<String>> = None;
    while index < args.len() {
        match args[index].as_str() {
            "SCOPE_FOR" => {
                index += 1;
                let mut values = Vec::new();
                while index < args.len() && args[index] != "PROPAGATE" {
                    values.push(args[index].clone());
                    index += 1;
                }
                if values.is_empty() {
                    return Err(
                        "Error after keyword \"SCOPE_FOR\":\n  missing required value\n".into(),
                    );
                }
                scope_for = Some(values);
                continue;
            }
            "PROPAGATE" => {
                index += 1;
                while index < args.len() && args[index] != "SCOPE_FOR" {
                    options.propagate.push(args[index].clone());
                    index += 1;
                }
                continue;
            }
            other => return Err(format!("called with unsupported argument \"{other}\"")),
        }
    }
    if let Some(scopes) = scope_for {
        options.variables = false;
        options.policies = false;
        for scope in scopes {
            match scope.as_str() {
                "VARIABLES" => options.variables = true,
                "POLICIES" => options.policies = true,
                other => return Err(format!("SCOPE_FOR unsupported scope \"{other}\"")),
            }
        }
    }
    if !options.variables && !options.propagate.is_empty() {
        return Err("PROPAGATE cannot be specified without a new scope for VARIABLES".into());
    }
    Ok(options)
}
