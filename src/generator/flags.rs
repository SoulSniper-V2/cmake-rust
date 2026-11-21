//! Linux single-config Ninja flag assembly (cmLocalGenerator,
//! cmCommonTargetGenerator, cmNinjaTargetGenerator).

mod compile;
mod link;
mod rule;
mod standard;

pub use compile::compile_vars_with_source_props;
pub use link::{link_dependency_file, link_flag_vars};
pub use rule::{compiler_launcher, expand_rule};

// API pending the source-property and linked-target integrations below;
// kept visible to the oracle replay tests.
#[cfg(test)]
pub use compile::{compile_vars, object_dependencies};
#[cfg(test)]
pub use rule::{RuleStep, expand_rule_for_step};

use super::ctx::GenCtx;
use crate::outconv::{FLAG_ALLOW_MAKE_VARIABLES, NINJA_UNIX, escape_for_shell};
use crate::policy::PolicyStatus;
use crate::state::Target;
use crate::util::expand_list;

struct Flags<'a, 'i> {
    ctx: &'a GenCtx<'i>,
    target: &'a Target,
    config: &'a str,
    lang: &'a str,
}

impl Flags<'_, '_> {
    fn var(&self, name: &str) -> Option<String> {
        self.ctx.var(self.target.makefile, name)
    }

    fn lang_var(&self, suffix: &str) -> Option<String> {
        self.var(&format!("CMAKE_{}_{}", self.lang, suffix))
    }

    fn prop(&self, name: &str) -> Option<&str> {
        self.target.props.get(name)
    }

    fn eval(&self, value: &str) -> String {
        self.ctx.genex(value, Some(self.target), Some(self.lang))
    }

    fn usage(&self, property: &str) -> Vec<String> {
        match property {
            "INCLUDE_DIRECTORIES" | "COMPILE_DEFINITIONS" | "COMPILE_OPTIONS" | "COMPILE_FEATURES"
            | "LINK_OPTIONS" | "LINK_DIRECTORIES" => {
                self.usage_items(property).into_iter().map(|item| item.value).collect()
            }
            _ => self
                .prop(property)
                .map(|value| expand_list(&self.eval(value), false))
                .unwrap_or_default(),
        }
    }

    /// Direct entries plus linked targets' INTERFACE_* entries
    /// (cmGeneratorTarget::Get*Directories/Definitions/Options).
    fn usage_items(&self, property: &str) -> Vec<crate::genex::UsageItem> {
        let key = self.ctx.target_key(self.target);
        let mut host = super::genex_host::CtxHost::new(self.ctx, self.target.makefile);
        match crate::genex::target_usage(&mut host, &key, property, self.config, self.lang) {
            Ok(items) => items,
            Err(error) => {
                if !error.quiet {
                    let backtrace = self.ctx.backtrace(error.backtrace);
                    self.ctx.error(backtrace, error.to_string());
                }
                Vec::new()
            }
        }
    }

    fn policy(&self, index: usize) -> PolicyStatus {
        // INTEGRATION: Use the target's creation-time snapshot for policies
        // queried through cmGeneratorTarget (CMP0069/0083/0119/0181).
        self.ctx.interp.makefiles[self.target.makefile]
            .policies
            .iter()
            .rev()
            .find_map(|frame| frame.map.get(index))
            .unwrap_or(PolicyStatus::Warn)
    }

    fn error(&self, text: impl Into<String>) {
        // INTEGRATION: Supply the target/property backtrace when available.
        self.ctx.error(Vec::new(), text);
    }

    fn config_flags(&self, name: &str) -> String {
        let mut result = self.var(name).unwrap_or_default();
        if result.trim_matches(cmake_space).is_empty() {
            result.clear();
        }
        if !self.config.is_empty()
            && let Some(value) = self.var(&format!("{name}_{}", self.config.to_ascii_uppercase()))
        {
            append(&mut result, &value);
        }
        result
    }

