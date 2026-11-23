use std::path::Path;

use super::rule::{RuleStep, expand_rule_for_step};
use super::{
    BtFrag, Flags, append, append_options, cmake_space, shell, shell_with_make_vars,
    target_options, unix_command_line,
};
use crate::generator::ctx::GenCtx;
use crate::policy::PolicyStatus;
use crate::state::{Target, TargetKind};
use crate::util::{expand_list, is_off_opt, is_on, is_on_opt};

/// Per-link bindings for the Ninja build statement.
#[derive(Clone, Debug, Default)]
pub struct LinkFlagVars {
    pub flags: String,
    pub language_compile_flags: String,
    pub link_flags: String,
    pub arch_flags: String,
}

pub fn link_flag_vars(ctx: &GenCtx, target: &Target, config: &str, language: &str) -> LinkFlagVars {
    let inputs = Flags {
        ctx,
        target,
        config,
        lang: language,
    };
    let mut result = LinkFlagVars::default();
    let mut flags = String::new();
    let upper_config = config.to_ascii_uppercase();
    let kind = match target.kind {
        TargetKind::Executable => "EXE",
        TargetKind::SharedLibrary => "SHARED",
        TargetKind::ModuleLibrary => "MODULE",
        TargetKind::StaticLibrary => "STATIC",
        _ => return result,
    };
    let config_var = format!("CMAKE_{kind}_LINKER_FLAGS");
    if target.kind == TargetKind::StaticLibrary {
        append(&mut flags, &inputs.config_flags(&config_var));
        for name in [
            "STATIC_LIBRARY_FLAGS".to_owned(),
            format!("STATIC_LIBRARY_FLAGS_{upper_config}"),
        ] {
            if (name == "STATIC_LIBRARY_FLAGS" || !config.is_empty())
                && let Some(raw) = inputs.prop(&name)
            {
                append(&mut flags, raw);
            }
        }
        let options = target_options(inputs.usage("STATIC_LIBRARY_OPTIONS"));
        append_options(&mut flags, resolve_wrapper(&inputs, options, "ARCHIVER"));
    } else {
        append_link_variable(&inputs, &mut flags, &creation_flags(&inputs));
        append_link_variable(&inputs, &mut flags, &inputs.config_flags(&config_var));
        let mut raw = String::new();
        if target.kind == TargetKind::Executable {
            let exe = if is_on_opt(inputs.prop("WIN32_EXECUTABLE")) {
                "WIN32"
            } else {
                "CONSOLE"
            };
            append_link_variable(
                &inputs,
                &mut flags,
                &inputs
                    .lang_var(&format!("CREATE_{exe}_EXE"))
                    .unwrap_or_default(),
            );
            if is_on_opt(inputs.prop("ENABLE_EXPORTS")) {
                raw.push_str(
                    &inputs
                        .var(&format!("CMAKE_EXE_EXPORTS_{language}_FLAG"))
                        .unwrap_or_default(),
                );
                raw.push(' ');
            }
            if is_on_opt(inputs.var("BUILD_SHARED_LIBS").as_deref()) {
                raw.push_str(
                    &inputs
                        .var(&format!("CMAKE_SHARED_BUILD_{language}_FLAGS"))
                        .unwrap_or_default(),
                );
                raw.push(' ');
            }
            if is_on_opt(inputs.prop("ENABLE_EXPORTS"))
                && let Some(export) =
                    inputs.var(&format!("CMAKE_SHARED_LIBRARY_LINK_{language}_FLAGS"))
                && !export.is_empty()
            {
                raw.push_str(&export);
                raw.push(' ');
            }
        }
        if let Some(value) = inputs.prop("LINK_FLAGS") {
            raw.push_str(value);
            raw.push(' ');
        }
        if !config.is_empty()
            && let Some(value) = inputs.prop(&format!("LINK_FLAGS_{upper_config}"))
        {
            raw.push_str(value);
            raw.push(' ');
        }
        append(&mut flags, &encode_literal(&raw));

        let options = inputs
            .usage("LINK_OPTIONS")
            .into_iter()
            .scan(false, |device_only, item| {
                if item == "<DEVICE_LINK>" || item == "</DEVICE_LINK>" {
                    *device_only = item == "<DEVICE_LINK>";
                    Some(None)
                } else {
                    Some((!*device_only).then_some(item))
                }
            })
            .flatten()
            .collect();
        append_options(&mut flags, resolve_wrapper(&inputs, options, "LINKER"));

        let mut extra = String::new();
        if inputs
            .lang_var("LINK_MODE")
            .as_deref()
            .is_none_or(|mode| mode == "DRIVER")
            && let Some(options) = selected_linker(&inputs)
        {
            for flag in resolve_wrapper(&inputs, expand_list(&options, false), "LINKER") {
                append(&mut extra, &flag);
            }
        }
        if target.kind == TargetKind::Executable
            && inputs.policy(83) == PolicyStatus::New
            && let Some(pic) = inputs.pic()
        {
            let mode = if pic { "PIE" } else { "NO_PIE" };
            if !is_off_opt(
                inputs
                    .lang_var(&format!("LINK_{mode}_SUPPORTED"))
                    .as_deref(),
            ) && let Some(options) = inputs.lang_var(&format!("LINK_OPTIONS_{mode}"))
            {
                append_options(&mut extra, expand_list(&options, false));
            }
        }
        warning_as_error(&inputs, &mut extra);
        if inputs.ipo_enabled()
            && let Some(options) = inputs.lang_var("LINK_OPTIONS_IPO")
        {
            append_options(&mut extra, expand_list(&options, false));
        }
        append(&mut flags, &encode_literal(&extra));
        append_depfile_flag(&inputs, &mut flags);
        if is_on_opt(inputs.prop("LINK_WHAT_YOU_USE"))
            && let Some(option) = inputs.lang_var("LINK_WHAT_YOU_USE_FLAG")
        {
            append_options(&mut flags, resolve_wrapper(&inputs, vec![option], "LINKER"));
        }
    }

    let language_flags = inputs
        .language_flags(true)
        .trim_matches(cmake_space)
        .to_owned();
    if target.kind == TargetKind::Executable {
        result.flags = language_flags;
    } else {
        result.language_compile_flags = language_flags;
    }
    result.link_flags = flags.trim_matches(cmake_space).to_owned();
    result
}

