//! Single-config Ninja install script generation.
use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write;
use std::path::Path;

use super::{ctx::GenCtx, export, install_name, link, package_info, target};
use crate::commands::install::{legacy_source, unix_slashes};
use crate::install_model::*;
use crate::policy::{PolicyStatus, index_of};
use crate::state::{Target, TargetKind};
use crate::util::{
    CompareOp, escape_for_cmake, expand_list, is_full_path, is_on_opt, version_compare,
};

#[cfg(test)]
pub fn write_install_scripts(ctx: &GenCtx) -> std::io::Result<()> {
    warn_skipped_install_rules(ctx);
    for index in 0..ctx.interp.makefiles.len() {
        write_install_script(ctx, index)?;
    }
    Ok(())
}

/// `cmGlobalGenerator::AddGlobalTarget_Install`'s warning: install rules
/// exist but CMAKE_SKIP_INSTALL_RULES is on.
pub fn warn_skipped_install_rules(ctx: &GenCtx) {
    if ctx.interp.ext.install.enabled
        && is_on_opt(ctx.var(0, "CMAKE_SKIP_INSTALL_RULES").as_deref())
    {
        ctx.message(
            crate::messages::MessageType::Warning,
            target::directory_backtrace(ctx, 0),
            "CMAKE_SKIP_INSTALL_RULES was enabled even though installation rules have been specified",
        );
    }
}

/// `cmLocalGenerator::GenerateInstallRules` for one directory.
pub fn write_install_script(ctx: &GenCtx, index: usize) -> std::io::Result<()> {
    if is_on_opt(ctx.var(index, "CMAKE_SKIP_INSTALL_RULES").as_deref()) {
        return Ok(());
    }
    let file = format!(
        "{}/cmake_install.cmake",
        ctx.interp.makefiles[index].binary_dir
    );
    let script = install_script(ctx, index)?;
    // cmGeneratedFileStream without SetCopyIfDifferent: always rewritten,
    // so the RERUN_CMAKE outputs are newer than CMakeCache.txt.
    crate::fsutil::write_generated(Path::new(&file), &script)
}

/// Writes `CMakeFiles/InstallScripts.json` after a successful generation.
pub fn write_install_index(ctx: &GenCtx) -> std::io::Result<()> {
    let files: Vec<String> = ctx
        .interp
        .makefiles
        .iter()
        .enumerate()
        .filter(|(index, _)| !is_on_opt(ctx.var(*index, "CMAKE_SKIP_INSTALL_RULES").as_deref()))
        .map(|(_, mf)| format!("{}/cmake_install.cmake", mf.binary_dir))
        .collect();
    let parallel = is_on_opt(ctx.interp.global_props.get("INSTALL_PARALLEL"));
    // SupportsDefaultConfigs: Ninja Multi-Config only.
    let configs: Option<Vec<String>> = ctx
        .multi_config
        .then(|| ctx.default_configs.iter().cloned().collect());
    crate::fsutil::write_generated(
        &Path::new(&ctx.interp.home_binary).join("CMakeFiles/InstallScripts.json"),
        &install_index(&files, parallel, configs.as_deref()),
    )
}

/// `cmGlobalGenerator::WriteInstallJson`.
pub fn install_index(files: &[String], parallel: bool, configs: Option<&[String]>) -> String {
    let array = |values: &[String]| {
        let values: Vec<_> = values
            .iter()
            .map(|value| serde_json::to_string(value).unwrap())
            .collect();
        if values.is_empty() {
            "[]".into()
        } else {
            format!("\n\t[\n\t\t{}\n\t]", values.join(",\n\t\t"))
        }
    };
    let configs = configs.map_or_else(String::new, |configs| {
        format!("\t\"Configs\" : {},\n", array(configs))
    });
    format!(
        "{{\n{configs}\t\"InstallScripts\" : {},\n\t\"Parallel\" : {parallel}\n}}\n",
        array(files)
    )
}

pub(super) fn directory_policy(ctx: &GenCtx, index: usize, name: &str) -> PolicyStatus {
    let Some(policy) = index_of(name) else {
        return PolicyStatus::Warn;
    };
    ctx.interp.makefiles[index]
        .policies
        .iter()
        .rev()
        .find_map(|p| p.map.get(policy))
        .unwrap_or(PolicyStatus::Warn)
}

pub(super) fn config_test(variable: &str, configs: &[String]) -> String {
    let mut out = format!("{variable} MATCHES \"^(");
    for (index, config) in configs.iter().enumerate() {
        if index != 0 {
            out.push('|');
        }
        for ch in config.chars() {
            if ch.is_ascii_alphabetic() {
                write!(
                    out,
                    "[{}{}]",
                    ch.to_ascii_uppercase(),
                    ch.to_ascii_lowercase()
                )
                .unwrap();
            } else {
                out.push(ch);
            }
        }
    }
    out.push_str(")$\"");
    out
}

/// `cmLocalGenerator::GenerateInstallRules`: the default install
/// configuration (`CMAKE_BUILD_TYPE` for single-config generators, else
/// the first of Release, MinSizeRel, RelWithDebInfo and Debug among the
/// configuration types, else the first type).
fn default_install_config(ctx: &GenCtx) -> String {
    if !ctx.multi_config {
        return ctx.config.clone();
    }
    for wanted in ["RELEASE", "MINSIZEREL", "RELWITHDEBINFO", "DEBUG"] {
        if let Some(config) = ctx
            .configs
            .iter()
            .find(|config| config.to_ascii_uppercase() == wanted)
        {
            return config.clone();
        }
    }
    ctx.configs.first().cloned().unwrap_or_default()
}

/// `cmScriptGenerator::GeneratesForConfig`.
pub(crate) fn generates_for_config(configurations: &[String], config: &str) -> bool {
    configurations.is_empty()
        || configurations
            .iter()
            .any(|item| item.eq_ignore_ascii_case(config))
}

/// `cmScriptGenerator::ActionsPerConfig` of the install generator a rule
/// creates.
fn actions_per_config(rule: &InstallRule) -> bool {
    let has_genex = |text: &str| {
        text.split_once("$<")
            .is_some_and(|(_, tail)| tail.contains('>'))
    };
    match &rule.action {
        InstallAction::Copy { paths, .. } => {
            has_genex(&rule.options.destination)
                || has_genex(&rule.options.rename)
                || paths.iter().any(|path| has_genex(path))
        }
        InstallAction::Script { text, .. } => has_genex(text),
        InstallAction::Target { .. }
        | InstallAction::LegacyTarget(_)
        | InstallAction::Bmi(_)
        | InstallAction::FileSet { .. }
        | InstallAction::RuntimeDependencies { .. }
        | InstallAction::RuntimeDepSet { .. } => true,
        InstallAction::Export(_)
        | InstallAction::PackageInfo { .. }
        | InstallAction::LegacyFiles { .. } => false,
    }
}

