use std::path::Path;

use super::rule::{RuleStep, expand_rule_for_step};
use super::{
    BtFrag, Flags, append, append_options, cmake_space, shell, shell_flag, shell_with_make_vars,
    target_options, unix_command_line,
};
use crate::generator::ctx::GenCtx;
use crate::generator::ninja::encode_literal;
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
        append_options_gen(
            &inputs,
            &mut flags,
            resolve_wrapper(&inputs, options, "ARCHIVER"),
        );
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
            let export = exe_export_flags(&inputs, language);
            if !export.is_empty() {
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
        append(&mut flags, &encode_literal_gen(&inputs, &raw));

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
        append_options_gen(
            &inputs,
            &mut flags,
            resolve_wrapper(&inputs, options, "LINKER"),
        );

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
                append_options_gen(&inputs, &mut extra, expand_list(&options, false));
            }
        }
        warning_as_error(&inputs, &mut extra);
        if inputs.ipo_enabled()
            && let Some(options) = inputs.lang_var("LINK_OPTIONS_IPO")
        {
            append_options_gen(&inputs, &mut extra, expand_list(&options, false));
        }
        append_module_definition_flag(&inputs, &mut extra);
        append(&mut flags, &encode_literal_gen(&inputs, &extra));
        append_depfile_flag(&inputs, &mut flags);
        if is_on_opt(inputs.prop("LINK_WHAT_YOU_USE"))
            && let Some(option) = inputs.lang_var("LINK_WHAT_YOU_USE_FLAG")
        {
            append_options_gen(
                &inputs,
                &mut flags,
                resolve_wrapper(&inputs, vec![option], "LINKER"),
            );
        }
    }

    let language_flags = inputs
        .language_flags(true)
        .trim_matches(cmake_space)
        .to_owned();
    if target.kind == TargetKind::Executable {
        let mut flags = language_flags;
        inputs.add_architecture_flags(&mut flags, "");
        result.flags = flags;
    } else {
        result.language_compile_flags = language_flags;
        inputs.add_architecture_flags(&mut result.arch_flags, "");
    }
    result.link_flags = flags.trim_matches(cmake_space).to_owned();
    result
}

/// Language and architecture flags for a link rule, without evaluating
/// target link options.  Generators whose link-flag ordering differs from
/// Ninja use this together with their own link-flag computation.
pub fn link_language_flag_vars(
    ctx: &GenCtx,
    target: &Target,
    config: &str,
    language: &str,
) -> LinkFlagVars {
    let inputs = Flags {
        ctx,
        target,
        config,
        lang: language,
    };
    let mut result = LinkFlagVars::default();
    let language_flags = inputs.language_flags(true);
    if target.kind == TargetKind::Executable {
        let mut flags = language_flags;
        inputs.add_architecture_flags(&mut flags, "");
        result.flags = flags;
    } else {
        result.language_compile_flags = language_flags;
        inputs.add_architecture_flags(&mut result.arch_flags, "");
    }
    result
}