fn creation_flags(inputs: &Flags<'_, '_>) -> String {
    let lang = inputs.lang;
    match inputs.target.kind {
        TargetKind::Executable => inputs.lang_var("LINK_FLAGS"),
        TargetKind::ModuleLibrary => inputs
            .var(&format!("CMAKE_SHARED_MODULE_CREATE_{lang}_FLAGS"))
            .or_else(|| inputs.var(&format!("CMAKE_SHARED_LIBRARY_CREATE_{lang}_FLAGS"))),
        TargetKind::SharedLibrary => {
            inputs.var(&format!("CMAKE_SHARED_LIBRARY_CREATE_{lang}_FLAGS"))
        }
        _ => None,
    }
    .unwrap_or_default()
}

fn append_link_variable(inputs: &Flags<'_, '_>, flags: &mut String, value: &str) {
    if inputs.policy(181) == PolicyStatus::New {
        append_options(
            flags,
            resolve_wrapper(inputs, unix_command_line(value), "LINKER"),
        );
    } else {
        append(flags, &encode_literal(value));
    }
}

fn encode_literal(value: &str) -> String {
    value.replace('$', "$$").replace('\n', "$\n")
}

pub(super) fn selected_linker(inputs: &Flags<'_, '_>) -> Option<String> {
    let selected = inputs
        .prop("LINKER_TYPE")
        .map(|value| inputs.eval(value))
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "DEFAULT".to_owned());
    let variable = format!("USING_LINKER_{selected}");
    let value = inputs.lang_var(&variable);
    if value.is_none() && selected != "DEFAULT" {
        if selected.bytes().all(|byte| byte.is_ascii_uppercase()) {
            inputs.local_error(format!(
                "LINKER_TYPE '{selected}' is unknown or not supported by this toolchain."
            ));
        } else {
            inputs.local_error(format!(
                "LINKER_TYPE '{selected}' is unknown. Did you forget to define the 'CMAKE_{}_{variable}' variable?",
                inputs.lang
            ));
        }
    }
    value
}