pub fn install_script(ctx: &GenCtx, index: usize) -> std::io::Result<String> {
    let mf = &ctx.interp.makefiles[index];
    let binary = &mf.binary_dir;
    let source = &mf.source_dir;
    let prefix = ctx
        .var(index, "CMAKE_STAGING_PREFIX")
        .or_else(|| ctx.var(index, "CMAKE_INSTALL_PREFIX"))
        .unwrap_or_else(|| "/usr/local".into());
    let mut out = format!("# Install script for directory: {source}\n\n");
    out.push_str(&format!(
        r#"# Set the install prefix
if(NOT DEFINED CMAKE_INSTALL_PREFIX)
  set(CMAKE_INSTALL_PREFIX "{prefix}")
endif()
string(REGEX REPLACE "/$" "" CMAKE_INSTALL_PREFIX "${{CMAKE_INSTALL_PREFIX}}")

# Set the install configuration name.
if(NOT DEFINED CMAKE_INSTALL_CONFIG_NAME)
  if(BUILD_TYPE)
    string(REGEX REPLACE "^[^A-Za-z0-9_]+" ""
           CMAKE_INSTALL_CONFIG_NAME "${{BUILD_TYPE}}")
  else()
    set(CMAKE_INSTALL_CONFIG_NAME "{}")
  endif()
  message(STATUS "Install configuration: \"${{CMAKE_INSTALL_CONFIG_NAME}}\"")
endif()

"#,
        default_install_config(ctx)
    ));
    out.push_str(
        r#"# Set the component getting installed.
if(NOT CMAKE_INSTALL_COMPONENT)
  if(COMPONENT)
    message(STATUS "Install component: \"${COMPONENT}\"")
    set(CMAKE_INSTALL_COMPONENT "${COMPONENT}")
  else()
    set(CMAKE_INSTALL_COMPONENT)
  endif()
endif()

"#,
    );
    for (key, comment) in [
        (
            "CMAKE_INSTALL_SO_NO_EXE",
            "Install shared libraries without execute permission?",
        ),
        (
            "CMAKE_CROSSCOMPILING",
            "Is this installation the result of a crosscompile?",
        ),
        (
            "CMAKE_INSTALL_DEFAULT_DIRECTORY_PERMISSIONS",
            "Set default install directory permissions.",
        ),
        (
            "CMAKE_GET_RUNTIME_DEPENDENCIES_PLATFORM",
            "Set OS and executable format for runtime-dependencies.",
        ),
        (
            "CMAKE_GET_RUNTIME_DEPENDENCIES_TOOL",
            "Set tool for dependency-resolution of runtime-dependencies.",
        ),
        (
            "CMAKE_GET_RUNTIME_DEPENDENCIES_COMMAND",
            "Set path to tool for dependency-resolution of runtime-dependencies.",
        ),
        (
            "CMAKE_OBJDUMP",
            "Set path to fallback-tool for dependency-resolution.",
        ),
    ] {
        if let Some(value) = ctx.var(index, key) {
            write!(
                out,
                "# {comment}\nif(NOT DEFINED {key})\n  set({key} \"{value}\")\nendif()\n\n"
            )
            .unwrap();
        }
    }
    let interleave = directory_policy(ctx, index, "CMP0082") == PolicyStatus::New;
    let mut children = mf.children.iter();
    let mut previous = 0;
    let rules = ctx
        .interp
        .ext
        .install
        .rules
        .get(&index)
        .map_or(&[][..], Vec::as_slice);
    for rule in rules.iter().filter(|rule| {
        !matches!(
            rule.action,
            InstallAction::LegacyTarget(_) | InstallAction::LegacyFiles { .. }
        )
    }) {
        if interleave {
            for &child in children
                .by_ref()
                .take(rule.preceding_children.saturating_sub(previous))
            {
                include_child(ctx, &mut out, child);
            }
            previous = rule.preceding_children;
        }
        render_rule(ctx, &mut out, index, rule)?;
    }
    if interleave {
        for &child in children {
            include_child(ctx, &mut out, child);
        }
    }
    if directory_policy(ctx, index, "CMP0082") == PolicyStatus::Warn {
        cmp0082_warning(ctx, index, rules);
    }
    for rule in rules
        .iter()
        .filter(|r| matches!(r.action, InstallAction::LegacyFiles { .. }))
    {
        render_rule(ctx, &mut out, index, rule)?;
    }
    for tgt in ctx
        .targets_in_order()
        .into_iter()
        .filter(|t| t.makefile == index && !t.imported && t.kind != TargetKind::InterfaceLibrary)
    {
        let legacy_script = |out: &mut String, prop: &str| -> std::io::Result<()> {
            if let Some(text) = tgt.props.get(prop) {
                render_rule(
                    ctx,
                    out,
                    index,
                    &InstallRule {
                        options: InstallOptions::default(),
                        action: InstallAction::Script {
                            text: text.into(),
                            is_code: false,
                            all_components: false,
                        },
                        preceding_children: 0,
                        backtrace: Vec::new(),
                    },
                )?;
            }
            Ok(())
        };
        legacy_script(&mut out, "PRE_INSTALL_SCRIPT")?;
        if let Some(rule) = rules.iter().rev().find(
            |rule| matches!(&rule.action, InstallAction::LegacyTarget(name) if *name == tgt.name),
        ) {
            render_rule(ctx, &mut out, index, rule)?;
        }
        legacy_script(&mut out, "POST_INSTALL_SCRIPT")?;
    }
    if !interleave && !mf.children.is_empty() {
        out.push_str("if(NOT CMAKE_INSTALL_LOCAL_ONLY)\n  # Include the install script for each subdirectory.\n");
        for &c in &mf.children {
            let child = &ctx.interp.makefiles[c];
            if !is_on_opt(child.props.get("EXCLUDE_FROM_ALL")) {
                writeln!(
                    out,
                    "  include(\"{}/cmake_install.cmake\")",
                    unix_slashes(&child.binary_dir)
                )
                .unwrap();
            }
        }
        out.push_str("\nendif()\n\n");
    }
    out.push_str(&format!(
        r#"string(REPLACE ";" "\n" CMAKE_INSTALL_MANIFEST_CONTENT
       "${{CMAKE_INSTALL_MANIFEST_FILES}}")
if(CMAKE_INSTALL_LOCAL_ONLY)
  file(WRITE "{binary}/install_local_manifest.txt"
     "${{CMAKE_INSTALL_MANIFEST_CONTENT}}")
endif()
"#
    ));
    if *binary == ctx.interp.home_binary {
        out.push_str(&format!(
            r#"if(CMAKE_INSTALL_COMPONENT)
  if(CMAKE_INSTALL_COMPONENT MATCHES "^[a-zA-Z0-9_.+-]+$")
    set(CMAKE_INSTALL_MANIFEST "install_manifest_${{CMAKE_INSTALL_COMPONENT}}.txt")
  else()
    string(MD5 CMAKE_INST_COMP_HASH "${{CMAKE_INSTALL_COMPONENT}}")
    set(CMAKE_INSTALL_MANIFEST "install_manifest_${{CMAKE_INST_COMP_HASH}}.txt")
    unset(CMAKE_INST_COMP_HASH)
  endif()
else()
  set(CMAKE_INSTALL_MANIFEST "install_manifest.txt")
endif()

if(NOT CMAKE_INSTALL_LOCAL_ONLY)
  file(WRITE "{binary}/${{CMAKE_INSTALL_MANIFEST}}"
     "${{CMAKE_INSTALL_MANIFEST_CONTENT}}")
endif()
"#
        ));
    }
    Ok(out)
}

/// `cmInstallSubdirectoryGenerator::HaveInstall`: the directory, or one
/// of its subdirectories, has an install generator.
fn have_install(ctx: &GenCtx, dir: usize) -> bool {
    let has_rules = ctx.interp.ext.install.rules.get(&dir).is_some_and(|rules| {
        rules
            .iter()
            .any(|rule| !matches!(rule.action, InstallAction::LegacyTarget(_)))
    });
    has_rules
        || ctx.interp.makefiles[dir]
            .children
            .iter()
            .any(|&child| have_install(ctx, child))
}

/// The CMP0082 WARN check of `cmLocalGenerator::GenerateInstallRules`: an
/// install rule follows a subdirectory (added earlier) that installs
/// something (`CheckCMP0082` over the generators in order).
fn cmp0082_warning(ctx: &GenCtx, index: usize, rules: &[InstallRule]) {
    let children = &ctx.interp.makefiles[index].children;
    let after_subdirectory_install = rules
        .iter()
        .filter(|rule| !matches!(rule.action, InstallAction::LegacyTarget(_)))
        .any(|rule| {
            children
                .iter()
                .take(rule.preceding_children)
                .any(|&child| have_install(ctx, child))
        });
    if after_subdirectory_install
        && ctx
            .interp
            .policy_optional_warning(ctx.var(index, "CMAKE_POLICY_WARNING_CMP0082").as_deref())
        && let Some(policy) = index_of("CMP0082")
    {
        let message = format!("{}\n", crate::policy::policy_warning(policy));
        ctx.warning(target::directory_backtrace(ctx, index), message);
    }
}

