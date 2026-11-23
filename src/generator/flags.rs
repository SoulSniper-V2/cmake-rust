//! Linux single-config Ninja flag assembly (cmLocalGenerator,
//! cmCommonTargetGenerator, cmNinjaTargetGenerator).

mod compile;
mod link;
mod rule;
mod standard;

pub use compile::compile_vars_with_source_props;
#[allow(unused_imports, reason = "consumed by the file API codemodel")]
pub(crate) use compile::{BtFrag, define_items, flag_fragments, include_items, pch_items};
pub(super) use compile::{fortran_module_dir, fortran_preprocess, fortran_source_include};
pub use link::{link_dependency_file, link_flag_vars};
pub use rule::{compiler_launcher, compiler_launcher_list, expand_rule};

// API pending the source-property and linked-target integrations below;
// kept visible to the oracle replay tests.
#[cfg(test)]
pub use compile::{compile_vars, object_dependencies};
#[cfg(test)]
pub use rule::{RuleStep, expand_rule_for_step};

use super::ctx::GenCtx;
use crate::messages::MessageType;
use crate::outconv::{FLAG_ALLOW_MAKE_VARIABLES, NINJA_UNIX, escape_for_shell};
use crate::policy::PolicyStatus;
use crate::state::Target;
use crate::util::expand_list;

/// `cmGeneratorTarget::ComputeCompileFeatures`: resolve the entries before
/// emitting build rules, including targets that have no object rules.
pub(super) fn validate_compile_features(ctx: &GenCtx) -> bool {
    for key in ctx.interp.makefiles.iter().flat_map(|mf| &mf.targets) {
        let target = &ctx.interp.ext.targets[key];
        let items = usage_item_fragments(ctx, target, &ctx.config, "", "COMPILE_FEATURES");
        let debug = expand_list(
            &ctx.var_safe(target.makefile, "CMAKE_DEBUG_TARGET_PROPERTIES"),
            false,
        )
        .iter()
        .any(|prop| prop == "COMPILE_FEATURES");
        if debug {
            for group in items.chunk_by(|a, b| a.1 == b.1) {
                let mut text = format!("Used compile features for target {}:\n", target.name);
                for (feature, _) in group {
                    text.push_str(&format!(" * {feature}\n"));
                }
                ctx.message(MessageType::Log, group[0].1.clone(), text);
            }
        }
        for (feature, _) in items {
            if crate::commands::targets::compile_feature_language(&feature).is_none() {
                ctx.error(
                    super::target::directory_backtrace(ctx, target.makefile),
                    format!(
                        "Specified unknown feature \"{feature}\" for target \"{}\".",
                        target.name
                    ),
                );
                return false;
            }
        }
    }
    !ctx.has_error()
}

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
            "INCLUDE_DIRECTORIES"
            | "COMPILE_DEFINITIONS"
            | "COMPILE_OPTIONS"
            | "COMPILE_FEATURES"
            | "LINK_OPTIONS"
            | "LINK_DIRECTORIES" => self
                .usage_items(property)
                .into_iter()
                .map(|item| item.value)
                .collect(),
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
        let memo = format!("{key}\u{0}{property}\u{0}{}\u{0}{}", self.config, self.lang);
        if let Some(items) = self.ctx.usage.borrow().get(&memo) {
            return items.as_ref().clone();
        }
        let mut host = super::genex_host::CtxHost::new(self.ctx, self.target.makefile);
        let items =
            match crate::genex::target_usage(&mut host, &key, property, self.config, self.lang) {
                Ok(items) => items,
                Err(error) => {
                    if !error.quiet {
                        let mut backtrace = self.ctx.backtrace(error.backtrace);
                        if backtrace.is_empty() && error.expression.is_empty() {
                            // Usage-requirement diagnostics (e.g. relative
                            // INTERFACE_INCLUDE_DIRECTORIES) are issued by the
                            // head target's cmLocalGenerator::IssueMessage.
                            backtrace =
                                super::target::directory_backtrace(self.ctx, self.target.makefile);
                        }
                        self.ctx.genex_error(backtrace, &error);
                    }
                    Vec::new()
                }
            };
        if property == "INCLUDE_DIRECTORIES"
            && expand_list(
                &self
                    .ctx
                    .var_safe(self.target.makefile, "CMAKE_DEBUG_TARGET_PROPERTIES"),
                false,
            )
            .iter()
            .any(|item| item == "INCLUDE_DIRECTORIES")
        {
            // `processIncludeDirectories`: the LOG message groups each
            // property entry's newly used includes at the entry's backtrace.
            for group in items.chunk_by(|a, b| a.backtrace == b.backtrace) {
                let mut text = format!("Used includes for target {}:\n", self.target.name);
                for item in group {
                    text.push_str(&format!(" * {}\n", item.value));
                }
                self.ctx.message(
                    MessageType::Log,
                    self.ctx.backtrace(group[0].backtrace),
                    text,
                );
            }
        }
        self.ctx
            .usage
            .borrow_mut()
            .insert(memo, std::rc::Rc::new(items.clone()));
        items
    }

    fn policy(&self, index: usize) -> PolicyStatus {
        super::target::target_policy(self.ctx, self.target, index)
    }

    /// `cmake::IssueMessage(FATAL_ERROR, text)` without a backtrace.
    fn error(&self, text: impl Into<String>) {
        self.ctx.error(Vec::new(), text);
    }

    /// `cmLocalGenerator::IssueMessage`: at the target directory's backtrace.
    fn local_error(&self, text: impl Into<String>) {
        self.ctx.error(
            super::target::directory_backtrace(self.ctx, self.target.makefile),
            text,
        );
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

    /// `cmGeneratorTarget::IsIPOOn`: CMP0069 gating with the same
    /// warn-once and error diagnostics as upstream.
    fn ipo_enabled(&self) -> bool {
        let configured = format!(
            "INTERPROCEDURAL_OPTIMIZATION_{}",
            self.config.to_ascii_uppercase()
        );
        let enabled = self
            .prop(&configured)
            .filter(|_| !self.config.is_empty())
            .or_else(|| self.prop("INTERPROCEDURAL_OPTIMIZATION"));
        if !crate::util::is_on_opt(enabled)
            || !matches!(self.lang, "C" | "CXX" | "CUDA" | "Fortran")
        {
            return false;
        }
        let memo_key = format!("cmp0069:{}:{}", self.target.makefile, self.target.name);
        match self.policy(69) {
            PolicyStatus::Old | PolicyStatus::Warn => {
                if self
                    .var(&format!("_CMAKE_{}_IPO_LEGACY_BEHAVIOR", self.lang))
                    .is_some_and(|value| crate::util::is_on(&value))
                {
                    return true;
                }
                if self.ctx.memo.borrow().contains_key(&memo_key) {
                    return false;
                }
                if self.policy(69) == PolicyStatus::Warn && !self.ctx.interp.in_try_compile {
                    self.ctx.memo.borrow_mut().insert(memo_key, String::new());
                    self.ctx.warning(
                        self.target.backtrace.clone(),
                        format!(
                            "{}\nINTERPROCEDURAL_OPTIMIZATION property will be ignored for target '{}'.",
                            crate::commands::target::policy_warning("CMP0069"),
                            self.target.name,
                        ),
                    );
                }
                false
            }
            PolicyStatus::New => {
                // The Ninja generator is always IPO-capable
                // (`cmGlobalNinjaGenerator::IsIPOSupported`).
                let message = if !self
                    .var(&format!("_CMAKE_{}_IPO_SUPPORTED_BY_CMAKE", self.lang))
                    .is_some_and(|value| crate::util::is_on(&value))
                {
                    Some("CMake doesn't support IPO for current compiler")
                } else if !self
                    .var(&format!(
                        "_CMAKE_{}_IPO_MAY_BE_SUPPORTED_BY_COMPILER",
                        self.lang
                    ))
                    .is_some_and(|value| crate::util::is_on(&value))
                {
                    Some("Compiler doesn't support IPO")
                } else {
                    None
                };
                let Some(message) = message else {
                    return true;
                };
                if self.ctx.memo.borrow().contains_key(&memo_key) {
                    return false;
                }
                self.ctx.memo.borrow_mut().insert(memo_key, String::new());
                self.ctx
                    .error(self.target.backtrace.clone(), message.to_owned());
                false
            }
        }
    }

    fn pic(&self) -> Option<bool> {
        super::compatible::pic(self.ctx, self.target, self.config).or_else(|| {
            matches!(
                self.target.kind,
                crate::state::TargetKind::SharedLibrary | crate::state::TargetKind::ModuleLibrary
            )
            .then_some(true)
        })
    }
}