fn warning_as_error(inputs: &Flags<'_, '_>, flags: &mut String) {
    // cmLocalGenerator::AppendWarningAsErrorLinkerFlags.
    if inputs.ctx.interp.ignore_link_warning_as_error {
        return;
    }
    let property = inputs.prop("LINK_WARNING_AS_ERROR");
    if is_off_opt(property) {
        return;
    }
    let mut modes = if is_on_opt(property) {
        vec!["DRIVER".to_owned(), "LINKER".to_owned()]
    } else {
        expand_list(property.unwrap_or_default(), false)
    };
    modes.sort_unstable();
    modes.dedup();
    let mut invalid = String::new();
    for mode in modes {
        match mode.as_str() {
            "DRIVER" if inputs.lang_var("LINK_MODE").as_deref() == Some("DRIVER") => {
                inputs.append_feature(flags, "WARNING_AS_ERROR");
            }
            "DRIVER" => {}
            "LINKER" => {
                let options = inputs
                    .lang_var("LINK_OPTIONS_WARNING_AS_ERROR")
                    .unwrap_or_default();
                append_options(
                    flags,
                    resolve_wrapper(inputs, expand_list(&options, false), "LINKER"),
                );
            }
            _ => {
                invalid.push_str("  ");
                invalid.push_str(&mode);
                invalid.push('\n');
            }
        }
    }
    if !invalid.is_empty() {
        inputs.error(format!(
            "Erroneous value(s) for 'LINK_WARNING_AS_ERROR' property of target '{}':\n{invalid}",
            inputs.target.name
        ));
    }
}

fn resolve_wrapper(inputs: &Flags<'_, '_>, options: Vec<String>, prefix: &str) -> Vec<String> {
    let mut wrapper = expand_list(
        &inputs
            .lang_var(&format!("{prefix}_WRAPPER_FLAG"))
            .unwrap_or_default(),
        false,
    );
    let separated = wrapper.last().is_some_and(|last| last == " ");
    if separated {
        wrapper.pop();
    }
    let sep = inputs
        .lang_var(&format!("{prefix}_WRAPPER_FLAG_SEP"))
        .unwrap_or_default();
    let marker = format!("{prefix}:");
    let mut result = Vec::new();
    for option in options {
        let Some(value) = option.strip_prefix(&marker) else {
            result.push(option);
            continue;
        };
        let mut args: Vec<_> = if let Some(value) = value.strip_prefix("SHELL:") {
            unix_command_line(value)
        } else {
            value
                .split(',')
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
                .collect()
        };
        if args.iter().any(|arg| arg.contains("SHELL:")) {
            inputs.ctx.error(
                inputs.target.backtrace.clone(),
                format!("'SHELL:' prefix is not supported as part of '{prefix}:' arguments."),
            );
            break;
        }
        if wrapper.is_empty() {
            result.extend(args);
        } else if !args.is_empty() {
            if !sep.is_empty() {
                args = vec![args.join(&sep)];
            }
            for mut arg in args {
                let wrapper_len = wrapper.len() - usize::from(!separated);
                result.extend_from_slice(&wrapper[..wrapper_len]);
                if !separated {
                    arg.insert_str(0, &wrapper[wrapper_len]);
                }
                result.push(arg);
            }
        }
    }
    result
}

/// The Ninja DEP_FILE binding, before Ninja path escaping.
pub fn link_dependency_file(ctx: &GenCtx, target: &Target, language: &str) -> Option<String> {
    let inputs = Flags {
        ctx,
        target,
        config: &ctx.config,
        lang: language,
    };
    if !matches!(
        target.kind,
        TargetKind::Executable | TargetKind::SharedLibrary | TargetKind::ModuleLibrary
    ) || is_on_opt(inputs.prop("LINK_DEPENDS_NO_SHARED"))
        || !is_on_opt(inputs.lang_var("LINK_DEPENDS_USE_LINKER").as_deref())
        // cmGeneratorTarget::HasLinkDependencyFile: defined-but-off counts
        // (IsDefinitionSet treats empty and -NOTFOUND values as defined).
        || inputs
            .var("CMAKE_LINK_DEPENDS_USE_LINKER")
            .is_some_and(|value| !is_on(&value))
    {
        return None;
    }
    // INTEGRATION: Use target.rs's support/object directory when available.
    let directory = &ctx.interp.makefiles[target.makefile].binary_dir;
    let path = Path::new(directory)
        .join("CMakeFiles")
        .join(format!("{}.dir", target.name))
        .join("link.d");
    let relative = path
        .strip_prefix(&ctx.interp.home_binary)
        .unwrap_or(&path)
        .to_string_lossy()
        .into_owned();
    // cmGlobalNinjaGenerator::NinjaOutputPath (via ConvertToNinjaPath).
    let mut prefix = ctx
        .var(0, "CMAKE_NINJA_OUTPUT_PATH_PREFIX")
        .unwrap_or_default();
    if !prefix.is_empty() && !prefix.ends_with('/') {
        prefix.push('/');
    }
    if !prefix.is_empty() && !crate::util::is_full_path(&relative) {
        Some(format!("{prefix}{relative}"))
    } else {
        Some(relative)
    }
}