fn include_child(ctx: &GenCtx, out: &mut String, child: usize) {
    let child = &ctx.interp.makefiles[child];
    if !is_on_opt(child.props.get("EXCLUDE_FROM_ALL")) {
        write!(out, "if(NOT CMAKE_INSTALL_LOCAL_ONLY)\n  # Include the install script for the subdirectory.\n  include(\"{}/cmake_install.cmake\")\nendif()\n\n", unix_slashes(&child.binary_dir)).unwrap();
    }
}

/// `cmInstallGenerator::CreateComponentTest` (never `AllComponents` here).
fn component_test(component: &str, exclude_from_all: bool) -> String {
    let mut test = format!("CMAKE_INSTALL_COMPONENT STREQUAL \"{component}\"");
    if !exclude_from_all {
        test.push_str(" OR NOT CMAKE_INSTALL_COMPONENT");
    }
    test
}

fn render_rule(
    ctx: &GenCtx,
    out: &mut String,
    index: usize,
    rule: &InstallRule,
) -> std::io::Result<()> {
    let opts = &rule.options;
    if let InstallAction::RuntimeDependencies {
        set,
        filters,
        library_component,
        framework_component,
        ..
    } = &rule.action
    {
        // cmInstallGetRuntimeDependenciesGenerator::GenerateScript wraps
        // unconditionally, testing the library and framework components.
        let test = if framework_component.is_empty() || framework_component == library_component {
            component_test(library_component, opts.exclude)
        } else {
            format!(
                "{} OR {}",
                component_test(library_component, true),
                component_test(framework_component, opts.exclude)
            )
        };
        writeln!(out, "if({test})").unwrap();
        render_configs(ctx, out, rule, "  ", &mut |ctx, out, indent| {
            render_get_runtime_dependencies(ctx, out, index, set, filters, indent);
            Ok(())
        })?;
        out.push_str("endif()\n\n");
        return Ok(());
    }
    let all = matches!(
        rule.action,
        InstallAction::Script {
            all_components: true,
            ..
        }
    );
    let conditional = !all || opts.exclude;
    if all {
        if opts.exclude {
            out.push_str("if(CMAKE_INSTALL_COMPONENT)\n");
        }
    } else {
        let other = if opts.exclude {
            ""
        } else {
            " OR NOT CMAKE_INSTALL_COMPONENT"
        };
        writeln!(
            out,
            "if(CMAKE_INSTALL_COMPONENT STREQUAL \"{}\"{other})",
            opts.component
        )
        .unwrap();
    }
    let base_indent = if all { "" } else { "  " };
    {
        let mut config_rules = String::new();
        render_configs(ctx, out, rule, base_indent, &mut |ctx, out, indent| {
            render_action(
                ctx,
                (out, indent),
                (&mut config_rules, base_indent),
                index,
                rule,
            )
        })?;
        out.push_str(&config_rules);
    }
    if conditional {
        out.push_str("endif()\n\n");
    }
    Ok(())
}

/// `cmScriptGenerator::GenerateScriptConfigs`.
fn render_configs(
    ctx: &GenCtx,
    out: &mut String,
    rule: &InstallRule,
    base_indent: &str,
    render: &mut dyn FnMut(&GenCtx, &mut String, &str) -> std::io::Result<()>,
) -> std::io::Result<()> {
    let configurations = &rule.options.configurations;
    if ctx.multi_config && actions_per_config(rule) {
        // GenerateScriptActionsPerConfig.
        let mut first = true;
        for config in &ctx.configs {
            if !generates_for_config(configurations, config) {
                continue;
            }
            writeln!(
                out,
                "{base_indent}{}({})",
                if first { "if" } else { "elseif" },
                config_test("CMAKE_INSTALL_CONFIG_NAME", std::slice::from_ref(config))
            )
            .unwrap();
            let config_ctx = ctx.for_config(config);
            render(&config_ctx, out, &format!("{base_indent}  "))?;
            first = false;
        }
        if !first {
            writeln!(out, "{base_indent}endif()").unwrap();
        }
    } else if configurations.is_empty() {
        render(ctx, out, base_indent)?;
    } else {
        writeln!(
            out,
            "{base_indent}if({})",
            config_test("CMAKE_INSTALL_CONFIG_NAME", configurations)
        )
        .unwrap();
        render(ctx, out, &format!("{base_indent}  "))?;
        writeln!(out, "{base_indent}endif()").unwrap();
    }
    Ok(())
}

/// `cmGeneratorExpression::Evaluate` in directory `index`.
pub(crate) fn genex_at(ctx: &GenCtx, index: usize, text: &str) -> String {
    ctx.genex_full(text, index, None, None, false, &[])
}

/// `cmInstallCxxModuleBmiGenerator::GetScriptLocation`.
pub(super) fn bmi_script(ctx: &GenCtx, target: &Target) -> String {
    let config = if ctx.config.is_empty() {
        "noconfig"
    } else {
        ctx.config.as_str()
    };
    format!(
        "{}/install-cxx-module-bmi-{config}.cmake",
        target::support_directory(ctx, target)
    )
}

/// The install generator's script actions for one configuration
/// (`GenerateScriptForConfig`, or `GenerateScriptActions`). Export rules also
/// collect the per-configuration rules that follow in `config_rules`.
fn render_action(
    ctx: &GenCtx,
    (out, indent): (&mut String, &str),
    config_rules: (&mut String, &str),
    index: usize,
    rule: &InstallRule,
) -> std::io::Result<()> {
    let opts = &rule.options;
    match &rule.action {
        InstallAction::Copy {
            kind,
            paths,
            dir_permissions,
            arguments,
        } => {
            let dest = genex_at(ctx, index, &opts.destination);
            let rename = genex_at(ctx, index, &opts.rename);
            let mut evaluated = Vec::new();
            let is_directory = matches!(kind, CopyKind::Directory);
            let files = if opts.destination.contains("$<")
                || opts.rename.contains("$<")
                || paths.iter().any(|p| p.contains("$<"))
            {
                for path in paths {
                    evaluated.extend(expand_list(&genex_at(ctx, index, path), false));
                }
                if is_directory {
                    for path in &mut evaluated {
                        if !is_full_path(path) {
                            *path = format!("{}/{path}", ctx.interp.makefiles[index].source_dir);
                        }
                    }
                }
                &evaluated
            } else {
                paths
            };
            write_copy(
                out,
                indent,
                opts,
                CopyRule {
                    dest: &dest,
                    kind: *kind,
                    files,
                    dir_permissions,
                    rename: &rename,
                    arguments,
                    files_var: "",
                },
            );
        }
        InstallAction::Script { text, is_code, .. } => {
            let policy = directory_policy(ctx, index, "CMP0087");
            let has_genex = text
                .split_once("$<")
                .is_some_and(|(_, tail)| tail.contains('>'));
            if has_genex && policy == PolicyStatus::Warn {
                ctx.message(
                    crate::messages::MessageType::AuthorWarning,
                    target::directory_backtrace(ctx, index),
                    crate::policy::policy_warning(index_of("CMP0087").unwrap()),
                );
            }
            let text = if has_genex && policy == PolicyStatus::New {
                genex_at(
                    ctx,
                    index,
                    &text.replace("$<INSTALL_PREFIX>", "${CMAKE_INSTALL_PREFIX}"),
                )
            } else {
                text.clone()
            };
            if *is_code {
                writeln!(out, "{indent}{text}").unwrap();
            } else {
                writeln!(out, "{indent}include(\"{text}\")").unwrap();
            }
        }
        InstallAction::Target {
            name,
            namelink,
            importlink,
            implib,
            imported,
        } => {
            if let Some(tgt) = ctx.target_at(index, name) {
                render_target(
                    ctx,
                    out,
                    rule,
                    tgt,
                    *namelink,
                    *importlink,
                    *implib,
                    *imported,
                    indent,
                );
            }
        }
        InstallAction::Bmi(name) => {
            if let Some(tgt) = ctx.target_at(index, name) {
                let script = escape_for_cmake(&bmi_script(ctx, tgt));
                writeln!(out, "{indent}include({script} OPTIONAL)").unwrap();
            }
        }
        InstallAction::FileSet { name, set } => {
            if let Some(tgt) = ctx.target_at(index, name) {
                render_file_set(ctx, (out, indent), index, opts, tgt, set);
            }
        }
        InstallAction::Export(export) => {
            export::write_install_export(ctx, (out, indent), config_rules, index, rule, export)?
        }
        InstallAction::PackageInfo { set, arguments } => package_info::write_install(
            ctx,
            (out, indent),
            config_rules,
            index,
            rule,
            set,
            arguments,
        )?,
        InstallAction::LegacyFiles { args, programs } => {
            let opts = InstallOptions {
                component: ctx
                    .var(index, "CMAKE_INSTALL_DEFAULT_COMPONENT_NAME")
                    .unwrap_or_else(|| opts.component.clone()),
                ..opts.clone()
            };
            let files = legacy_files(ctx, index, args, *programs);
            let kind = if *programs {
                CopyKind::Program
            } else {
                CopyKind::File
            };
            write_copy(
                out,
                indent,
                &opts,
                CopyRule::new(&opts.destination, kind, &files),
            );
        }
        InstallAction::LegacyTarget(name) => {
            if let Some(tgt) = ctx.target(name).filter(|t| {
                matches!(
                    t.kind,
                    TargetKind::Executable
                        | TargetKind::StaticLibrary
                        | TargetKind::SharedLibrary
                        | TargetKind::ModuleLibrary
                )
            }) {
                render_target(
                    ctx,
                    out,
                    rule,
                    tgt,
                    Namelink::None,
                    Namelink::None,
                    false,
                    false,
                    indent,
                );
            }
        }
        InstallAction::RuntimeDepSet { .. } => {
            render_runtime_dependency_set(ctx, out, index, rule, indent);
        }
        InstallAction::RuntimeDependencies { .. } => unreachable!(),
    }
    Ok(())
}

