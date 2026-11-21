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
    // RANGE
    if args.len() >= 2 && args[1] == "RANGE" {
        let parse = |text: &str| -> Option<i64> {
            let (value, ok) = util::strtol(text, 10);
            ok.then_some(value)
        };
        let numbers: Vec<Option<i64>> = args[2..].iter().map(|text| parse(text)).collect();
        let (start, stop, step) = match numbers.as_slice() {
            [Some(stop)] => (0, *stop, 1),
            [Some(start), Some(stop)] => (*start, *stop, 1),
            [Some(start), Some(stop), Some(step)] => (*start, *stop, *step),
            _ => {
                let text = if numbers.iter().any(Option::is_none) && !numbers.is_empty() && numbers.len() <= 3 {
                    format!(
                        "foreach called with incorrect range specification: {}",
                        args[2..].join(" ")
                    )
                } else {
                    "foreach called with incorrect number of arguments".to_owned()
                };
                let _ = text;
                interp.issue(
                    MessageType::FatalError,
                    "foreach called with incorrect number of arguments",
                );
                interp.set_fatal_if_script();
                return None;
            }
        };
        let step = if step == 0 {
            if start > stop { -1 } else { 1 }
        } else {
            step
        };
        if (start > stop && step > 0) || (start < stop && step < 0) {
            let text = format!(
                "called with incorrect range specification: start {start}, stop {stop}, step {step}"
            );
            interp.issue(MessageType::FatalError, &format!("foreach {text}"));
            interp.set_fatal_if_script();
            return None;
        }
        let mut iterations = Vec::new();
        let mut value = start;
        loop {
            if (step > 0 && value > stop) || (step < 0 && value < stop) {
                break;
            }
            iterations.push(vec![Some(value.to_string())]);
            value += step;
        }
        return Some(ForeachPlan {
            vars: vec![args[0].clone()],
            iterations,
        });
    }
    // HandleInMode: IN [LISTS ...] [ITEMS ...] | IN ZIP_LISTS ...
    if let Some(position) = args.iter().position(|arg| arg == "IN") {
        return foreach_in_mode(interp, &args[..position], &args[position + 1..]);
    }
    Some(ForeachPlan {
        vars: vec![args[0].clone()],
        iterations: args[1..].iter().map(|item| vec![Some(item.clone())]).collect(),
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
    let fatal = |interp: &mut Interp, text: &str| {
        interp.issue(MessageType::FatalError, text);
        interp.set_fatal_if_script();
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
                return fatal(interp, "ITEMS or LISTS require exactly one iteration variable");
            }
            doing = if arg == "LISTS" { Doing::Lists } else { Doing::Items };
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
            &format!("Expected {vars_count} list variables, but given {}", args.len()),
        );
    }
    if !zip {
        // ReplayItems asserts exactly one variable; with none of the keywords
        // given (`foreach(a b IN)`) there is nothing to iterate.
        return Some(ForeachPlan {
            vars: vars.iter().take(1).cloned().collect(),
            iterations: if vars_count == 1 {
                args.into_iter().map(|item| vec![Some(item)]).collect()
            } else {
                Vec::new()
            },
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
        (0..lists.len()).map(|index| format!("{}_{index}", vars[0])).collect()
    };
    let iterations = (0..longest)
        .map(|index| lists.iter().map(|list| list.get(index).cloned()).collect())
        .collect();
    Some(ForeachPlan { vars: names, iterations })
}

pub struct BlockOptions {
    pub variables: bool,
    pub policies: bool,
    pub propagate: Vec<String>,
}

/// Parses `block([SCOPE_FOR [POLICIES] [VARIABLES]] [PROPAGATE <var>...])`.
pub fn parse_block(args: &[String]) -> Result<BlockOptions, String> {
    let mut options = BlockOptions {
        variables: true,
        policies: true,
        propagate: Vec::new(),
    };
    let mut index = 0;
    let mut scope_for: Option<(bool, bool)> = None;
    while index < args.len() {
        match args[index].as_str() {
            "SCOPE_FOR" => {
                let mut variables = false;
                let mut policies = false;
                index += 1;
                while index < args.len() && matches!(args[index].as_str(), "VARIABLES" | "POLICIES") {
                    if args[index] == "VARIABLES" {
                        variables = true;
                    } else {
                        policies = true;
                    }
                    index += 1;
                }
                scope_for = Some((variables, policies));
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
    if let Some((variables, policies)) = scope_for {
        options.variables = variables;
        options.policies = policies;
    }
    if !options.variables && !options.propagate.is_empty() {
        return Err("PROPAGATE cannot be specified without a new scope for VARIABLES".into());
    }
    Ok(options)
}