fn append_depfile_flag(inputs: &Flags<'_, '_>, flags: &mut String) {
    if let Some(file) = link_dependency_file(inputs.ctx, inputs.target, inputs.lang)
        && let Some(option) = inputs.lang_var("LINKER_DEPFILE_FLAGS")
        && !option.is_empty()
    {
        let option = expand_rule_for_step(
            inputs.ctx,
            inputs.target,
            inputs.lang,
            RuleStep::Link,
            &option,
            &[("DEP_FILE", shell_with_make_vars(&file))],
        );
        for flag in resolve_wrapper(inputs, expand_list(&option, false), "LINKER") {
            append(flags, &flag);
        }
    }
}

/// `cmLocalGenerator::GetTargetFlags` for the file API codemodel:
/// `(linkLanguageFlags, linkFlags)` — the "flags"-role fragments, where only
/// LINK_OPTIONS/STATIC_LIBRARY_OPTIONS entries carry backtraces.
pub(crate) fn codemodel_link_flags(
    ctx: &GenCtx,
    target: &Target,
    config: &str,
    language: &str,
) -> (String, Vec<(String, Vec<crate::messages::Frame>)>) {
    let inputs = Flags {
        ctx,
        target,
        config,
        lang: language,
    };
    let mut language_flags = String::new();
    let mut out: Vec<BtFrag> = Vec::new();
    let upper = config.to_ascii_uppercase();
    if !matches!(
        target.kind,
        TargetKind::Executable
            | TargetKind::SharedLibrary
            | TargetKind::ModuleLibrary
            | TargetKind::StaticLibrary
    ) {
        return (language_flags, out);
    }
    // AppendTargetCreationLinkFlags.
    let create = creation_flags(&inputs);
    if !create.trim_matches(cmake_space).is_empty() {
        out.push((create.trim_end().to_owned(), Vec::new()));
    }
    match target.kind {
        TargetKind::StaticLibrary => {
            let mut flags = String::new();
            append(
                &mut flags,
                &inputs.config_flags("CMAKE_STATIC_LINKER_FLAGS"),
            );
            if !flags.is_empty() {
                out.push((flags, Vec::new()));
            }
            let mut prop_flags = String::new();
            for name in [
                "STATIC_LIBRARY_FLAGS".to_owned(),
                format!("STATIC_LIBRARY_FLAGS_{upper}"),
            ] {
                if (name == "STATIC_LIBRARY_FLAGS" || !config.is_empty())
                    && let Some(raw) = inputs.prop(&name)
                {
                    append(&mut prop_flags, raw);
                }
            }
            if !prop_flags.is_empty() {
                out.push((prop_flags, Vec::new()));
            }
            for item in inputs.usage_items("STATIC_LIBRARY_OPTIONS") {
                for option in resolve_wrapper(&inputs, vec![item.value.clone()], "ARCHIVER") {
                    out.push((shell(&option), ctx.backtrace(item.backtrace)));
                }
            }
        }
        TargetKind::SharedLibrary | TargetKind::ModuleLibrary => {
            let variable = if target.kind == TargetKind::ModuleLibrary {
                "CMAKE_MODULE_LINKER_FLAGS"
            } else {
                "CMAKE_SHARED_LINKER_FLAGS"
            };
            let mut lib_flags = String::new();
            append_link_variable(&inputs, &mut lib_flags, &inputs.config_flags(variable));
            if !lib_flags.trim_matches(cmake_space).is_empty() {
                out.push((lib_flags.trim_end().to_owned(), Vec::new()));
            }
            let mut shared = String::new();
            if let Some(value) = inputs.prop("LINK_FLAGS") {
                shared.push_str(value);
                shared.push(' ');
            }
            if !config.is_empty()
                && let Some(value) = inputs.prop(&format!("LINK_FLAGS_{upper}"))
            {
                shared.push_str(value);
                shared.push(' ');
            }
            if !shared.is_empty() {
                out.push((encode_literal(&shared), Vec::new()));
            }
            for item in inputs.usage_items("LINK_OPTIONS") {
                for option in resolve_wrapper(&inputs, vec![item.value.clone()], "LINKER") {
                    out.push((shell(&option), ctx.backtrace(item.backtrace)));
                }
            }
        }
        TargetKind::Executable => {
            language_flags = inputs
                .language_flags(true)
                .trim_matches(cmake_space)
                .to_owned();
            let mut exe_flags = String::new();
            append_link_variable(
                &inputs,
                &mut exe_flags,
                &inputs.config_flags("CMAKE_EXE_LINKER_FLAGS"),
            );
            if !exe_flags.trim_matches(cmake_space).is_empty() {
                out.push((exe_flags.trim_end().to_owned(), Vec::new()));
            }
            let mut create_exe = String::new();
            let exe = if is_on_opt(inputs.prop("WIN32_EXECUTABLE")) {
                "WIN32"
            } else {
                "CONSOLE"
            };
            append_link_variable(
                &inputs,
                &mut create_exe,
                &inputs
                    .lang_var(&format!("CREATE_{exe}_EXE"))
                    .unwrap_or_default(),
            );
            if !create_exe.trim_matches(cmake_space).is_empty() {
                out.push((create_exe.trim_end().to_owned(), Vec::new()));
            }
            let mut raw = String::new();
            if is_on_opt(inputs.prop("ENABLE_EXPORTS")) {
                raw.push_str(
                    &inputs
                        .var(&format!("CMAKE_EXE_EXPORTS_{language}_FLAG"))
                        .unwrap_or_default(),
                );
                raw.push(' ');
            }
            if is_on_opt(inputs.var("BUILD_SHARED_LIBS").as_deref()) {
                raw.push_str(
                    &inputs
                        .var(&format!("CMAKE_SHARED_BUILD_{language}_FLAGS"))
                        .unwrap_or_default(),
                );
                raw.push(' ');
            }
            if is_on_opt(inputs.prop("ENABLE_EXPORTS"))
                && let Some(export) =
                    inputs.var(&format!("CMAKE_SHARED_LIBRARY_LINK_{language}_FLAGS"))
                && !export.is_empty()
            {
                raw.push_str(&export);
                raw.push(' ');
            }
            if let Some(value) = inputs.prop("LINK_FLAGS") {
                raw.push_str(value);
                raw.push(' ');
            }
            if !config.is_empty()
                && let Some(value) = inputs.prop(&format!("LINK_FLAGS_{upper}"))
            {
                raw.push_str(value);
                raw.push(' ');
            }
            if !raw.is_empty() {
                out.push((encode_literal(&raw), Vec::new()));
            }
            for item in inputs.usage_items("LINK_OPTIONS") {
                for option in resolve_wrapper(&inputs, vec![item.value.clone()], "LINKER") {
                    out.push((shell(&option), ctx.backtrace(item.backtrace)));
                }
            }
        }
        _ => {}
    }
    // extraLinkFlags (linker type, PIE, WARNING_AS_ERROR, IPO).
    let mut extra = String::new();
    if target.kind != TargetKind::StaticLibrary {
        if inputs
            .lang_var("LINK_MODE")
            .as_deref()
            .is_none_or(|mode| mode == "DRIVER")
            && let Some(options) = selected_linker(&inputs)
        {
            for flag in resolve_wrapper(&inputs, expand_list(&options, false), "LINKER") {
                append(&mut extra, &flag);
            }
        }
        if target.kind == TargetKind::Executable
            && inputs.policy(83) == PolicyStatus::New
            && let Some(pic) = inputs.pic()
        {
            let mode = if pic { "PIE" } else { "NO_PIE" };
            if !is_off_opt(
                inputs
                    .lang_var(&format!("LINK_{mode}_SUPPORTED"))
                    .as_deref(),
            ) && let Some(options) = inputs.lang_var(&format!("LINK_OPTIONS_{mode}"))
            {
                append_options(&mut extra, expand_list(&options, false));
            }
        }
        warning_as_error(&inputs, &mut extra);
    }
    if inputs.ipo_enabled()
        && let Some(options) = inputs.lang_var("LINK_OPTIONS_IPO")
    {
        append_options(&mut extra, expand_list(&options, false));
    }
    if !extra.trim_matches(cmake_space).is_empty() {
        out.push((encode_literal(&extra), Vec::new()));
    }
    (language_flags, out)
}