fn legacy_files(ctx: &GenCtx, index: usize, args: &[String], programs: bool) -> Vec<String> {
    let mf = &ctx.interp.makefiles[index];
    let names = if args[0] == "FILES" || (programs && args.len() > 1) {
        let start = usize::from(args[0] == "FILES");
        args[start..].to_vec()
    } else if args.len() > 1 {
        args[1..]
            .iter()
            .map(|name| {
                let start = name.rfind('/').map_or(0, |i| i + 1);
                let end = name[start..].rfind('.').map_or(name.len(), |i| start + i);
                format!("{}{}", &name[..end], args[0])
            })
            .collect()
    } else {
        let Ok(regex) = crate::cmregex::Regex::new(&args[0]) else {
            return Vec::new();
        };
        let mut names: Vec<_> = std::fs::read_dir(&mf.source_dir)
            .into_iter()
            .flatten()
            .filter_map(Result::ok)
            .filter_map(|entry| entry.file_name().into_string().ok())
            .filter(|name| regex.find(name).is_some())
            .collect();
        names.sort();
        names
    };
    names
        .iter()
        .map(|name| legacy_source(&mf.source_dir, &mf.binary_dir, name))
        .collect()
}

pub(super) fn absolute_dest(dest: &str) -> String {
    if dest.is_empty() || is_full_path(dest) {
        dest.into()
    } else {
        format!("${{CMAKE_INSTALL_PREFIX}}/{dest}")
    }
}

pub(super) struct CopyRule<'a> {
    pub dest: &'a str,
    pub kind: CopyKind,
    pub files: &'a [String],
    pub dir_permissions: &'a [String],
    pub rename: &'a str,
    pub arguments: &'a str,
    /// The script variable holding the list of files.
    pub files_var: &'a str,
}

impl<'a> CopyRule<'a> {
    pub(super) fn new(dest: &'a str, kind: CopyKind, files: &'a [String]) -> Self {
        Self {
            dest,
            kind,
            files,
            dir_permissions: &[],
            rename: "",
            arguments: "",
            files_var: "",
        }
    }
}

pub(super) fn write_copy(
    out: &mut String,
    indent: &str,
    opts: &InstallOptions,
    rule: CopyRule<'_>,
) {
    let CopyRule {
        dest,
        kind,
        files,
        dir_permissions,
        rename,
        arguments,
        files_var,
    } = rule;
    if is_full_path(dest) {
        if !files.is_empty() {
            let absolute: Vec<_> = files
                .iter()
                .map(|file| {
                    let name = if rename.is_empty() {
                        file.rsplit('/').next().unwrap()
                    } else {
                        rename
                    };
                    format!("{dest}/{name}")
                })
                .collect();
            write!(
                out,
                "{indent}list(APPEND CMAKE_ABSOLUTE_DESTINATION_FILES\n{indent} \"{}\")\n",
                absolute.join(";")
            )
            .unwrap();
        }
        if !files_var.is_empty() {
            write!(out, "{indent}foreach(_cmake_abs_file IN LISTS {files_var})\n{indent}  get_filename_component(_cmake_abs_file_name \"${{_cmake_abs_file}}\" NAME)\n{indent}  list(APPEND CMAKE_ABSOLUTE_DESTINATION_FILES \"{dest}/${{_cmake_abs_file_name}}\")\n{indent}endforeach()\n{indent}unset(_cmake_abs_file_name)\n{indent}unset(_cmake_abs_file)\n").unwrap();
        }
        write!(out, "{indent}if(CMAKE_WARN_ON_ABSOLUTE_INSTALL_DESTINATION)\n{indent}  message(WARNING \"ABSOLUTE path INSTALL DESTINATION : ${{CMAKE_ABSOLUTE_DESTINATION_FILES}}\")\n{indent}endif()\n").unwrap();
        write!(out, "{indent}if(CMAKE_ERROR_ON_ABSOLUTE_INSTALL_DESTINATION)\n{indent}  message(FATAL_ERROR \"ABSOLUTE path INSTALL DESTINATION forbidden (by caller): ${{CMAKE_ABSOLUTE_DESTINATION_FILES}}\")\n{indent}endif()\n").unwrap();
    }
    write!(
        out,
        "{indent}file(INSTALL DESTINATION \"{}\" TYPE {}",
        absolute_dest(dest),
        kind.token()
    )
    .unwrap();
    if opts.optional {
        out.push_str(" OPTIONAL");
    }
    if !opts.message.is_empty() {
        write!(out, " {}", opts.message).unwrap();
    }
    if !opts.permissions.is_empty() {
        write!(out, " PERMISSIONS {}", opts.permissions.join(" ")).unwrap();
    }
    if !dir_permissions.is_empty() {
        write!(out, " DIR_PERMISSIONS {}", dir_permissions.join(" ")).unwrap();
    }
    if !rename.is_empty() {
        write!(out, " RENAME \"{rename}\"").unwrap();
    }
    out.push_str(" FILES");
    if let [file] = files {
        write!(out, " \"{file}\"").unwrap();
    } else {
        for file in files {
            write!(out, "\n{indent}  \"{file}\"").unwrap();
        }
        if !files_var.is_empty() {
            write!(out, " ${{{files_var}}}").unwrap();
        }
        write!(out, "\n{indent} ").unwrap();
        if arguments.is_empty() {
            out.push(' ');
        }
    }
    writeln!(out, "{arguments})").unwrap();
}

pub(crate) fn object_subdir(tgt: &Target, config: &str) -> String {
    if config.is_empty() {
        format!("objects/{}", tgt.name)
    } else {
        format!("objects-{config}/{}", tgt.name)
    }
}

/// The object names of `cmGeneratorTarget::GetObjectSources`.
pub(crate) fn object_source_names(ctx: &GenCtx, tgt: &Target) -> Vec<String> {
    target::sources(ctx, tgt, &ctx.config)
        .iter()
        .filter(|source| matches!(source.kind, target::SourceKind::ObjectSource))
        .map(|source| source.object_name.clone())
        .collect()
}

