//! Argument handling for `foreach()` and `block()`.

use crate::interp::Interp;
use crate::messages::MessageType;
use crate::util;

pub struct ForeachPlan {
    pub vars: Vec<String>,
    pub iterations: Vec<Vec<String>>,
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
            iterations.push(vec![value.to_string()]);
            value += step;
        }
        return Some(ForeachPlan {
            vars: vec![args[0].clone()],
            iterations,
        });
    }
    // IN [LISTS ...] [ITEMS ...] [ZIP_LISTS ...]
    let in_position = args.iter().position(|arg| arg == "IN");
    if let Some(position) = in_position {
        let vars: Vec<String> = args[..position].to_vec();
        let rest = &args[position + 1..];
        if rest.first().map(String::as_str) == Some("ZIP_LISTS") {
            let lists: Vec<Vec<String>> = rest[1..]
                .iter()
                .map(|name| {
                    interp
                        .get_def(name)
                        .map(|value| util::expand_list(value, true))
                        .unwrap_or_default()
                })
                .collect();
            if vars.len() != 1 && vars.len() != lists.len() {
                interp.issue(
                    MessageType::FatalError,
                    "foreach Expected 1 or 2 loop variables, but got more.",
                );
                interp.set_fatal_if_script();
                return None;
            }
            let longest = lists.iter().map(Vec::len).max().unwrap_or(0);
            let mut iterations = Vec::new();
            for index in 0..longest {
                if vars.len() == 1 {
                    // Single variable: VAR_0, VAR_1, ...
                    iterations.push(
                        lists
                            .iter()
                            .map(|list| list.get(index).cloned().unwrap_or_default())
                            .collect(),
                    );
                } else {
                    iterations.push(
                        lists
                            .iter()
                            .map(|list| list.get(index).cloned().unwrap_or_default())
                            .collect(),
                    );
                }
            }
            let names = if vars.len() == 1 {
                (0..lists.len()).map(|index| format!("{}_{index}", vars[0])).collect()
            } else {
                vars
            };
            return Some(ForeachPlan {
                vars: names,
                iterations,
            });
        }
        if vars.len() != 1 {
            interp.issue(
                MessageType::FatalError,
                "foreach ITEMS or LISTS require ZIP_LISTS if more than one loop variable is used.",
            );
            interp.set_fatal_if_script();
            return None;
        }
        let mut items = Vec::new();
        let mut mode = 0; // 0 = items (before keyword), 1 = LISTS, 2 = ITEMS
        for arg in rest {
            if mode != 2 && arg == "LISTS" {
                mode = 1;
            } else if arg == "ITEMS" && mode != 2 {
                mode = 2;
            } else if mode == 1 {
                if let Some(value) = interp.get_def(arg) {
                    items.extend(util::expand_list(value, true));
                }
            } else {
                items.push(arg.clone());
            }
        }
        return Some(ForeachPlan {
            vars,
            iterations: items.into_iter().map(|item| vec![item]).collect(),
        });
    }
    Some(ForeachPlan {
        vars: vec![args[0].clone()],
        iterations: args[1..].iter().map(|item| vec![item.clone()]).collect(),
    })
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