/// `cmMakefileTargetGenerator::GetTargetLinkFlags` combined with the
/// executable/library-specific flag ordering used by the Unix Makefiles
/// generator.  This differs from Ninja: Makefile link rules put target link
/// flags (including the linker depfile flag) before shared-library creation
/// flags, while executable creation/export flags come first.
pub fn makefile_link_flags(ctx: &GenCtx, target: &Target, config: &str, language: &str) -> String {
    let inputs = Flags {
        ctx,
        target,
        config,
        lang: language,
    };
    let upper_config = config.to_ascii_uppercase();
    if target.kind == TargetKind::StaticLibrary {
        return link_flag_vars(ctx, target, config, language).link_flags;
    }

    // cmMakefileTargetGenerator::GetTargetLinkFlags.
    let mut target_flags = String::new();
    if let Some(value) = inputs.prop("LINK_FLAGS") {
        append(&mut target_flags, &encode_literal_gen(&inputs, value));
    }
    if !config.is_empty()
        && let Some(value) = inputs.prop(&format!("LINK_FLAGS_{upper_config}"))
    {
        append(&mut target_flags, &encode_literal_gen(&inputs, value));
    }
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
    append_options_gen(
        &inputs,
        &mut target_flags,
        resolve_wrapper(&inputs, options, "LINKER"),
    );
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
            append_options_gen(&inputs, &mut extra, expand_list(&options, false));
        }
    }
    warning_as_error(&inputs, &mut extra);
    append(&mut target_flags, &encode_literal_gen(&inputs, &extra));
    append_depfile_flag(&inputs, &mut target_flags);

    let mut flags = String::new();
    if target.kind == TargetKind::Executable {
        append_link_variable(&inputs, &mut flags, &creation_flags(&inputs));
        append_link_variable(
            &inputs,
            &mut flags,
            &inputs.config_flags("CMAKE_EXE_LINKER_FLAGS"),
        );
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
            append(
                &mut flags,
                &inputs
                    .var(&format!("CMAKE_EXE_EXPORTS_{language}_FLAG"))
                    .unwrap_or_default(),
            );
        }
        let export = exe_export_flags(&inputs, language);
        if !export.is_empty() {
            append(&mut flags, &export);
        }
        if is_on_opt(inputs.prop("LINK_WHAT_YOU_USE"))
            && let Some(option) = inputs.lang_var("LINK_WHAT_YOU_USE_FLAG")
        {
            append_options_gen(
                &inputs,
                &mut flags,
                resolve_wrapper(&inputs, vec![option], "LINKER"),
            );
        }
        append(&mut flags, &target_flags);
    } else {
        flags = target_flags;
        append_link_variable(&inputs, &mut flags, &creation_flags(&inputs));
        let config_var = if target.kind == TargetKind::ModuleLibrary {
            "CMAKE_MODULE_LINKER_FLAGS"
        } else {
            "CMAKE_SHARED_LINKER_FLAGS"
        };
        append_link_variable(&inputs, &mut flags, &inputs.config_flags(config_var));
    }
    append_module_definition_flag(&inputs, &mut flags);
    if target.kind != TargetKind::Executable
        && is_on_opt(inputs.prop("LINK_WHAT_YOU_USE"))
        && let Some(option) = inputs.lang_var("LINK_WHAT_YOU_USE_FLAG")
    {
        append_options_gen(
            &inputs,
            &mut flags,
            resolve_wrapper(&inputs, vec![option], "LINKER"),
        );
    }
    if inputs.ipo_enabled()
        && let Some(options) = inputs.lang_var("LINK_OPTIONS_IPO")
    {
        append_options_gen(&inputs, &mut flags, expand_list(&options, false));
    }
    flags.trim_end_matches(cmake_space).to_owned()
}

/// Values for a CUDA device-link build statement.
#[derive(Clone, Debug, Default)]
pub struct DeviceLinkFlagVars {
    pub language_compile_flags: String,
    pub link_flags: String,
}

/// `cmLocalGenerator::GetDeviceLinkFlags` and the CUDA language flags passed
/// to the device-link step (`AddLanguageFlagsForLinking(..., "CUDA", ...)`).
pub fn device_link_flag_vars(
    ctx: &GenCtx,
    target: &Target,
    config: &str,
    dependency_ipo: bool,
) -> DeviceLinkFlagVars {
    let inputs = Flags {
        ctx,
        target,
        config,
        lang: "CUDA",
    };
    let mut result = DeviceLinkFlagVars::default();
    let mut link_flags = String::new();
    let ipo = inputs.ipo_enabled() || dependency_ipo;
    if ipo && let Some(options) = inputs.var("CMAKE_CUDA_DEVICE_LINK_OPTIONS_IPO") {
        link_flags.push_str(&options);
    }
    let visibility = "CUDA_VISIBILITY_PRESET";
    if let (Some(prefix), Some(preset)) = (
        inputs.lang_var("COMPILE_OPTIONS_VISIBILITY"),
        inputs.prop(visibility),
    ) {
        if matches!(preset, "default" | "hidden" | "protected" | "internal") {
            append(&mut link_flags, &format!("{prefix}{preset}"));
        } else {
            inputs.ctx.system_error(format!(
                "Target {} uses unsupported value \"{preset}\" for {visibility}. The supported values are: default, hidden, protected, and internal.",
                inputs.target.name
            ));
        }
    }
    let mut encoded = encode_literal_gen(&inputs, &link_flags);
    for option in device_link_options(&inputs) {
        append(&mut encoded, &option);
    }
    result.link_flags = encoded.trim_matches(cmake_space).to_owned();
    result.language_compile_flags = inputs
        .language_flags(true)
        .trim_matches(cmake_space)
        .to_owned();
    result
}