/// `cmGeneratorTarget::GetTargetObjectNames`: the object sources' names
/// followed by the extra ISPC objects.
pub(crate) fn object_names(ctx: &GenCtx, tgt: &Target) -> Vec<String> {
    let mut names = object_source_names(ctx, tgt);
    names.extend(super::ispc::target_object_names(ctx, tgt, &ctx.config));
    names
}

/// `cmInstallTargetGenerator::GetFiles`: the `file(INSTALL)` type, extra
/// literal arguments, the effective `files.NamelinkMode`, and the
/// `(from, to)` pairs for one artifact kind.  `from` paths join the
/// artifact directory; `to` paths are relative to the install destination
/// and receive the tweak rules.
#[allow(clippy::too_many_arguments)]
pub(crate) fn target_files(
    ctx: &GenCtx,
    tgt: &Target,
    names: &target::Names,
    namelink: Namelink,
    importlink: Namelink,
    implib: bool,
    backtrace: &[crate::messages::Frame],
) -> (CopyKind, String, Namelink, Vec<(String, String)>) {
    let mut kind = match tgt.kind {
        TargetKind::Executable => CopyKind::Executable,
        TargetKind::StaticLibrary => CopyKind::Static,
        TargetKind::SharedLibrary => CopyKind::Shared,
        TargetKind::ModuleLibrary => CopyKind::Module,
        TargetKind::ObjectLibrary => CopyKind::File,
        _ => return (CopyKind::File, String::new(), Namelink::None, Vec::new()),
    };
    let mut arguments = String::new();
    let mut mode = Namelink::None;
    let mut from_names: Vec<String> = Vec::new();
    let mut to_names: Vec<String> = Vec::new();
    if implib {
        // An import library looks like a static library.
        kind = CopyKind::Static;
        if tgt.kind == TargetKind::Executable {
            to_names.push(names.import_library.clone());
        } else {
            if compute_install_names(
                &mut to_names,
                importlink,
                &names.import_output,
                &names.import_library,
                &names.import_real,
            ) {
                mode = importlink;
            }
        }
        // `GetImplibGNUtoMS` appends the Microsoft-style names.
        if install_name::has_implib_gnu_to_ms(ctx, tgt, &ctx.config) {
            for name in to_names.clone() {
                if let Some(ms) = install_name::implib_gnu_to_ms(ctx, tgt, &ctx.config, &name, None)
                {
                    to_names.push(ms);
                }
            }
        }
        from_names.clone_from(&to_names);
    } else if tgt.is_app_bundle_on_apple() {
        // Install the whole app bundle directory; tweaks apply to the
        // binary inside.
        kind = CopyKind::Directory;
        arguments.push_str(" USE_SOURCE_PERMISSIONS");
        from_names.push(target::app_bundle_directory(
            ctx,
            tgt,
            &ctx.config,
            target::BundleLevel::BundleDir,
        ));
        to_names.push(format!(
            "{}/{}",
            target::app_bundle_directory(ctx, tgt, &ctx.config, target::BundleLevel::Full),
            names.output
        ));
    } else if tgt.is_framework_on_apple() {
        match namelink {
            Namelink::Only => return (kind, arguments, mode, Vec::new()),
            Namelink::Skip => {
                ctx.message(
                    crate::messages::MessageType::AuthorWarning,
                    backtrace.to_vec(),
                    format!(
                        "Target '{}' was changed to a FRAMEWORK sometime after install().  This may result in the wrong install DESTINATION.  Set the FRAMEWORK property earlier.",
                        tgt.name
                    ),
                );
            }
            Namelink::None => {}
        }
        kind = CopyKind::Directory;
        arguments.push_str(" USE_SOURCE_PERMISSIONS");
        // `names.output` is `<name>.framework/<binary>`; the from is the
        // whole framework directory.
        from_names.push(crate::util::filename_path(&names.output));
        to_names.push(names.real.clone());
    } else if tgt.is_cf_bundle_on_apple() {
        kind = CopyKind::Directory;
        arguments.push_str(" USE_SOURCE_PERMISSIONS");
        from_names.push(
            names
                .output
                .split('/')
                .next()
                .unwrap_or_default()
                .to_owned(),
        );
        to_names.push(names.output.clone());
    } else {
        let archived_aix = tgt.is_archived_aix_shared_library();
        let soname = if tgt.kind == TargetKind::Executable || archived_aix {
            ""
        } else {
            names.shared_object.as_str()
        };
        if compute_install_names(&mut to_names, namelink, &names.output, soname, &names.real)
            && tgt.kind != TargetKind::Executable
        {
            mode = namelink;
        }
        from_names.clone_from(&to_names);
    }
    if to_names.is_empty() {
        return (kind, arguments, mode, Vec::new());
    }
    let from_artifact = if implib {
        target::ArtifactType::Import
    } else {
        target::ArtifactType::Runtime
    };
    let from = target::directory(ctx, tgt, &ctx.config, from_artifact);
    let pairs = from_names
        .iter()
        .zip(to_names.iter())
        .map(|(from_name, to_name)| (format!("{from}/{from_name}"), to_name.clone()))
        .collect();
    (kind, arguments, mode, pairs)
}