fn cmake_space(ch: char) -> bool {
    ch.is_ascii_whitespace() || ch == '\u{b}'
}

pub(crate) fn append(flags: &mut String, text: &str) {
    if !text.trim_matches(cmake_space).is_empty() {
        if !flags.is_empty() {
            flags.push(' ');
        }
        flags.push_str(text);
    }
}

pub(crate) fn shell(text: &str) -> String {
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
pub(crate) fn unix_command_line(text: &str) -> Vec<String> {
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

/// `cmGeneratorTarget::IsIPOEnabled` for the file API codemodel.
pub(crate) fn ipo_enabled(ctx: &GenCtx, target: &Target, config: &str, lang: &str) -> bool {
    Flags {
        ctx,
        target,
        config,
        lang,
    }
    .ipo_enabled()
}

/// `target->GetLinkDirectories` usage entries for the codemodel
/// `libraryPath` backtraces: (dir, backtrace).
pub(crate) fn link_dir_items(
    ctx: &GenCtx,
    target: &Target,
    config: &str,
    lang: &str,
) -> Vec<(String, Vec<crate::messages::Frame>)> {
    Flags {
        ctx,
        target,
        config,
        lang,
    }
    .usage_items("LINK_DIRECTORIES")
    .into_iter()
    .map(|item| (item.value, ctx.backtrace(item.backtrace)))
    .collect()
}

pub(crate) use link::{codemodel_link_flags, resolve_linker_wrapper_joined};
pub(crate) use standard::{level, levels};

/// `usage_items` values + backtraces for File API consumers (e.g. the
/// `COMPILE_FEATURES` entries behind `languageStandard.backtraces`).
pub(crate) fn usage_item_fragments(
    ctx: &GenCtx,
    target: &Target,
    config: &str,
    lang: &str,
    property: &str,
) -> Vec<(String, Vec<crate::messages::Frame>)> {
    Flags {
        ctx,
        target,
        config,
        lang,
    }
    .usage_items(property)
    .into_iter()
    .map(|item| (item.value, ctx.backtrace(item.backtrace)))
    .collect()
}
