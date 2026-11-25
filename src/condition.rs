//! `if()`/`while()` condition evaluation, a port of `cmConditionEvaluator`.
//! CMake 4.x always uses NEW behavior for CMP0012 and CMP0054.

use crate::interp::Interp;
use crate::messages::MessageType;
use crate::policy::PolicyStatus;
use crate::util::{self, CompareOp};

#[derive(Clone, Debug)]
struct CArg {
    value: String,
    quoted: bool,
}

type CondError = Option<(MessageType, String)>;

fn bool_str(value: bool) -> CArg {
    CArg {
        value: if value { "1" } else { "0" }.to_owned(),
        quoted: true,
    }
}

fn is_keyword(arg: &CArg, keyword: &str) -> bool {
    !arg.quoted && arg.value == keyword
}

fn get_definition_if_unquoted(interp: &mut Interp, arg: &CArg) -> Option<String> {
    if arg.quoted {
        None
    } else {
        interp.read_definition(&arg.value)
    }
}

fn get_variable_or_string(interp: &mut Interp, arg: &CArg) -> String {
    get_definition_if_unquoted(interp, arg).unwrap_or_else(|| arg.value.clone())
}

fn get_boolean_value(interp: &mut Interp, arg: &CArg) -> bool {
    if util::is_on(&arg.value) {
        return true;
    }
    if util::is_off(&arg.value) {
        return false;
    }
    if !arg.value.is_empty()
        && let Some(number) = util::strtod_full(&arg.value)
    {
        return number != 0.0;
    }
    !util::is_off_opt(get_definition_if_unquoted(interp, arg).as_deref())
}

/// Evaluates expanded arguments `(value, quoted)`. Returns the result and an
/// optional diagnostic.
pub fn evaluate(interp: &mut Interp, args: &[(String, bool)]) -> (bool, CondError) {
    let args: Vec<CArg> = args
        .iter()
        .map(|(value, quoted)| CArg {
            value: value.clone(),
            quoted: *quoted,
        })
        .collect();
    let mut error = None;
    let result = is_true(interp, args, &mut error);
    (result, error)
}

fn is_true(interp: &mut Interp, args: Vec<CArg>, error: &mut CondError) -> bool {
    if args.is_empty() {
        return false;
    }
    let mut list = args;
    type Level = fn(&mut Interp, &mut Vec<CArg>, &mut CondError) -> bool;
    let levels: [Level; 5] = [level0, level1, level2, level3, level4];
    for level in levels {
        loop {
            let before = list.len();
            if !level(interp, &mut list, error) {
                return false;
            }
            if list.len() >= before {
                break;
            }
        }
    }
    if list.len() != 1 {
        *error = Some((
            MessageType::FatalError,
            "Unknown arguments specified".into(),
        ));
        return false;
    }
    get_boolean_value(interp, &list[0])
}

fn level0(interp: &mut Interp, list: &mut Vec<CArg>, error: &mut CondError) -> bool {
    let mut index = 0;
    while index < list.len() {
        if is_keyword(&list[index], "(") {
            let mut depth = 1i32;
            let mut close = index + 1;
            while close < list.len() && depth != 0 {
                depth += i32::from(is_keyword(&list[close], "("))
                    - i32::from(is_keyword(&list[close], ")"));
                close += 1;
            }
            if depth != 0 {
                *error = Some((
                    MessageType::FatalError,
                    "mismatched parenthesis in condition".into(),
                ));
                return false;
            }
            // `close` is one past the matching ")".
            let sub: Vec<CArg> = list[index + 1..close - 1].to_vec();
            let value = is_true(interp, sub, error);
            list[index] = bool_str(value);
            list.drain(index + 1..close);
        }
        index += 1;
    }
    true
}

