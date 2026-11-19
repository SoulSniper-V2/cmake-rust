use std::cmp::Ordering;
use std::path::Path;

use regex::{Regex, RegexBuilder};

use super::{Interpreter, is_false, is_true_literal};
use crate::syntax::{Argument, Form, split_list};
use crate::{Result, fail};

impl Interpreter {
    pub(super) fn condition(&mut self, args: &[Argument]) -> Result<bool> {
        if args.is_empty() {
            return Ok(false);
        }
        let mut parser = Condition {
            args,
            index: 0,
            state: self,
        };
        let result = parser.expression()?;
        if let Some(argument) = parser.peek() {
            return fail(format!("unexpected condition argument {:?}", argument.text));
        }
        Ok(result)
    }

    pub(super) fn resolve_operand(&self, value: &Argument) -> String {
        if value.form == Form::Bare {
            self.get(&value.text).unwrap_or_else(|| value.text.clone())
        } else {
            value.text.clone()
        }
    }
}

struct Condition<'a> {
    args: &'a [Argument],
    index: usize,
    state: &'a mut Interpreter,
}

impl<'a> Condition<'a> {
    fn peek(&self) -> Option<&'a Argument> {
        self.args.get(self.index)
    }

    fn pop(&mut self) -> Result<&'a Argument> {
        let argument = self.peek().ok_or("missing condition operand")?;
        self.index += 1;
        Ok(argument)
    }

    fn keyword(&mut self, keyword: &str) -> bool {
        if self
            .peek()
            .is_some_and(|arg| arg.form == Form::Bare && arg.text == keyword)
        {
            self.index += 1;
            true
        } else {
            false
        }
    }

    fn expression(&mut self) -> Result<bool> {
        let mut result = self.term()?;
        loop {
            if self.keyword("AND") {
                result &= self.term()?;
            } else if self.keyword("OR") {
                result |= self.term()?;
            } else {
                return Ok(result);
            }
        }
    }

    fn term(&mut self) -> Result<bool> {
        if self.keyword("(") {
            let result = self.expression()?;
            if !self.keyword(")") {
                return fail("missing ')' in condition");
            }
            return Ok(result);
        }
        if self.keyword("NOT") {
            return Ok(!self.term()?);
        }
        for operator in [
            "DEFINED",
            "EXISTS",
            "IS_DIRECTORY",
            "IS_SYMLINK",
            "IS_ABSOLUTE",
            "TARGET",
            "TEST",
            "COMMAND",
            "POLICY",
        ] {
            if self.keyword(operator) {
                let argument = &self.pop()?.text;
                return Ok(match operator {
                    "DEFINED" => {
                        if let Some(name) = argument
                            .strip_prefix("ENV{")
                            .and_then(|s| s.strip_suffix('}'))
                        {
                            !name.is_empty() && std::env::var_os(name).is_some()
                        } else if let Some(name) = argument
                            .strip_prefix("CACHE{")
                            .and_then(|s| s.strip_suffix('}'))
                        {
                            self.state.cache.contains_key(name)
                        } else {
                            self.state.get(argument).is_some()
                        }
                    }
                    "EXISTS" => Path::new(argument).exists(),
                    "IS_DIRECTORY" => Path::new(argument).is_dir(),
                    "IS_SYMLINK" => Path::new(argument).is_symlink(),
                    "IS_ABSOLUTE" => Path::new(argument).is_absolute(),
                    "TARGET" => self.state.graph.has_target(argument),
                    "TEST" => self
                        .state
                        .graph
                        .tests
                        .iter()
                        .any(|test| &test.name == argument),
                    "COMMAND" => self.state.knows_command(&argument.to_ascii_lowercase()),
                    "POLICY" => super::commands::knows_policy(argument),
                    _ => unreachable!("listed condition operators are exhaustive"),
                });
            }
        }
        let left_arg = self.pop()?;
        for operator in [
            "STREQUAL",
            "STRLESS",
            "STRGREATER",
            "STRLESS_EQUAL",
            "STRGREATER_EQUAL",
            "EQUAL",
            "LESS",
            "GREATER",
            "LESS_EQUAL",
            "GREATER_EQUAL",
            "VERSION_EQUAL",
            "VERSION_LESS",
            "VERSION_GREATER",
            "VERSION_LESS_EQUAL",
            "VERSION_GREATER_EQUAL",
            "MATCHES",
            "IN_LIST",
        ] {
            if self.keyword(operator) {
                let right_arg = self.pop()?;
                let left = self.state.resolve_operand(left_arg);
                let right = self.state.resolve_operand(right_arg);
                if operator == "MATCHES" {
                    let regex = cmake_regex(&right)?;
                    let captures = regex.captures(&left);
                    self.state.save_matches(captures.as_ref());
                    return Ok(captures.is_some());
                }
                if operator == "IN_LIST" {
                    return Ok(self
                        .state
                        .get(&right_arg.text)
                        .is_some_and(|value| split_list(&value).contains(&left)));
                }
                let order = if operator.starts_with("STR") {
                    Some(left.cmp(&right))
                } else if operator.starts_with("VERSION_") {
                    Some(version_order(&left, &right))
                } else {
                    left.parse::<f64>()
                        .ok()
                        .zip(right.parse::<f64>().ok())
                        .and_then(|(a, b)| a.partial_cmp(&b))
                };
                return Ok(order.is_some_and(|order| match operator {
                    "STREQUAL" | "EQUAL" | "VERSION_EQUAL" => order == Ordering::Equal,
                    "STRLESS" | "LESS" | "VERSION_LESS" => order == Ordering::Less,
                    "STRGREATER" | "GREATER" | "VERSION_GREATER" => order == Ordering::Greater,
                    "STRLESS_EQUAL" | "LESS_EQUAL" | "VERSION_LESS_EQUAL" => {
                        order != Ordering::Greater
                    }
                    "STRGREATER_EQUAL" | "GREATER_EQUAL" | "VERSION_GREATER_EQUAL" => {
                        order != Ordering::Less
                    }
                    _ => unreachable!("listed comparison operators are exhaustive"),
                }));
            }
        }
        Ok(is_true_literal(&left_arg.text)
            || (left_arg.form == Form::Bare
                && !is_false(&left_arg.text)
                && self
                    .state
                    .get(&left_arg.text)
                    .is_some_and(|value| !is_false(&value))))
    }
}

pub(super) fn cmake_regex(pattern: &str) -> Result<Regex> {
    Ok(RegexBuilder::new(pattern)
        .dot_matches_new_line(true)
        .build()?)
}

pub(super) fn version_order(left: &str, right: &str) -> Ordering {
    fn parts(value: &str) -> [u64; 4] {
        let mut parts = [0; 4];
        for (slot, part) in parts.iter_mut().zip(value.split('.')) {
            let length = part.bytes().take_while(u8::is_ascii_digit).count();
            *slot = part[..length].parse().unwrap_or(0);
            if length != part.len() {
                break;
            }
        }
        parts
    }
    parts(left).cmp(&parts(right))
}
