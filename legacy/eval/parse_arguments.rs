use std::collections::{BTreeMap, BTreeSet};

use super::{Interpreter, need};
use crate::syntax::split_list;
use crate::{Result, fail};

impl Interpreter {
    pub(super) fn parse_arguments(&mut self, args: &[String]) -> Result<()> {
        let (names, values, preserve_semicolons) =
            if args.first().is_some_and(|arg| arg == "PARSE_ARGV") {
                if args.len() != 6 {
                    return fail("cmake_parse_arguments(PARSE_ARGV) requires six arguments");
                }
                let start = args[1].parse::<usize>()?;
                let current = self
                    .function_args
                    .last()
                    .ok_or("PARSE_ARGV requires a function invocation")?;
                let values = current.get(start..).unwrap_or_default().to_vec();
                (&args[2..], values, true)
            } else {
                need(args, 4)?;
                (&args[..4], args[4..].to_vec(), false)
            };
        let prefix = &names[0];
        let mut keys = BTreeMap::new();
        for (index, group) in names[1..].iter().enumerate() {
            for key in split_list(group).into_iter().filter(|key| !key.is_empty()) {
                if keys.insert(key.clone(), index).is_some() {
                    eprintln!(
                        "Warning: cmake_parse_arguments: keyword {key} is defined more than once"
                    );
                }
            }
        }
        let mut fields: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
        let mut missing = BTreeSet::new();
        let mut unparsed = Vec::new();
        let mut values = values.iter().peekable();
        while let Some(value) = values.next() {
            match keys.get(value) {
                Some(0) => {
                    fields.insert(value, vec!["TRUE"]);
                }
                Some(kind) => {
                    let mut field = Vec::new();
                    while let Some(next) = values.next_if(|next| !keys.contains_key(*next)) {
                        field.push(next.as_str());
                        if *kind == 1 {
                            break;
                        }
                    }
                    if field.is_empty() {
                        missing.insert(value.as_str());
                    } else {
                        missing.remove(value.as_str());
                    }
                    if *kind == 1 {
                        fields.insert(value, field);
                    } else {
                        fields.entry(value).or_default().extend(field);
                    }
                }
                None => unparsed.push(value.as_str()),
            }
        }
        let list = |items: &[&str]| -> String {
            if preserve_semicolons {
                items
                    .iter()
                    .map(|value| value.replace(';', "\\;"))
                    .collect::<Vec<_>>()
                    .join(";")
            } else {
                items.join(";")
            }
        };
        for (key, kind) in keys {
            let variable = format!("{prefix}_{key}");
            match fields.get(key.as_str()).map(Vec::as_slice) {
                None if kind == 0 => self.set(variable, "FALSE"),
                Some([value]) if kind == 1 && !value.is_empty() => self.set(variable, *value),
                Some(values) if kind != 1 && !values.is_empty() => self.set(variable, list(values)),
                _ => {
                    self.variables_mut().remove(&variable);
                }
            }
        }
        for (suffix, values) in [
            ("UNPARSED_ARGUMENTS", unparsed),
            ("KEYWORDS_MISSING_VALUES", missing.into_iter().collect()),
        ] {
            let variable = format!("{prefix}_{suffix}");
            if values.is_empty() {
                self.variables_mut().remove(&variable);
            } else {
                self.set(variable, list(&values));
            }
        }
        Ok(())
    }
}