fn level1(interp: &mut Interp, list: &mut Vec<CArg>, _error: &mut CondError) -> bool {
    let mut index = 0;
    while index < list.len() {
        if index + 1 >= list.len() {
            index += 1;
            continue;
        }
        let current = &list[index];
        let next = list[index + 1].value.clone();
        let result: Option<bool> = if is_keyword(current, "EXISTS") {
            Some(!next.is_empty() && std::fs::metadata(&next).is_ok())
        } else if is_keyword(current, "IS_READABLE") {
            Some(access(&next, libc::R_OK))
        } else if is_keyword(current, "IS_WRITABLE") {
            Some(access(&next, libc::W_OK))
        } else if is_keyword(current, "IS_EXECUTABLE") {
            Some(access(&next, libc::X_OK))
        } else if is_keyword(current, "IS_DIRECTORY") {
            Some(std::fs::metadata(&next).is_ok_and(|meta| meta.is_dir()))
        } else if is_keyword(current, "IS_SYMLINK") {
            Some(std::fs::symlink_metadata(&next).is_ok_and(|meta| meta.file_type().is_symlink()))
        } else if is_keyword(current, "IS_ABSOLUTE") {
            Some(util::is_full_path(&next))
        } else if is_keyword(current, "COMMAND") {
            Some(interp.has_command(&next))
        } else if is_keyword(current, "POLICY") {
            Some(crate::policy::index_of(&next).is_some())
        } else if is_keyword(current, "TARGET") {
            Some(crate::state::find_target_to_use(interp, &next).is_some())
        } else if is_keyword(current, "DEFINED") {
            let len = next.len();
            if len >= 6 && next.starts_with("ENV{") && next.ends_with('}') {
                Some(std::env::var_os(&next[4..len - 1]).is_some())
            } else if len >= 8 && next.starts_with("CACHE{") && next.ends_with('}') {
                Some(interp.cache.get(&next[6..len - 1]).is_some())
            } else {
                let defined = interp.is_def_set(&next);
                if !defined && interp.ext.variable_watches.contains_key(&next) {
                    crate::commands::sysinfo::notify_variable(
                        interp,
                        &next,
                        "UNKNOWN_DEFINED_ACCESS",
                        "",
                    );
                }
                Some(defined)
            }
        } else if is_keyword(current, "TEST") {
            Some(crate::state::has_test(interp, &next))
        } else {
            None
        };
        if let Some(value) = result {
            list[index] = bool_str(value);
            list.remove(index + 1);
        }
        index += 1;
    }
    true
}

fn access(path: &str, mode: i32) -> bool {
    let Ok(c) = std::ffi::CString::new(path) else {
        return false;
    };
    // SAFETY: valid NUL-terminated path.
    unsafe { libc::access(c.as_ptr(), mode) == 0 }
}

fn match_key(arg: &CArg, keys: &[&str]) -> usize {
    for (index, key) in keys.iter().enumerate() {
        if is_keyword(arg, key) {
            return index + 1;
        }
    }
    0
}

fn compare<T: PartialOrd>(op: usize, lhs: T, rhs: T) -> bool {
    match op {
        1 => lhs < rhs,
        2 => lhs <= rhs,
        3 => lhs > rhs,
        4 => lhs >= rhs,
        5 => lhs == rhs,
        _ => false,
    }
}