#[allow(clippy::too_many_arguments)]
fn render_target(
    ctx: &GenCtx,
    out: &mut String,
    rule: &InstallRule,
    tgt: &Target,
    namelink: Namelink,
    importlink: Namelink,
    implib: bool,
    imported: bool,
    indent: &str,
) {
    let opts = &rule.options;
    let mut kind = match tgt.kind {
        TargetKind::Executable => CopyKind::Executable,
        TargetKind::StaticLibrary => CopyKind::Static,
        TargetKind::SharedLibrary => CopyKind::Shared,
        TargetKind::ModuleLibrary => CopyKind::Module,
        TargetKind::ObjectLibrary => CopyKind::File,
        _ => return,
    };
    let dest = genex_at(ctx, tgt.makefile, &opts.destination);
    if tgt.kind == TargetKind::ObjectLibrary {
        let files = object_names(ctx, tgt);
        if !files.is_empty() {
            let base = target::object_directory(ctx, tgt, &ctx.config);
            let dest = format!("{dest}/{}", object_subdir(tgt, &ctx.config));
            let arguments = format!(" FILES_FROM_DIR \"{base}\"");
            write_copy(
                out,
                indent,
                opts,
                CopyRule {
                    arguments: &arguments,
                    ..CopyRule::new(&dest, kind, &files)
                },
            );
        }
        return;
    }
    if imported {
        // `cmInstallImportedRuntimeArtifactsGenerator::GenerateScriptForConfig`.
        let location =
            target::full_path(ctx, tgt, &ctx.config, target::ArtifactType::Runtime, false);
        let dir_install = |ext: &str| {
            // `(dir)(name).ext` out of the location path.
            let re = crate::cmregex::Regex::new(&format!("^(.*/)?([^/]*)\\.{ext}/(.*)$")).unwrap();
            let found = re.find(&location)?;
            let group = |index: usize| {
                found.groups[index]
                    .map(|(start, end)| location[start..end].to_owned())
                    .unwrap_or_default()
            };
            Some(format!(
                "{}{}.{}",
                group(1),
                group(2),
                ext.trim_start_matches('.')
            ))
        };
        let mut bundle_install = |kind: CopyKind, ext: &str| {
            if let Some(bundle) = dir_install(ext) {
                let dir_permissions: Vec<String> = opts.permissions.clone();
                let copy_opts = InstallOptions {
                    permissions: Vec::new(),
                    ..opts.clone()
                };
                write_copy(
                    out,
                    indent,
                    &copy_opts,
                    CopyRule {
                        dest: &dest,
                        kind,
                        files: std::slice::from_ref(&bundle),
                        dir_permissions: &dir_permissions,
                        rename: "",
                        arguments: " USE_SOURCE_PERMISSIONS",
                        files_var: "",
                    },
                );
                return true;
            }
            false
        };
        match tgt.kind {
            TargetKind::Executable => {
                if tgt.is_bundle_on_apple() {
                    bundle_install(CopyKind::Directory, "app");
                } else {
                    write_copy(
                        out,
                        indent,
                        opts,
                        CopyRule::new(&dest, CopyKind::Executable, &[location]),
                    );
                }
            }
            TargetKind::SharedLibrary => {
                if tgt.is_framework_on_apple() {
                    bundle_install(CopyKind::Directory, "framework");
                } else {
                    let mut files = vec![location];
                    if !tgt.is_archived_aix_shared_library()
                        && let Some(info) = target::import_info(tgt, &ctx.config)
                        && !info.soname.is_empty()
                    {
                        let so_name_file = format!(
                            "{}/{}",
                            target::directory(ctx, tgt, &ctx.config, target::ArtifactType::Runtime),
                            info.soname
                        );
                        if so_name_file != files[0] {
                            files.push(so_name_file);
                        }
                    }
                    write_copy(
                        out,
                        indent,
                        opts,
                        CopyRule::new(&dest, CopyKind::Shared, &files),
                    );
                }
            }
            TargetKind::ModuleLibrary => {
                if tgt.is_cf_bundle_on_apple() {
                    bundle_install(CopyKind::Directory, "bundle");
                } else {
                    write_copy(
                        out,
                        indent,
                        opts,
                        CopyRule::new(&dest, CopyKind::Module, &[location]),
                    );
                }
            }
            _ => {}
        }
        return;
    }
    let names = target::names(ctx, tgt, &ctx.config);
    let (pair_kind, arguments, _mode, pairs) = target_files(
        ctx,
        tgt,
        &names,
        namelink,
        importlink,
        implib,
        &rule.backtrace,
    );
    kind = pair_kind;
    if pairs.is_empty() {
        return;
    }
    let files: Vec<String> = pairs.iter().map(|(from, _)| from.clone()).collect();
    let to_names: Vec<String> = pairs.iter().map(|(_, to)| to.clone()).collect();
    let installname = is_on_opt(
        ctx.var(tgt.makefile, "CMAKE_PLATFORM_HAS_INSTALLNAME")
            .as_deref(),
    );
    let dynamic = !implib && namelink != Namelink::Only && tgt.kind != TargetKind::StaticLibrary;
    let line = dynamic.then(|| {
        let language = target::linker_language(ctx, tgt, &ctx.config);
        link::link_line(ctx, tgt, &ctx.config, &language)
    });
    // `AddRPathCheckRule`/`AddChrpathPatchRule` skip install-name platforms.
    let rpaths = line
        .as_ref()
        .and_then(|line| line.rpath_change.clone())
        .filter(|_| !installname);
    let rpath_dirs = line.as_ref().and_then(|line| line.rpath_dirs.clone());
    let cmp0095 = index_of("CMP0095").unwrap();
    let policy = tgt.policies.get(cmp0095).unwrap_or(PolicyStatus::Warn);
    let format_install_rpath = |value: &str| {
        if policy == PolicyStatus::New {
            escape_for_cmake(value)
        } else {
            format!("\"{value}\"")
        }
    };
    let tweak_dir = format!("$ENV{{DESTDIR}}{}/", absolute_dest(&dest));
    let to_refs: Vec<&String> = to_names.iter().collect();
    if let Some((_, new)) = &rpaths {
        let new = format_install_rpath(new);
        tweak(out, indent, &tweak_dir, &to_refs, |out, indent, file| {
            write!(out, "{indent}file(RPATH_CHECK\n{indent}     FILE \"{file}\"\n{indent}     RPATH {new})\n").unwrap();
        });
    }
    // `optional = Optional || ImportLibrary`.
    let copy_opts = InstallOptions {
        optional: opts.optional || implib,
        ..opts.clone()
    };
    write_copy(
        out,
        indent,
        &copy_opts,
        CopyRule {
            dest: &dest,
            kind,
            files: &files,
            dir_permissions: &[],
            rename: "",
            arguments: &arguments,
            files_var: "",
        },
    );
    let rpaths = rpaths.filter(|(old, new)| old != new);
    if let Some((_, new)) = &rpaths
        && policy == PolicyStatus::Warn
        && new.contains("${")
    {
        ctx.message(
            crate::messages::MessageType::AuthorWarning,
            rule.backtrace.clone(),
            format!(
                "{}\nRPATH entries for target '{}' will not be escaped in the intermediary cmake_install.cmake script.",
                crate::policy::policy_warning(cmp0095),
                tgt.name
            ),
        );
    }
    // `AddInstallNamePatchRule`: the -id and -change arguments for the
    // installed file.
    let tool = ctx.var_safe(tgt.makefile, "CMAKE_INSTALL_NAME_TOOL");
    let mut new_id = String::new();
    let mut remap = BTreeMap::new();
    if dynamic
        && !tool.is_empty()
        && matches!(
            tgt.kind,
            TargetKind::Executable | TargetKind::SharedLibrary | TargetKind::ModuleLibrary
        )
    {
        if tgt.kind == TargetKind::SharedLibrary {
            let for_build = install_name::install_name_dir_for_build_tree(ctx, tgt, &ctx.config);
            let for_install = install_name::install_name_dir_for_install_tree(
                ctx,
                tgt,
                &ctx.config,
                "${CMAKE_INSTALL_PREFIX}",
            );
            if for_build != for_install {
                new_id = format!("{for_install}{}", names.shared_object);
            }
        }
        if let Some(line) = &line {
            for key in &line.shared_libraries_linked {
                let Some(dep) = ctx.interp.ext.targets.get(key) else {
                    continue;
                };
                // The install_name of an imported target does not change.
                if dep.imported {
                    continue;
                }
                let for_build =
                    install_name::install_name_dir_for_build_tree(ctx, dep, &ctx.config);
                let for_install = install_name::install_name_dir_for_install_tree(
                    ctx,
                    dep,
                    &ctx.config,
                    "${CMAKE_INSTALL_PREFIX}",
                );
                if for_build != for_install {
                    let fname = target::names(ctx, dep, &ctx.config).shared_object;
                    remap.insert(
                        format!("{for_build}{fname}"),
                        format!("{for_install}{fname}"),
                    );
                }
            }
        }
    }
    // `AddChrpathPatchRule` for install_name_tool: ordered -delete_rpath
    // then -add_rpath entries, each path appearing once.
    let mut runpaths: Vec<(String, &'static str)> = Vec::new();
    if dynamic
        && installname
        && !tool.is_empty()
        && let Some((old_dirs, new_dirs)) = &rpath_dirs
    {
        let mut change: BTreeMap<String, Option<&'static str>> = BTreeMap::new();
        let mut ordered = Vec::new();
        for dir in old_dirs {
            if change.insert(dir.clone(), Some("delete")).is_none() {
                ordered.push(dir.clone());
            }
        }
        for dir in new_dirs {
            match change.get_mut(dir) {
                None => {
                    change.insert(dir.clone(), Some("add"));
                    ordered.push(dir.clone());
                }
                Some(slot) => *slot = None,
            }
        }
        let major = ctx
            .var_safe(tgt.makefile, "DARWIN_MAJOR_VERSION")
            .parse::<i64>();
        if major.is_ok_and(|major| major <= 9) && (!old_dirs.is_empty() || !new_dirs.is_empty()) {
            ctx.message(
                crate::messages::MessageType::Warning,
                rule.backtrace.clone(),
                format!(
                    "WARNING: Target \"{}\" has runtime paths which cannot be changed during install.  To change runtime paths, OS X version 10.6 or newer is required.  Therefore, runtime paths will not be changed when installing.  CMAKE_BUILD_WITH_INSTALL_RPATH may be used to work around this limitation.",
                    tgt.name
                ),
            );
        } else {
            for dir in ordered {
                if let Some(op) = change[&dir] {
                    runpaths.push((dir, op));
                }
            }
        }
    }
    let universal = target::apple_embedded(ctx, tgt)
        && is_on_opt(ctx.var(tgt.makefile, "XCODE").as_deref())
        && ctx
            .var(tgt.makefile, "XCODE_VERSION")
            .is_some_and(|version| !version_compare(CompareOp::Greater, "6", &version))
        && matches!(
            tgt.kind,
            TargetKind::Executable
                | TargetKind::StaticLibrary
                | TargetKind::SharedLibrary
                | TargetKind::ModuleLibrary
        )
        && is_on_opt(tgt.props.get("IOS_INSTALL_COMBINED"));
    let ranlib = if tgt.kind == TargetKind::StaticLibrary && tgt.platform.apple {
        ctx.var_safe(tgt.makefile, "CMAKE_RANLIB")
    } else {
        String::new()
    };
    let strip = if dynamic && !(tgt.platform.apple && is_on_opt(tgt.props.get("MACOSX_BUNDLE"))) {
        ctx.var_safe(tgt.makefile, "CMAKE_STRIP")
    } else {
        String::new()
    };
    let strip_args = if strip.is_empty() || !tgt.platform.apple {
        ""
    } else if matches!(
        tgt.kind,
        TargetKind::SharedLibrary | TargetKind::ModuleLibrary
    ) {
        "-x "
    } else if tgt.kind == TargetKind::Executable && crate::util::filename_name(&strip) == "strip" {
        // `cmGlobalGenerator::GetStripCommandStyle` probe approximation.
        "-u -r "
    } else {
        ""
    };
    if !new_id.is_empty()
        || !remap.is_empty()
        || !runpaths.is_empty()
        || rpaths.is_some()
        || universal
        || !ranlib.is_empty()
        || !strip.is_empty()
    {
        tweak(out, indent, &tweak_dir, &to_refs, |out, indent, file| {
            if !new_id.is_empty() || !remap.is_empty() {
                write!(out, "{indent}execute_process(COMMAND \"{tool}\"").unwrap();
                if !new_id.is_empty() {
                    write!(out, "\n{indent}  -id \"{new_id}\"").unwrap();
                }
                for (old, new) in &remap {
                    write!(out, "\n{indent}  -change \"{old}\" \"{new}\"").unwrap();
                }
                writeln!(out, "\n{indent}  \"{file}\")").unwrap();
            }
            if let Some((old, new)) = &rpaths {
                let old = escape_for_cmake(old);
                let new = format_install_rpath(new);
                write!(out, "{indent}file(RPATH_CHANGE\n{indent}     FILE \"{file}\"\n{indent}     OLD_RPATH {old}\n{indent}     NEW_RPATH {new}").unwrap();
                if is_on_opt(tgt.props.get("INSTALL_REMOVE_ENVIRONMENT_RPATH")) {
                    write!(out, "\n{indent}     INSTALL_REMOVE_ENVIRONMENT_RPATH").unwrap();
                }
                out.push_str(")\n");
            }
            if !runpaths.is_empty() {
                writeln!(out, "{indent}execute_process(COMMAND {tool}").unwrap();
                for (dir, op) in &runpaths {
                    writeln!(out, "{indent}  -{op}_rpath \"{dir}\"").unwrap();
                }
                writeln!(out, "{indent}  \"{file}\")").unwrap();
            }
            if universal {
                write!(
                        out,
                        "{indent}include(CMakeIOSInstallCombined)\n{indent}ios_install_combined(\"{}\" \"{file}\")\n",
                        tgt.name
                    )
                    .unwrap();
            }
            if !ranlib.is_empty() {
                writeln!(
                    out,
                    "{indent}execute_process(COMMAND \"{ranlib}\" \"{file}\")"
                )
                .unwrap();
            }
            if !strip.is_empty() {
                write!(out, "{indent}if(CMAKE_INSTALL_DO_STRIP)\n{indent}  execute_process(COMMAND \"{strip}\" {strip_args}\"{file}\")\n{indent}endif()\n").unwrap();
            }
        });
    }
}

/// `computeFilesToInstall`: the artifact/interface/implementation names a
/// namelink mode selects.  `output` is the link name, `library` the
/// interface (soname) name, and `real` the implementation name.  Returns
/// whether the artifact has a namelink (`files.NamelinkMode` is then set).
fn compute_install_names(
    to: &mut Vec<String>,
    mode: Namelink,
    output: &str,
    library: &str,
    real: &str,
) -> bool {
    let have_soname = !library.is_empty() && library != output;
    let have_real = real != output && real != library;
    if have_soname || have_real {
        if mode == Namelink::Only {
            to.push(output.to_owned());
        } else {
            if have_real {
                to.push(real.to_owned());
            }
            if have_soname {
                to.push(library.to_owned());
            }
            if mode != Namelink::Skip {
                to.push(output.to_owned());
            }
        }
        true
    } else {
        if mode != Namelink::Only {
            to.push(output.to_owned());
        }
        false
    }
}

fn tweak(
    out: &mut String,
    indent: &str,
    dir: &str,
    names: &[&String],
    body: impl FnOnce(&mut String, &str, &str),
) {
    let loop_files = names.len() != 1;
    let nested = format!("{indent}  ");
    let (local, file) = if loop_files {
        writeln!(out, "{indent}foreach(file").unwrap();
        for name in names {
            writeln!(out, "{indent}    \"{dir}{name}\"").unwrap();
        }
        writeln!(out, "{indent}    )").unwrap();
        (nested.as_str(), "${file}".to_owned())
    } else {
        (indent, format!("{dir}{}", names[0]))
    };
    write!(
        out,
        "{local}if(EXISTS \"{file}\" AND\n{local}   NOT IS_SYMLINK \"{file}\")\n"
    )
    .unwrap();
    body(out, &format!("{local}  "), &file);
    writeln!(out, "{local}endif()").unwrap();
    if loop_files {
        writeln!(out, "{indent}endforeach()").unwrap();
    }
}

pub(super) fn file_set_properties(
    tgt: &Target,
    set: &str,
) -> Option<(String, String, &'static str)> {
    let suffix = |default: &str| {
        if set == default {
            String::new()
        } else {
            format!("_{set}")
        }
    };
    for (default, prefix, kind) in [
        ("HEADERS", "HEADER", "HEADERS"),
        ("CXX_MODULES", "CXX_MODULE", "CXX_MODULES"),
    ] {
        let suffix = suffix(default);
        if let Some(files) = tgt.props.get(&format!("{prefix}_SET{suffix}")) {
            let dirs = tgt
                .props
                .get(&format!("{prefix}_DIRS{suffix}"))
                .unwrap_or("");
            return Some((files.to_owned(), dirs.to_owned(), kind));
        }
    }
    None
}

fn render_file_set(
    ctx: &GenCtx,
    (out, indent): (&mut String, &str),
    index: usize,
    opts: &InstallOptions,
    tgt: &Target,
    set: &str,
) {
    if !["INTERFACE_HEADER_SETS", "INTERFACE_CXX_MODULE_SETS"]
        .iter()
        .any(|property| {
            expand_list(tgt.props.get(property).unwrap_or_default(), false)
                .iter()
                .any(|name| name == set)
        })
    {
        return;
    }
    let Some((files, bases, _)) = file_set_properties(tgt, set) else {
        return;
    };
    let src = &ctx.interp.makefiles[tgt.makefile].source_dir;
    let resolver = |p: &str| crate::util::collapse_full_path(p, src);
    let bases: Vec<_> = expand_list(&ctx.genex(&bases, Some(tgt), None), false)
        .iter()
        .map(|p| resolver(p))
        .collect();
    let files = expand_list(&ctx.genex(&files, Some(tgt), None), false);
    let mut groups: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for file in files {
        let file = resolver(&file);
        let relative = bases
            .iter()
            .find_map(|base| Path::new(&file).strip_prefix(base).ok());
        if let Some(dir) = relative.and_then(Path::parent) {
            groups
                .entry(dir.to_string_lossy().into_owned())
                .or_default()
                .push(file);
        }
    }
    let dest = genex_at(ctx, index, &opts.destination);
    for (dir, files) in groups {
        let dest = if dir.is_empty() {
            dest.clone()
        } else {
            format!("{dest}/{dir}")
        };
        write_copy(
            out,
            indent,
            opts,
            CopyRule::new(&dest, CopyKind::File, &files),
        );
    }
}

// ---------------------------------------------------------------------
/// `WriteMultiArgument`.
fn write_dep_argument(out: &mut String, indent: &str, keyword: &str, items: &[String]) {
    if !items.is_empty() {
        writeln!(out, "{indent}  {keyword}").unwrap();
    }
    for item in items {
        writeln!(out, "{indent}    {item}").unwrap();
    }
}

/// `GetTargetDependsClosure` (cmInstallRuntimeDependencySet.cxx): the keys of
/// the linked executables, shared libraries and modules.
fn runtime_dep_closure(
    ctx: &GenCtx,
    target: &Target,
    visited: &mut BTreeSet<String>,
    found: &mut BTreeSet<String>,
) {
    for depend in link::target_depends(ctx, target).iter() {
        if depend.cross || !depend.link {
            continue;
        }
        let dependee = &ctx.interp.ext.targets[&depend.key];
        if matches!(
            dependee.kind,
            TargetKind::Executable | TargetKind::SharedLibrary | TargetKind::ModuleLibrary
        ) {
            found.insert(depend.key.clone());
        }
        if visited.insert(depend.key.clone()) {
            runtime_dep_closure(ctx, dependee, visited, found);
        }
    }
}

/// `cmInstallGetRuntimeDependenciesGenerator::GenerateScriptForConfig`
/// without the install_name_tool arguments, which exist only on Apple.
fn render_get_runtime_dependencies(
    ctx: &GenCtx,
    out: &mut String,
    index: usize,
    set: &str,
    filters: &BTreeMap<String, Vec<String>>,
    indent: &str,
) {
    let full_path = |target: &Target| {
        target::full_path(
            ctx,
            target,
            &ctx.config,
            target::ArtifactType::Runtime,
            false,
        )
    };
    let dep_set = ctx.interp.ext.install.runtime_sets.get(set);
    let empty = Vec::new();
    let lookup = |items: &Vec<(usize, String)>| {
        items
            .iter()
            .filter_map(|(dir, name)| ctx.target_at(*dir, name))
            .collect::<Vec<&Target>>()
    };
    writeln!(
        out,
        "{indent}file(GET_RUNTIME_DEPENDENCIES\n{indent}  RESOLVED_DEPENDENCIES_VAR _CMAKE_DEPS"
    )
    .unwrap();
    let mut closure = BTreeSet::new();
    for (keyword, items) in [
        (
            "EXECUTABLES",
            dep_set.map_or(&empty, |set| &set.executables),
        ),
        ("LIBRARIES", dep_set.map_or(&empty, |set| &set.libraries)),
        ("MODULES", dep_set.map_or(&empty, |set| &set.modules)),
    ] {
        let mut paths = Vec::new();
        for target in lookup(items) {
            paths.push(format!("\"{}\"", full_path(target)));
            if !target.imported {
                runtime_dep_closure(ctx, target, &mut BTreeSet::new(), &mut closure);
            }
        }
        write_dep_argument(out, indent, keyword, &paths);
    }
    if let Some((dir, name)) = dep_set.and_then(|set| set.bundle_executable.as_ref())
        && let Some(target) = ctx.target_at(*dir, name)
    {
        writeln!(out, "{indent}  BUNDLE_EXECUTABLE \"{}\"", full_path(target)).unwrap();
    }
    for keyword in [
        "DIRECTORIES",
        "PRE_INCLUDE_REGEXES",
        "PRE_EXCLUDE_REGEXES",
        "POST_INCLUDE_REGEXES",
        "POST_EXCLUDE_REGEXES",
        "POST_INCLUDE_FILES",
        "POST_EXCLUDE_FILES",
    ] {
        let evaluated: Vec<String> = filters
            .get(keyword)
            .into_iter()
            .flatten()
            .map(|value| genex_at(ctx, index, value))
            .filter(|value| !value.is_empty())
            .map(|value| escape_for_cmake(&value))
            .collect();
        write_dep_argument(out, indent, keyword, &evaluated);
    }
    let post_exclude: BTreeSet<String> = closure
        .iter()
        .map(|key| format!("\"{}\"", full_path(&ctx.interp.ext.targets[key])))
        .collect();
    write_dep_argument(
        out,
        indent,
        "POST_EXCLUDE_FILES_STRICT",
        &post_exclude.into_iter().collect::<Vec<_>>(),
    );
    writeln!(out, "{indent}  )").unwrap();
}

/// `cmInstallRuntimeDependencySetGenerator::GenerateScriptForConfig`.
/// `install()` creates these with `NoInstallName` and `NoInstallRPath`,
/// so with an install_name_tool only the library/framework split and the
/// `get_filename_component` line appear; there are no rpath or `-id` edits.
fn render_runtime_dependency_set(
    ctx: &GenCtx,
    out: &mut String,
    index: usize,
    rule: &InstallRule,
    indent: &str,
) {
    let InstallAction::RuntimeDepSet {
        kind,
        destination,
        permissions,
        ..
    } = &rule.action
    else {
        return;
    };
    let dest = genex_at(ctx, index, destination);
    let opts = InstallOptions {
        permissions: permissions.clone(),
        ..InstallOptions::default()
    };
    let copy = |out: &mut String, indent: &str, kind: CopyKind, var: &str, args: &str| {
        write_copy(
            out,
            indent,
            &opts,
            CopyRule {
                files_var: var,
                arguments: args,
                ..CopyRule::new(&dest, kind, &[])
            },
        );
    };
    let tool = ctx.var_safe(index, "CMAKE_INSTALL_NAME_TOOL");
    writeln!(out, "{indent}foreach(_CMAKE_TMP_dep IN LISTS _CMAKE_DEPS)").unwrap();
    let inner = format!("{indent}  ");
    let body = format!("{inner}  ");
    if !tool.is_empty() {
        match kind {
            DepSetKind::Library => {
                writeln!(
                    out,
                    "{inner}if(NOT _CMAKE_TMP_dep MATCHES \"\\\\.framework/\")"
                )
                .unwrap();
                copy(
                    out,
                    &body,
                    CopyKind::Shared,
                    "_CMAKE_TMP_dep",
                    " FOLLOW_SYMLINK_CHAIN",
                );
                writeln!(
                    out,
                    "{body}get_filename_component(_CMAKE_TMP_dep_name \"${{_CMAKE_TMP_dep}}\" NAME)"
                )
                .unwrap();
                writeln!(out, "{inner}endif()").unwrap();
            }
            DepSetKind::Framework => {
                writeln!(
                    out,
                    "{inner}if(_CMAKE_TMP_dep MATCHES \"^(.*/)?([^/]*\\\\.framework)/(.*)$\")"
                )
                .unwrap();
                writeln!(out, "{body}set(_CMAKE_TMP_dir \"${{CMAKE_MATCH_1}}\")").unwrap();
                writeln!(out, "{body}set(_CMAKE_TMP_name \"${{CMAKE_MATCH_2}}\")").unwrap();
                writeln!(out, "{body}set(_CMAKE_TMP_file \"${{CMAKE_MATCH_3}}\")").unwrap();
                writeln!(
                    out,
                    "{body}set(_CMAKE_TMP_path \"${{_CMAKE_TMP_dir}}${{_CMAKE_TMP_name}}\")"
                )
                .unwrap();
                copy(
                    out,
                    &body,
                    CopyKind::Directory,
                    "_CMAKE_TMP_path",
                    " USE_SOURCE_PERMISSIONS",
                );
                writeln!(out, "{inner}endif()").unwrap();
            }
        }
    } else {
        copy(
            out,
            &inner,
            CopyKind::Shared,
            "_CMAKE_TMP_dep",
            " FOLLOW_SYMLINK_CHAIN",
        );
    }
    writeln!(out, "{indent}endforeach()").unwrap();
}
