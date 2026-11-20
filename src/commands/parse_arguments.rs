//! `cmake_parse_arguments()` command: port of `cmParseArgumentsCommand.cxx`
//! (with the `cmArgumentParser` mechanics it relies on).

use std::collections::{BTreeMap, BTreeSet};

use crate::interp::{CmdResult, Interp};
use crate::messages::MessageType;
use crate::policy::PolicyStatus;
use crate::util;

pub fn register(interp: &mut Interp) {
    interp.register("cmake_parse_arguments", parse_arguments_command);
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum KeywordKind {
    /// `Bind(bool&)` — expect 0 values.
    Option,
    /// `Bind(std::string&)` — expect 1 value, stop after one.
    Single,
    /// `Bind(NonEmpty<std::vector<std::string>>&)` — expect >= 1, continue.
    Multi,
}

/// `cmParseArgumentsCommand`'s `UserArgumentParser` instance state.
struct ArgParser {
    /// keyword -> kind (the `KeywordActionMap`); shared across all three maps.
    keywords: BTreeMap<String, KeywordKind>,
    /// Currently active keyword name.
    keyword: String,
    keyword_values_seen: usize,
    keyword_values_expected: usize,
    /// The active keyword's `KeywordValueFunc` kind. `None` when there is no
    /// live value consumer (mirrors a null `KeywordValueFunc`).
    value_func: Option<KeywordKind>,
    done_with_positional: bool,
    /// Result maps in declaration order (the C++ code uses std::map, whose
    /// iteration order is by key; `PassParsedArguments` iterates them).
    options: BTreeMap<String, bool>,
    singles: BTreeMap<String, String>,
    multis: BTreeMap<String, Vec<String>>,
    keywords_seen: BTreeSet<String>,
    keywords_missing_values: BTreeSet<String>,
    unparsed: Vec<String>,
}

impl ArgParser {
    fn new() -> ArgParser {
        ArgParser {
            keywords: BTreeMap::new(),
            keyword: String::new(),
            keyword_values_seen: 0,
            keyword_values_expected: 0,
            value_func: None,
            done_with_positional: false,
            options: BTreeMap::new(),
            singles: BTreeMap::new(),
            multis: BTreeMap::new(),
            keywords_seen: BTreeSet::new(),
            keywords_missing_values: BTreeSet::new(),
            unparsed: Vec::new(),
        }
    }

    /// `UserArgumentParser::Bind(names, map, duplicateKey)`.
    /// Returns names whose keyword binding failed (duplicates).
    fn bind(&mut self, names: &[String], kind: KeywordKind) -> Vec<String> {
        let mut duplicates = Vec::new();
        for name in names {
            // `ref.emplace(key, T{})` — ensure an entry exists in the map.
            match kind {
                KeywordKind::Option => {
                    self.options.entry(name.clone()).or_insert(false);
                }
                KeywordKind::Single => {
                    self.singles.entry(name.clone()).or_default();
                }
                KeywordKind::Multi => {
                    self.multis.entry(name.clone()).or_default();
                }
            }
            // KeywordActionMap::Emplace: fails if keyword already bound.
            if self.keywords.contains_key(name) {
                duplicates.push(name.clone());
            } else {
                self.keywords.insert(name.clone(), kind);
            }
        }
        duplicates
    }

    /// `Instance::FinishKeyword`.
    fn finish_keyword(&mut self) {
        if !self.done_with_positional {
            return;
        }
        if self.keyword_values_seen < self.keyword_values_expected {
            self.keywords_missing_values.insert(self.keyword.clone());
        }
    }

    /// `Instance::Consume` (positions/trailing args unused here).
    fn consume(&mut self, arg: &str) {
        if let Some(&kind) = self.keywords.get(arg) {
            self.finish_keyword();
            self.keyword = arg.to_owned();
            self.keyword_values_seen = 0;
            self.done_with_positional = true;
            self.keywords_seen.insert(arg.to_owned());
            // `it->second(*this)`: the bound action for this keyword.
            match kind {
                KeywordKind::Option => {
                    if let Some(v) = self.options.get_mut(arg) {
                        *v = true;
                    }
                    self.value_func = None;
                    self.keyword_values_expected = 0;
                }
                KeywordKind::Single | KeywordKind::Multi => {
                    self.value_func = Some(kind);
                    self.keyword_values_expected = 1;
                }
            }
            return;
        }
        match self.value_func {
            Some(KeywordKind::Single) => {
                // Bind(std::string&) → set, Continue::No.
                if let Some(v) = self.singles.get_mut(&self.keyword.clone()) {
                    *v = arg.to_owned();
                }
                self.value_func = None;
                self.keyword_values_seen += 1;
            }
            Some(KeywordKind::Multi) => {
                // Bind(NonEmpty<vector>) → push, Continue::Yes.
                if let Some(v) = self.multis.get_mut(&self.keyword.clone()) {
                    v.push(arg.to_owned());
                }
                self.keyword_values_seen += 1;
            }
            _ => {
                self.unparsed.push(arg.to_owned());
            }
        }
    }
}

/// `cmStrToULong`: `strtoul(str, &end, 10)` must consume the whole string
/// without overflow (errno == 0).
fn str_to_ulong(s: &str) -> Option<u64> {
    let b = s.as_bytes();
    let mut i = 0usize;
    while i < b.len() && b[i].is_ascii_whitespace() {
        i += 1;
    }
    let mut negative = false;
    if i < b.len() && (b[i] == b'+' || b[i] == b'-') {
        negative = b[i] == b'-';
        i += 1;
    }
    let start = i;
    let mut value: u64 = 0;
    let mut overflow = false;
    while i < b.len() && b[i].is_ascii_digit() {
        value = value
            .checked_mul(10)
            .and_then(|v| v.checked_add((b[i] - b'0') as u64))
            .unwrap_or_else(|| {
                overflow = true;
                u64::MAX
            });
        i += 1;
    }
    if i == start || i != b.len() || overflow {
        return None;
    }
    Some(if negative { value.wrapping_neg() } else { value })
}

fn escape_arg(arg: &str) -> String {
    arg.replace(';', "\\;")
}

fn join_list(items: &[String], escape: bool) -> String {
    if escape {
        util::join_list(
            &items
                .iter()
                .map(|a| escape_arg(a))
                .collect::<Vec<String>>(),
        )
    } else {
        util::join_list(items)
    }
}

fn parse_arguments_command(interp: &mut Interp, args: &[String]) -> CmdResult {
    // cmake_parse_arguments(prefix options single multi <ARGN>)
    // or
    // cmake_parse_arguments(PARSE_ARGV N prefix options single multi)
    if args.len() < 4 {
        return crate::interp::err("must be called with at least 4 arguments.");
    }

    let mut iter = 0usize;
    let mut parse_from_argv = false;
    let mut argv_start: u64 = 0;
    if args[0] == "PARSE_ARGV" {
        if args.len() != 6 {
            interp.issue(
                MessageType::FatalError,
                "PARSE_ARGV must be called with exactly 6 arguments.",
            );
            interp.set_fatal();
            return Ok(());
        }
        parse_from_argv = true;
        iter += 1;
        match str_to_ulong(&args[iter]) {
            Some(v) => argv_start = v,
            None => {
                interp.issue(
                    MessageType::FatalError,
                    &format!("PARSE_ARGV index '{}' is not an unsigned integer", args[iter]),
                );
                interp.set_fatal();
                return Ok(());
            }
        }
        iter += 1;
    }

    let prefix = format!("{}_", args[iter]);
    iter += 1;

    let mut parser = ArgParser::new();
    let duplicate_key = |interp: &mut Interp, key: &str| {
        interp.issue(
            MessageType::Warning,
            &format!("keyword defined more than once: {key}"),
        );
    };

    // second argument: list of options without argument
    let names = util::expand_list(&args[iter], false);
    iter += 1;
    for key in parser.bind(&names, KeywordKind::Option) {
        duplicate_key(interp, &key);
    }
    // third: single argument options
    let names = util::expand_list(&args[iter], false);
    iter += 1;
    for key in parser.bind(&names, KeywordKind::Single) {
        duplicate_key(interp, &key);
    }
    // fourth: multi argument options
    let names = util::expand_list(&args[iter], false);
    iter += 1;
    for key in parser.bind(&names, KeywordKind::Multi) {
        duplicate_key(interp, &key);
    }

    // Flatten the trailing arguments into a single list of elements.
    let mut list: Vec<String> = Vec::new();
    if !parse_from_argv {
        for arg in &args[iter..] {
            util::expand_list_into(arg, false, &mut list);
        }
    } else {
        let argc = interp.get_safe("ARGC");
        let Some(count) = str_to_ulong(&argc) else {
            interp.issue(
                MessageType::FatalError,
                &format!("PARSE_ARGV called with ARGC='{argc}' that is not an unsigned integer"),
            );
            interp.set_fatal();
            return Ok(());
        };
        for i in argv_start..count {
            let arg_name = format!("ARGV{i}");
            let Some(arg) = interp.get_def(&arg_name) else {
                interp.issue(
                    MessageType::FatalError,
                    &format!("PARSE_ARGV called with {arg_name} not set"),
                );
                interp.set_fatal();
                return Ok(());
            };
            list.push(arg.to_owned());
        }
    }

    for arg in &list {
        parser.consume(arg);
    }
    parser.finish_keyword();

    // PassParsedArguments
    for (key, seen) in &parser.options {
        interp.set_def(
            format!("{prefix}{key}"),
            if *seen { "TRUE" } else { "FALSE" },
        );
    }

    let cmp0174 = interp.policy_status("CMP0174");
    for (key, value) in &parser.singles {
        let name = format!("{prefix}{key}");
        if !parser.keywords_seen.contains(key) {
            interp.unset_def(&name);
        } else if (parse_from_argv && cmp0174 == PolicyStatus::New) || !value.is_empty() {
            interp.set_def(name, value.clone());
        } else {
            // OLD behavior doesn't define a variable for an empty/missing
            // value, and we can't differentiate between those two cases.
            if parse_from_argv && cmp0174 == PolicyStatus::Warn {
                interp.issue(
                    MessageType::AuthorWarning,
                    &format!(
                        "The {key} keyword was followed by an empty string or no value at all. Policy CMP0174 is not set, so cmake_parse_arguments() will unset the {name} variable rather than setting it to an empty string."
                    ),
                );
            }
            interp.unset_def(&name);
        }
    }

    for (key, values) in &parser.multis {
        let name = format!("{prefix}{key}");
        if !values.is_empty() {
            interp.set_def(name, join_list(values, parse_from_argv));
        } else {
            interp.unset_def(&name);
        }
    }

    if !parser.unparsed.is_empty() {
        interp.set_def(
            format!("{prefix}UNPARSED_ARGUMENTS"),
            join_list(&parser.unparsed, parse_from_argv),
        );
    } else {
        interp.unset_def(&format!("{prefix}UNPARSED_ARGUMENTS"));
    }

    if !parser.keywords_missing_values.is_empty() {
        let missing: Vec<String> = parser.keywords_missing_values.iter().cloned().collect();
        interp.set_def(
            format!("{prefix}KEYWORDS_MISSING_VALUES"),
            util::join_list(&missing),
        );
    } else {
        interp.unset_def(&format!("{prefix}KEYWORDS_MISSING_VALUES"));
    }

    Ok(())
}