fn level2(interp: &mut Interp, list: &mut Vec<CArg>, error: &mut CondError) -> bool {
    let mut index = 0;
    while index < list.len() {
        let has_next = index + 1 < list.len();
        let has_next2 = index + 2 < list.len();
        if has_next && is_keyword(&list[index], "MATCHES") {
            list[index] = bool_str(false);
            list.remove(index + 1);
            index += 1;
            continue;
        }
        if !has_next || !has_next2 {
            index += 1;
            continue;
        }
        let op = list[index + 1].clone();
        let lhs_arg = list[index].clone();
        let rhs_arg = list[index + 2].clone();
        let mut reduced: Option<bool> = None;
        if is_keyword(&op, "MATCHES") {
            let def = get_definition_if_unquoted(interp, &lhs_arg)
                .unwrap_or_else(|| lhs_arg.value.clone());
            crate::state::clear_matches(interp);
            let regex = match crate::cmregex::Regex::new(&rhs_arg.value) {
                Ok(regex) => regex,
                Err(_) => {
                    *error = Some((
                        MessageType::FatalError,
                        format!("Regular expression \"{}\" cannot compile", rhs_arg.value),
                    ));
                    return false;
                }
            };
            let found = regex.find(&def);
            if let Some(found) = &found {
                crate::state::store_matches(interp, &def, found);
            }
            reduced = Some(found.is_some());
        } else {
            let numeric = match_key(
                &op,
                &["LESS", "LESS_EQUAL", "GREATER", "GREATER_EQUAL", "EQUAL"],
            );
            if numeric > 0 {
                let l = get_variable_or_string(interp, &lhs_arg);
                let r = get_variable_or_string(interp, &rhs_arg);
                let result = match (util::scan_double(&l), util::scan_double(&r)) {
                    (Some(l), Some(r)) => compare(numeric, l, r),
                    _ => false,
                };
                reduced = Some(result);
            } else {
                let string = match_key(
                    &op,
                    &[
                        "STRLESS",
                        "STRLESS_EQUAL",
                        "STRGREATER",
                        "STRGREATER_EQUAL",
                        "STREQUAL",
                    ],
                );
                if string > 0 {
                    let l = get_variable_or_string(interp, &lhs_arg);
                    let r = get_variable_or_string(interp, &rhs_arg);
                    reduced = Some(compare(string, l.as_bytes(), r.as_bytes()));
                } else {
                    let version = match_key(
                        &op,
                        &[
                            "VERSION_LESS",
                            "VERSION_LESS_EQUAL",
                            "VERSION_GREATER",
                            "VERSION_GREATER_EQUAL",
                            "VERSION_EQUAL",
                        ],
                    );
                    if version > 0 {
                        let ops = [
                            CompareOp::Less,
                            CompareOp::LessEqual,
                            CompareOp::Greater,
                            CompareOp::GreaterEqual,
                            CompareOp::Equal,
                        ];
                        let l = get_variable_or_string(interp, &lhs_arg);
                        let r = get_variable_or_string(interp, &rhs_arg);
                        reduced = Some(util::version_compare(ops[version - 1], &l, &r));
                    } else if is_keyword(&op, "IS_NEWER_THAN") {
                        reduced = Some(file_is_newer(&lhs_arg.value, &rhs_arg.value));
                    } else if is_keyword(&op, "IN_LIST") {
                        let l = get_variable_or_string(interp, &lhs_arg);
                        let found = interp
                            .read_definition(&rhs_arg.value)
                            .is_some_and(|list| util::expand_list(&list, true).contains(&l));
                        reduced = Some(found);
                    } else if is_keyword(&op, "PATH_EQUAL") {
                        match interp.policy_status("CMP0139") {
                            PolicyStatus::New => {
                                let l = get_variable_or_string(interp, &lhs_arg);
                                let r = get_variable_or_string(interp, &rhs_arg);
                                reduced = Some(crate::state::path_equal(&l, &r));
                            }
                            PolicyStatus::Warn => {
                                let text = format!(
                                    "{}\nPATH_EQUAL will be interpreted as an operator when the policy is set to NEW.  Since the policy is not set the OLD behavior will be used.",
                                    crate::policy::policy_warning(
                                        crate::policy::index_of("CMP0139").expect("CMP0139")
                                    )
                                );
                                interp.issue(MessageType::AuthorWarning, &text);
                            }
                            PolicyStatus::Old => {}
                        }
                    }
                }
            }
        }
        if let Some(value) = reduced {
            list[index] = bool_str(value);
            list.remove(index + 2);
            list.remove(index + 1);
        }
        index += 1;
    }
    true
}

/// `cmSystemTools::FileTimeCompare`-based IS_NEWER_THAN: true if either
/// file is missing or lhs is newer or same age.
fn file_is_newer(lhs: &str, rhs: &str) -> bool {
    let (Ok(l), Ok(r)) = (std::fs::metadata(lhs), std::fs::metadata(rhs)) else {
        return true;
    };
    use std::os::unix::fs::MetadataExt;
    let lt = (l.mtime(), l.mtime_nsec());
    let rt = (r.mtime(), r.mtime_nsec());
    lt >= rt
}

fn level3(interp: &mut Interp, list: &mut Vec<CArg>, _error: &mut CondError) -> bool {
    let mut index = 0;
    while index + 1 < list.len() {
        if is_keyword(&list[index], "NOT") {
            let value = get_boolean_value(interp, &list[index + 1]);
            list[index] = bool_str(!value);
            list.remove(index + 1);
        }
        index += 1;
    }
    true
}

fn level4(interp: &mut Interp, list: &mut Vec<CArg>, _error: &mut CondError) -> bool {
    let mut index = 0;
    while index + 2 < list.len() {
        let op = match_key(&list[index + 1], &["AND", "OR"]);
        if op > 0 {
            let lhs = get_boolean_value(interp, &list[index]);
            let rhs = get_boolean_value(interp, &list[index + 2]);
            let value = if op == 1 { lhs && rhs } else { lhs || rhs };
            list[index] = bool_str(value);
            list.remove(index + 2);
            list.remove(index + 1);
        }
        index += 1;
    }
    true
}