/// `cmGeneratorTarget::GetLinkOptions` with the generator-expression
/// device-link context enabled.
fn device_link_options(inputs: &Flags<'_, '_>) -> Vec<String> {
    if inputs
        .target
        .policies
        .get(crate::policy::index_of("CMP0105").unwrap())
        != Some(PolicyStatus::New)
    {
        return Vec::new();
    }
    let key = inputs.ctx.target_key(inputs.target);
    let mut host = crate::generator::genex_host::CtxHost::new(inputs.ctx, inputs.target.makefile);
    let mut eval = crate::genex::EvalContext {
        config: inputs.config,
        language: Some("CUDA"),
        device_link: true,
        ..crate::genex::EvalContext::default()
    };
    let items = match crate::genex::usage_requirements(&mut host, &key, "LINK_OPTIONS", &mut eval) {
        Ok(items) => items,
        Err(error) => {
            if !error.quiet {
                let backtrace = inputs.ctx.backtrace(error.backtrace);
                inputs.ctx.genex_error(backtrace, &error);
            }
            Vec::new()
        }
    };
    items.into_iter().map(|item| item.value).collect()
}

/// CUDA language flags in compile context, used by the Clang CUDA
/// registration-stub compile rule (`GetFlags("CUDA", config)`).
pub fn cuda_compile_language_flags(ctx: &GenCtx, target: &Target, config: &str) -> String {
    Flags {
        ctx,
        target,
        config,
        lang: "CUDA",
    }
    .language_flags(false)
    .trim_matches(cmake_space)
    .to_owned()
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

fn unix_makefiles_link_script(ctx: &GenCtx) -> bool {
    ctx.interp.get_def("CMAKE_GENERATOR") == Some("Unix Makefiles")
}

/// `cmGlobalGenerator::GetEncodedLiteral`: Ninja encodes `$` and newlines;
/// Unix Makefiles has no additional build-file encoding inside link.txt.
fn encode_literal_gen(inputs: &Flags<'_, '_>, value: &str) -> String {
    if unix_makefiles_link_script(inputs.ctx) {
        value.to_owned()
    } else {
        encode_literal(value)
    }
}

/// `cmLocalGenerator::AppendFlagEscape` for link options.  Makefile link
/// scripts are shell scripts (`LinkScriptShell`), unlike Makefile rules or
/// Ninja statements, so `$` is shell-escaped but not Makefile-doubled.
fn append_options_gen(
    inputs: &Flags<'_, '_>,
    flags: &mut String,
    options: impl IntoIterator<Item = String>,
) {
    if unix_makefiles_link_script(inputs.ctx) {
        for option in options {
            append(
                flags,
                &crate::outconv::escape_for_shell(&option, crate::outconv::FLAG_IS_UNIX),
            );
        }
    } else {
        append_options(flags, options);
    }
}

fn append_link_variable(inputs: &Flags<'_, '_>, flags: &mut String, value: &str) {
    if inputs.policy(181) == PolicyStatus::New {
        append_options_gen(
            inputs,
            flags,
            resolve_wrapper(inputs, unix_command_line(value), "LINKER"),
        );
    } else {
        append(flags, &encode_literal_gen(inputs, value));
    }
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
                append_options_gen(
                    inputs,
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

/// `cmLocalGenerator::GetExeExportFlags`: flags to export symbols from an
/// executable, only with ENABLE_EXPORTS except on AIX, where the exports
/// are computed.
fn exe_export_flags(inputs: &Flags<'_, '_>, language: &str) -> String {
    let target = inputs.target;
    if target.kind != TargetKind::Executable
        || !is_on_opt(
            inputs
                .ctx
                .interp
                .global_props
                .get("TARGET_SUPPORTS_SHARED_LIBS"),
        )
        || target.platform.aix
        || !is_on_opt(inputs.prop("ENABLE_EXPORTS"))
    {
        return String::new();
    }
    inputs
        .var(&format!("CMAKE_SHARED_LIBRARY_LINK_{language}_FLAGS"))
        .unwrap_or_default()
}

/// `cmLocalGenerator::AppendModuleDefinitionFlag`: the `/DEF:` or bare
/// `.def` path from `cmGeneratorTarget::GetModuleDefinitionInfo`.
fn append_module_definition_flag(inputs: &Flags<'_, '_>, flags: &mut String) {
    let Some(info) =
        crate::generator::target::module_definition_info(inputs.ctx, inputs.target, inputs.config)
    else {
        return;
    };
    if info.def_file.is_empty() {
        return;
    }
    let def_file = info.def_file;
    let flag = inputs
        .lang_var("LINK_DEF_FILE_FLAG")
        .or_else(|| inputs.var("CMAKE_LINK_DEF_FILE_FLAG"));
    let Some(flag) = flag else {
        return;
    };
    // ConvertToOutputFormat(ConvertToLinkReference(defFile), SHELL), with
    // the Ninja link line computer's reference.
    let reference = crate::generator::link::ninja_path(inputs.ctx, &def_file);
    append(flags, &format!("{flag}{}", shell(&reference)));
}

/// `cmGeneratorTarget::ResolveLinkerWrapper(items, language, joinItems =
/// true)` for one item, as cmComputeLinkInformation applies it to `LINKER:`
/// link items and feature fragments.
pub(crate) fn resolve_linker_wrapper_joined(
    ctx: &GenCtx,
    target: &Target,
    lang: &str,
    value: &str,
) -> String {
    let inputs = Flags {
        ctx,
        target,
        config: &ctx.config,
        lang,
    };
    resolve_wrapper(&inputs, vec![value.to_owned()], "LINKER").join(" ")
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

/// The linker DEP_FILE binding, before generator-specific path escaping.
///
/// Ninja wants the path relative to the top-level build directory.  Unix
/// Makefiles executes the link inside the target's directory and wants the
/// path relative to that directory instead.
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
    // cmLocalNinjaGenerator::GetLinkDependencyFile: the support directory
    // plus `ConfigDirectory(config)`.  The Makefile generator's local
    // variant has no per-config component.
    let mut path = Path::new(directory)
        .join("CMakeFiles")
        .join(format!("{}.dir", target.name));
    if ctx.multi_config && !ctx.config.is_empty() {
        path = path.join(&ctx.config);
    }
    let path = path.join("link.d");
    if ctx.interp.get_def("CMAKE_GENERATOR") == Some("Unix Makefiles") {
        return Some(
            path.strip_prefix(directory)
                .unwrap_or(&path)
                .to_string_lossy()
                .into_owned(),
        );
    }
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
                    out.push((shell_flag(&option), ctx.backtrace(item.backtrace)));
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
                out.push((encode_literal_gen(&inputs, &shared), Vec::new()));
            }
            for item in inputs.usage_items("LINK_OPTIONS") {
                for option in resolve_wrapper(&inputs, vec![item.value.clone()], "LINKER") {
                    out.push((shell_flag(&option), ctx.backtrace(item.backtrace)));
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
            let export = exe_export_flags(&inputs, language);
            if !export.is_empty() {
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
                out.push((encode_literal_gen(&inputs, &raw), Vec::new()));
            }
            for item in inputs.usage_items("LINK_OPTIONS") {
                for option in resolve_wrapper(&inputs, vec![item.value.clone()], "LINKER") {
                    out.push((shell_flag(&option), ctx.backtrace(item.backtrace)));
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
                append_options_gen(&inputs, &mut extra, expand_list(&options, false));
            }
        }
        warning_as_error(&inputs, &mut extra);
    }
    if inputs.ipo_enabled()
        && let Some(options) = inputs.lang_var("LINK_OPTIONS_IPO")
    {
        append_options_gen(&inputs, &mut extra, expand_list(&options, false));
    }
    append_module_definition_flag(&inputs, &mut extra);
    if !extra.trim_matches(cmake_space).is_empty() {
        out.push((encode_literal_gen(&inputs, &extra), Vec::new()));
    }
    (language_flags, out)
}