    fn language_flags(&self, linking: bool) -> String {
        let mut flags = self.config_flags(&format!("CMAKE_{}_FLAGS", self.lang));
        if !linking
            || self
                .lang_var("LINK_WITH_STANDARD_COMPILE_OPTION")
                .is_some_and(|value| crate::util::is_on(&value))
        {
            standard::append_standard(self, &mut flags);
        }
        if self.ipo_enabled() {
            self.append_feature(&mut flags, "IPO");
        }
        flags
    }

    fn append_feature(&self, flags: &mut String, feature: &str) {
        if let Some(options) = self.lang_var(&format!("COMPILE_OPTIONS_{feature}")) {
            append_options(flags, expand_list(&options, false));
        }
    }

    fn ipo_enabled(&self) -> bool {
        let configured = format!(
            "INTERPROCEDURAL_OPTIMIZATION_{}",
            self.config.to_ascii_uppercase()
        );
        let enabled = self
            .prop(&configured)
            .filter(|_| !self.config.is_empty())
            .or_else(|| self.prop("INTERPROCEDURAL_OPTIMIZATION"));
        crate::util::is_on_opt(enabled)
            && matches!(self.lang, "C" | "CXX" | "CUDA" | "Fortran")
            && self
                .var(&format!("_CMAKE_{}_IPO_SUPPORTED_BY_CMAKE", self.lang))
                .is_some_and(|value| crate::util::is_on(&value))
            && self
                .var(&format!(
                    "_CMAKE_{}_IPO_MAY_BE_SUPPORTED_BY_COMPILER",
                    self.lang
                ))
                .is_some_and(|value| crate::util::is_on(&value))
            && self.policy(69) == PolicyStatus::New
    }

    fn pic(&self) -> Option<bool> {
        // INTEGRATION: Resolve INTERFACE_POSITION_INDEPENDENT_CODE and
        // diagnose incompatible values using compatible interface properties.
        self.prop("POSITION_INDEPENDENT_CODE")
            .map(crate::util::is_on)
            .or_else(|| {
                matches!(
                    self.target.kind,
                    crate::state::TargetKind::SharedLibrary
                        | crate::state::TargetKind::ModuleLibrary
                )
                .then_some(true)
            })
    }
}

fn cmake_space(ch: char) -> bool {
    ch.is_ascii_whitespace() || ch == '\u{b}'
}

fn append(flags: &mut String, text: &str) {
    if !text.trim_matches(cmake_space).is_empty() {
        if !flags.is_empty() {
            flags.push(' ');
        }
        flags.push_str(text);
    }
}

fn shell(text: &str) -> String {
    escape_for_shell(text, NINJA_UNIX)
}

fn shell_with_make_vars(text: &str) -> String {
    escape_for_shell(text, NINJA_UNIX | FLAG_ALLOW_MAKE_VARIABLES)
}

fn append_options(flags: &mut String, options: impl IntoIterator<Item = String>) {
    for option in options {
        append(flags, &shell(&option));
    }
}

fn target_options(options: Vec<String>) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    options
        .into_iter()
        .filter(|item| seen.insert(item.clone()))
        .flat_map(|item| match item.strip_prefix("SHELL:") {
            Some(shell) => unix_command_line(shell),
            None => vec![item],
        })
        .collect()
}

/// kwsys SystemTools::ParseUnixCommandLine uses the process argument parser.
fn unix_command_line(text: &str) -> Vec<String> {
    // kwsys accepts unfinished quotes and treats backslash as an escape
    // in every quote context.
    let mut args = Vec::new();
    let mut word = String::new();
    let mut quote = None;
    let mut started = false;
    let mut chars = text.chars();
    while let Some(ch) = chars.next() {
        match (quote, ch) {
            (None, ch) if cmake_space(ch) => {
                if started {
                    args.push(std::mem::take(&mut word));
                    started = false;
                }
                continue;
            }
            (None, '\'' | '"') => quote = Some(ch),
            (Some(active), ch) if active == ch => quote = None,
            (_, '\\') => {
                let Some(next) = chars.next() else {
                    break;
                };
                word.push(next);
            }
            _ => word.push(ch),
        }
        started = true;
    }
    if started {
        args.push(word);
    }
    args
}

#[cfg(test)]
mod oracle_tests;
