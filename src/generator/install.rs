//! Single-config Ninja install script generation.
use std::collections::BTreeMap;
use std::fmt::Write;
use std::path::Path;

use super::{ctx::GenCtx, export, package_info, target};
use crate::commands::install::{legacy_source, unix_slashes};
use crate::fsutil::write_if_different;
use crate::install_model::*;
use crate::policy::{PolicyStatus, index_of};
use crate::state::{Target, TargetKind};
use crate::util::{escape_for_cmake, expand_list, is_full_path, is_on_opt};

pub fn write_install_scripts(ctx: &GenCtx) -> std::io::Result<()> {
    let mut files = Vec::new();
    for (index, mf) in ctx.interp.makefiles.iter().enumerate() {
        if is_on_opt(ctx.var(index, "CMAKE_SKIP_INSTALL_RULES").as_deref()) { continue; }
        let file = format!("{}/cmake_install.cmake", mf.binary_dir);
        let script = install_script(ctx, index)?;
        // cmGeneratedFileStream without SetCopyIfDifferent: always rewritten,
        // so the RERUN_CMAKE outputs are newer than CMakeCache.txt.
        crate::fsutil::write_generated(Path::new(&file), script.as_bytes())?;
        files.push(file);
    }
    if !is_on_opt(ctx.var(0, "CMAKE_SKIP_INSTALL_RULES").as_deref()) {
        let parallel = is_on_opt(ctx.interp.global_props.get("INSTALL_PARALLEL"));
        let json = install_index(&files, parallel);
        write_if_different(
            &Path::new(&ctx.interp.home_binary).join("CMakeFiles/InstallScripts.json"), json.as_bytes()
        )?;
    }
    Ok(())
}

pub fn install_index(files: &[String], parallel: bool) -> String {
    let files: Vec<_> = files.iter().map(|path| serde_json::to_string(path).unwrap()).collect();
    let files = if files.is_empty() { "[]".into() } else {
        format!("\n\t[\n\t\t{}\n\t]", files.join(",\n\t\t"))
    };
    format!("{{\n\t\"InstallScripts\" : {files},\n\t\"Parallel\" : {parallel}\n}}\n")
}

pub(super) fn directory_policy(ctx: &GenCtx, index: usize, name: &str) -> PolicyStatus {
    let Some(policy) = index_of(name) else { return PolicyStatus::Warn; };
    ctx.interp.makefiles[index].policies.iter().rev().find_map(|p| p.map.get(policy))
        .unwrap_or(PolicyStatus::Warn)
}

pub(super) fn config_test(variable: &str, configs: &[String]) -> String {
    let mut out = format!("{variable} MATCHES \"^(");
    for (index, config) in configs.iter().enumerate() {
        if index != 0 { out.push('|'); }
        for ch in config.chars() {
            if ch.is_ascii_alphabetic() {
                write!(out, "[{}{}]", ch.to_ascii_uppercase(), ch.to_ascii_lowercase()).unwrap();
            } else { out.push(ch); }
        }
    }
    out.push_str(")$\"");
    out
}

pub fn install_script(ctx: &GenCtx, index: usize) -> std::io::Result<String> {
    let mf = &ctx.interp.makefiles[index];
    let binary = &mf.binary_dir;
    let source = &mf.source_dir;
    let prefix = ctx.var(index, "CMAKE_STAGING_PREFIX").or_else(|| ctx.var(index, "CMAKE_INSTALL_PREFIX"))
        .unwrap_or_else(|| "/usr/local".into());
    let mut out = format!("# Install script for directory: {source}\n\n");
    out.push_str(&format!(r#"# Set the install prefix
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

"#, ctx.config));
    out.push_str(r#"# Set the component getting installed.
if(NOT CMAKE_INSTALL_COMPONENT)
  if(COMPONENT)
    message(STATUS "Install component: \"${COMPONENT}\"")
    set(CMAKE_INSTALL_COMPONENT "${COMPONENT}")
  else()
    set(CMAKE_INSTALL_COMPONENT)
  endif()
endif()

"#);
    for (key, comment) in [
        ("CMAKE_INSTALL_SO_NO_EXE", "Install shared libraries without execute permission?"),
        ("CMAKE_CROSSCOMPILING", "Is this installation the result of a crosscompile?"),
        ("CMAKE_INSTALL_DEFAULT_DIRECTORY_PERMISSIONS", "Set default install directory permissions."),
        ("CMAKE_GET_RUNTIME_DEPENDENCIES_PLATFORM", "Set OS and executable format for runtime-dependencies."),
        ("CMAKE_GET_RUNTIME_DEPENDENCIES_TOOL", "Set tool for dependency-resolution of runtime-dependencies."),
        ("CMAKE_GET_RUNTIME_DEPENDENCIES_COMMAND", "Set path to tool for dependency-resolution of runtime-dependencies."),
        ("CMAKE_OBJDUMP", "Set path to fallback-tool for dependency-resolution."),
    ] {
        if let Some(value) = ctx.var(index, key) {
            write!(out, "# {comment}\nif(NOT DEFINED {key})\n  set({key} \"{value}\")\nendif()\n\n").unwrap();
        }
    }
    let interleave = directory_policy(ctx, index, "CMP0082") == PolicyStatus::New;
    let mut children = mf.children.iter();
    let mut previous = 0;
    let rules = ctx.interp.ext.install.rules.get(&index).map_or(&[][..], Vec::as_slice);
    for rule in rules.iter().filter(|rule| !matches!(rule.action, InstallAction::LegacyTarget(_) | InstallAction::LegacyFiles { .. })) {
        if interleave {
            for &child in children.by_ref().take(rule.preceding_children.saturating_sub(previous)) {
                include_child(ctx, &mut out, child);
            }
            previous = rule.preceding_children;
        }
        render_rule(ctx, &mut out, index, rule)?;
    }
    if interleave {
        for &child in children { include_child(ctx, &mut out, child); }
    }
    if directory_policy(ctx, index, "CMP0082") == PolicyStatus::Warn {
        cmp0082_warning(ctx, index, rules);
    }
    for rule in rules.iter().filter(|r| matches!(r.action, InstallAction::LegacyFiles { .. })) {
        render_rule(ctx, &mut out, index, rule)?;
    }
    for tgt in ctx.targets_in_order().into_iter().filter(|t| t.makefile == index && !t.imported && t.kind != TargetKind::InterfaceLibrary) {
        let legacy_script = |out: &mut String, prop: &str| -> std::io::Result<()> {
            if let Some(text) = tgt.props.get(prop) {
                render_rule(ctx, out, index, &InstallRule {
                    options: InstallOptions::default(),
                    action: InstallAction::Script { text: text.into(), is_code: false, all_components: false },
                    preceding_children: 0,
                    backtrace: Vec::new(),
                })?;
            }
            Ok(())
        };
        legacy_script(&mut out, "PRE_INSTALL_SCRIPT")?;
        if let Some(rule) = rules.iter().rev().find(|rule| matches!(&rule.action, InstallAction::LegacyTarget(name) if *name == tgt.name)) {
            render_rule(ctx, &mut out, index, rule)?;
        }
        legacy_script(&mut out, "POST_INSTALL_SCRIPT")?;
    }
    if !interleave && !mf.children.is_empty() {
        out.push_str("if(NOT CMAKE_INSTALL_LOCAL_ONLY)\n  # Include the install script for each subdirectory.\n");
        for &c in &mf.children {
            let child = &ctx.interp.makefiles[c];
            if !is_on_opt(child.props.get("EXCLUDE_FROM_ALL")) {
                writeln!(out, "  include(\"{}/cmake_install.cmake\")", unix_slashes(&child.binary_dir)).unwrap();
            }
        }
        out.push_str("\nendif()\n\n");
    }
    out.push_str(&format!(r#"string(REPLACE ";" "\n" CMAKE_INSTALL_MANIFEST_CONTENT
       "${{CMAKE_INSTALL_MANIFEST_FILES}}")
if(CMAKE_INSTALL_LOCAL_ONLY)
  file(WRITE "{binary}/install_local_manifest.txt"
     "${{CMAKE_INSTALL_MANIFEST_CONTENT}}")
endif()
"#));
    if *binary == ctx.interp.home_binary {
        out.push_str(&format!(r#"if(CMAKE_INSTALL_COMPONENT)
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
"#));
    }
    Ok(out)
}

/// `cmInstallSubdirectoryGenerator::HaveInstall`: the directory, or one
/// of its subdirectories, has an install generator.
fn have_install(ctx: &GenCtx, dir: usize) -> bool {
    let has_rules = ctx.interp.ext.install.rules.get(&dir).is_some_and(|rules| {
        rules.iter().any(|rule| !matches!(rule.action, InstallAction::LegacyTarget(_)))
    });
    has_rules || ctx.interp.makefiles[dir].children.iter().any(|&child| have_install(ctx, child))
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

fn render_rule(ctx: &GenCtx, out: &mut String, index: usize, rule: &InstallRule) -> std::io::Result<()> {
    let opts = &rule.options;
    if let InstallAction::RuntimeDependencies { set, filters } = &rule.action {
        if ctx.interp.ext.install.runtime_sets.get(set).is_some_and(|targets| !targets.is_empty()) {
            ctx.error(rule.backtrace.clone(), format!(
                "install runtime dependency set \"{set}\" is not supported by cmake-rust ({} filter groups).", filters.len()
            ));
        }
        return Ok(());
    }
    let all = matches!(rule.action, InstallAction::Script { all_components: true, .. });
    let conditional = !all || opts.exclude;
    if all {
        if opts.exclude { out.push_str("if(CMAKE_INSTALL_COMPONENT)\n"); }
    } else {
        let other = if opts.exclude { "" } else { " OR NOT CMAKE_INSTALL_COMPONENT" };
        writeln!(out, "if(CMAKE_INSTALL_COMPONENT STREQUAL \"{}\"{other})", opts.component).unwrap();
    }
    let mut indent = if all { "" } else { "  " };
    let config = !opts.configurations.is_empty();
    if config {
        writeln!(out, "{indent}if({})", config_test("CMAKE_INSTALL_CONFIG_NAME", &opts.configurations)).unwrap();
        indent = if all { "  " } else { "    " };
    }
    match &rule.action {
        InstallAction::Copy { kind, paths, dir_permissions, arguments } => {
            // INTEGRATION: file and directory destination and path expressions
            // are evaluated in the generating makefile.
            let dest = ctx.genex(&opts.destination, None, None);
            let rename = ctx.genex(&opts.rename, None, None);
            let mut evaluated = Vec::new();
            let is_directory = matches!(kind, CopyKind::Directory);
            let files = if opts.destination.contains("$<") || opts.rename.contains("$<") || paths.iter().any(|p| p.contains("$<")) {
                for path in paths {
                    evaluated.extend(expand_list(&ctx.genex(path, None, None), false));
                }
                if is_directory {
                    for path in &mut evaluated {
                        if !is_full_path(path) { *path = format!("{}/{path}", ctx.interp.makefiles[index].source_dir); }
                    }
                }
                &evaluated
            } else { paths };
            write_copy(out, indent, opts, CopyRule { dest: &dest, kind: *kind, files, dir_permissions, rename: &rename, arguments });
        }
        InstallAction::Script { text, is_code, .. } => {
            // INTEGRATION: CMP0087 enables evaluation of install script arguments.
            let text = if text.contains("$<") && directory_policy(ctx, index, "CMP0087") == PolicyStatus::New {
                ctx.genex(&text.replace("$<INSTALL_PREFIX>", "${CMAKE_INSTALL_PREFIX}"), None, None)
            } else { text.clone() };
            if *is_code { writeln!(out, "{indent}{text}").unwrap(); }
            else { writeln!(out, "{indent}include(\"{text}\")").unwrap(); }
        }
        InstallAction::Target { name, namelink, imported } => {
            if let Some(tgt) = ctx.target_at(index, name) {
                render_target(ctx, out, opts, tgt, *namelink, *imported, indent);
            }
        }
        InstallAction::Bmi(name) => {
            if let Some(tgt) = ctx.target_at(index, name) {
                let objects = target::object_directory(ctx, tgt, &ctx.config);
                // cmInstallCxxModuleBmiGenerator::GetScriptLocation: the
                // configuration as given, or "noconfig".
                let config = if ctx.config.is_empty() { "noconfig" } else { ctx.config.as_str() };
                writeln!(out, "{indent}include(\"{objects}install-cxx-module-bmi-{config}.cmake\" OPTIONAL)").unwrap();
            }
        }
        InstallAction::FileSet { name, set } => {
            if let Some(tgt) = ctx.target_at(index, name) {
                render_file_set(ctx, out, opts, tgt, set, indent);
            }
        }
        InstallAction::Export(export) => export::write_install_export(ctx, out, index, rule, export, indent)?,
        InstallAction::PackageInfo { set, arguments } => {
            package_info::write_install(ctx, out, index, rule, set, arguments, indent)?
        }
        InstallAction::LegacyFiles { args, programs } => {
            let opts = InstallOptions {
                component: ctx.var(index, "CMAKE_INSTALL_DEFAULT_COMPONENT_NAME").unwrap_or_else(|| opts.component.clone()),
                ..opts.clone()
            };
            let files = legacy_files(ctx, index, args, *programs);
            let kind = if *programs { CopyKind::Program } else { CopyKind::File };
            write_copy(out, indent, &opts, CopyRule::new(&opts.destination, kind, &files));
        }
        InstallAction::LegacyTarget(name) => {
            if let Some(tgt) = ctx.target(name).filter(|t| matches!(t.kind, TargetKind::Executable | TargetKind::StaticLibrary | TargetKind::SharedLibrary | TargetKind::ModuleLibrary)) {
                render_target(ctx, out, opts, tgt, Namelink::None, false, indent);
            }
        }
        InstallAction::RuntimeDependencies { .. } => unreachable!(),
    }
    if config { writeln!(out, "{}endif()", if all { "" } else { "  " }).unwrap(); }
    if conditional { out.push_str("endif()\n\n"); }
    Ok(())
}

fn legacy_files(ctx: &GenCtx, index: usize, args: &[String], programs: bool) -> Vec<String> {
    let mf = &ctx.interp.makefiles[index];
    let names = if args[0] == "FILES" || (programs && args.len() > 1) {
        let start = usize::from(args[0] == "FILES");
        args[start..].to_vec()
    } else if args.len() > 1 {
        args[1..].iter().map(|name| {
            let start = name.rfind('/').map_or(0, |i| i + 1);
            let end = name[start..].rfind('.').map_or(name.len(), |i| start + i);
            format!("{}{}", &name[..end], args[0])
        }).collect()
    } else {
        let Ok(regex) = crate::cmregex::Regex::new(&args[0]) else { return Vec::new(); };
        let mut names: Vec<_> = std::fs::read_dir(&mf.source_dir).into_iter().flatten().filter_map(Result::ok)
            .filter_map(|entry| entry.file_name().into_string().ok())
            .filter(|name| regex.find(name).is_some()).collect();
        names.sort();
        names
    };
    names.iter().map(|name| legacy_source(&mf.source_dir, &mf.binary_dir, name)).collect()
}

pub(super) fn absolute_dest(dest: &str) -> String {
    if dest.is_empty() || is_full_path(dest) { dest.into() }
    else { format!("${{CMAKE_INSTALL_PREFIX}}/{dest}") }
}

pub(super) struct CopyRule<'a> {
    pub dest: &'a str,
    pub kind: CopyKind,
    pub files: &'a [String],
    pub dir_permissions: &'a [String],
    pub rename: &'a str,
    pub arguments: &'a str,
}

impl<'a> CopyRule<'a> {
    pub(super) fn new(dest: &'a str, kind: CopyKind, files: &'a [String]) -> Self {
        Self { dest, kind, files, dir_permissions: &[], rename: "", arguments: "" }
    }
}

pub(super) fn write_copy(out: &mut String, indent: &str, opts: &InstallOptions, rule: CopyRule<'_>) {
    let CopyRule { dest, kind, files, dir_permissions, rename, arguments } = rule;
    if is_full_path(dest) {
        if !files.is_empty() {
            let absolute: Vec<_> = files.iter().map(|file| {
                let name = if rename.is_empty() { file.rsplit('/').next().unwrap() } else { rename };
                format!("{dest}/{name}")
            }).collect();
            write!(out, "{indent}list(APPEND CMAKE_ABSOLUTE_DESTINATION_FILES\n{indent} \"{}\")\n", absolute.join(";")).unwrap();
        }
        write!(out, "{indent}if(CMAKE_WARN_ON_ABSOLUTE_INSTALL_DESTINATION)\n{indent}  message(WARNING \"ABSOLUTE path INSTALL DESTINATION : ${{CMAKE_ABSOLUTE_DESTINATION_FILES}}\")\n{indent}endif()\n").unwrap();
        write!(out, "{indent}if(CMAKE_ERROR_ON_ABSOLUTE_INSTALL_DESTINATION)\n{indent}  message(FATAL_ERROR \"ABSOLUTE path INSTALL DESTINATION forbidden (by caller): ${{CMAKE_ABSOLUTE_DESTINATION_FILES}}\")\n{indent}endif()\n").unwrap();
    }
    write!(out, "{indent}file(INSTALL DESTINATION \"{}\" TYPE {}", absolute_dest(dest), kind.token()).unwrap();
    if opts.optional { out.push_str(" OPTIONAL"); }
    if !opts.message.is_empty() { write!(out, " {}", opts.message).unwrap(); }
    if !opts.permissions.is_empty() { write!(out, " PERMISSIONS {}", opts.permissions.join(" ")).unwrap(); }
    if !dir_permissions.is_empty() { write!(out, " DIR_PERMISSIONS {}", dir_permissions.join(" ")).unwrap(); }
    if !rename.is_empty() { write!(out, " RENAME \"{rename}\"").unwrap(); }
    out.push_str(" FILES");
    if let [file] = files {
        write!(out, " \"{file}\"").unwrap();
    } else {
        for file in files { write!(out, "\n{indent}  \"{file}\"").unwrap(); }
        write!(out, "\n{indent} ").unwrap();
        if arguments.is_empty() { out.push(' '); }
    }
    writeln!(out, "{arguments})").unwrap();
}

pub(super) fn object_subdir(tgt: &Target, config: &str) -> String {
    if config.is_empty() { format!("objects/{}", tgt.name) }
    else { format!("objects-{config}/{}", tgt.name) }
}

pub(super) fn object_names(ctx: &GenCtx, tgt: &Target) -> Vec<String> {
    target::sources(ctx, tgt, &ctx.config).iter()
        .filter(|source| matches!(source.kind, target::SourceKind::ObjectSource))
        .map(|source| source.object_name.clone()).collect()
}

fn render_target(
    ctx: &GenCtx, out: &mut String, opts: &InstallOptions, tgt: &Target,
    namelink: Namelink, imported: bool, indent: &str,
) {
    let kind = match tgt.kind {
        TargetKind::Executable => CopyKind::Executable,
        TargetKind::StaticLibrary => CopyKind::Static,
        TargetKind::SharedLibrary => CopyKind::Shared,
        TargetKind::ModuleLibrary => CopyKind::Module,
        TargetKind::ObjectLibrary => CopyKind::File,
        _ => return,
    };
    // INTEGRATION: artifact destinations may depend on the target's config.
    let dest = ctx.genex(&opts.destination, None, None);
    if tgt.kind == TargetKind::ObjectLibrary {
        let files = object_names(ctx, tgt);
        if !files.is_empty() {
            let base = target::object_directory(ctx, tgt, &ctx.config);
            let dest = format!("{dest}/{}", object_subdir(tgt, &ctx.config));
            let arguments = format!(" FILES_FROM_DIR \"{base}./\"");
            write_copy(out, indent, opts, CopyRule {
                arguments: &arguments, ..CopyRule::new(&dest, kind, &files)
            });
        }
        return;
    }
    if imported {
        let location = target::full_path(ctx, tgt, &ctx.config, target::ArtifactType::Runtime, false);
        let mut files = vec![location];
        if tgt.kind == TargetKind::SharedLibrary
            && let Some(info) = target::import_info(tgt, &ctx.config)
            && !info.soname.is_empty()
        {
            let soname = format!("{}/{}", crate::util::filename_path(&files[0]), info.soname);
            if soname != files[0] { files.push(soname); }
        }
        write_copy(out, indent, opts, CopyRule::new(&dest, kind, &files));
        return;
    }
    let names = target::names(ctx, tgt, &ctx.config);
    let soname = if tgt.kind == TargetKind::Executable { "" } else { &names.shared_object };
    let have_soname = !soname.is_empty() && soname != names.output;
    let have_real = !names.real.is_empty() && names.real != names.output && names.real != soname;
    let mut install_names = Vec::new();
    if namelink != Namelink::Only {
        if have_real { install_names.push(&names.real); }
        if have_soname { install_names.push(&names.shared_object); }
    }
    let add_link = match namelink {
        Namelink::Only => have_real || have_soname,
        Namelink::Skip => !have_real && !have_soname,
        Namelink::None => true,
    };
    if add_link { install_names.push(&names.output); }
    if install_names.is_empty() { return; }
    let from = target::directory(ctx, tgt, &ctx.config, target::ArtifactType::Runtime);
    let files: Vec<_> = install_names.iter().map(|name| format!("{from}/{name}")).collect();
    let dynamic = tgt.kind != TargetKind::StaticLibrary && namelink != Namelink::Only;
    let rpaths = if dynamic { install_rpaths(ctx, tgt) } else { None };
    let tweak_dir = format!("$ENV{{DESTDIR}}{}/", absolute_dest(&dest));
    if let Some((_, new)) = &rpaths {
        tweak(out, indent, &tweak_dir, &install_names, |out, indent, file| {
            write!(out, "{indent}file(RPATH_CHECK\n{indent}     FILE \"{file}\"\n{indent}     RPATH {new})\n").unwrap();
        });
    }
    write_copy(out, indent, opts, CopyRule::new(&dest, kind, &files));
    let strip = if dynamic { ctx.var_safe(tgt.makefile, "CMAKE_STRIP") } else { String::new() };
    let rpaths = rpaths.filter(|(old, new)| old != new);
    if !strip.is_empty() || rpaths.is_some() {
        tweak(out, indent, &tweak_dir, &install_names, |out, indent, file| {
            if let Some((old, new)) = &rpaths {
                write!(out, "{indent}file(RPATH_CHANGE\n{indent}     FILE \"{file}\"\n{indent}     OLD_RPATH {old}\n{indent}     NEW_RPATH {new}").unwrap();
                if is_on_opt(tgt.props.get("INSTALL_REMOVE_ENVIRONMENT_RPATH")) {
                    write!(out, "\n{indent}     INSTALL_REMOVE_ENVIRONMENT_RPATH").unwrap();
                }
                out.push_str(")\n");
            }
            if !strip.is_empty() {
                write!(out, "{indent}if(CMAKE_INSTALL_DO_STRIP)\n{indent}  execute_process(COMMAND \"{strip}\" \"{file}\")\n{indent}endif()\n").unwrap();
            }
        });
    }
}

fn tweak(
    out: &mut String, indent: &str, dir: &str, names: &[&String],
    body: impl FnOnce(&mut String, &str, &str),
) {
    let loop_files = names.len() != 1;
    let nested = format!("{indent}  ");
    let (local, file) = if loop_files {
        writeln!(out, "{indent}foreach(file").unwrap();
        for name in names { writeln!(out, "{indent}    \"{dir}{name}\"").unwrap(); }
        writeln!(out, "{indent}    )").unwrap();
        (nested.as_str(), "${file}".to_owned())
    } else { (indent, format!("{dir}{}", names[0])) };
    write!(out, "{local}if(EXISTS \"{file}\" AND\n{local}   NOT IS_SYMLINK \"{file}\")\n").unwrap();
    body(out, &format!("{local}  "), &file);
    writeln!(out, "{local}endif()").unwrap();
    if loop_files { writeln!(out, "{indent}endforeach()").unwrap(); }
}

fn install_rpaths(ctx: &GenCtx, tgt: &Target) -> Option<(String, String)> {
    let on = |key| is_on_opt(ctx.var(tgt.makefile, key).as_deref());
    if on("CMAKE_SKIP_RPATH") || on("CMAKE_NO_BUILTIN_CHRPATH") ||
        is_on_opt(tgt.props.get("BUILD_WITH_INSTALL_RPATH")) ||
        ctx.var_safe(tgt.makefile, "CMAKE_EXECUTABLE_FORMAT") != "ELF" {
        return None;
    }
    let language = target::linker_language(ctx, tgt, &ctx.config);
    let separator = ctx.var_safe(tgt.makefile, &format!("CMAKE_SHARED_LIBRARY_RUNTIME_{language}_FLAG_SEP"));
    if separator.is_empty() { return None; }
    // INTEGRATION: use cmComputeLinkInformation's shared runtime search
    // paths once generator::link exposes them.
    let rpath = |key| expand_list(&ctx.genex(tgt.props.get(key).unwrap_or(""), Some(tgt), None), false);
    let mut old = if is_on_opt(tgt.props.get("SKIP_BUILD_RPATH")) { Vec::new() } else { rpath("BUILD_RPATH") };
    if !is_on_opt(tgt.props.get("SKIP_BUILD_RPATH")) {
        for item in expand_list(tgt.props.get("LINK_LIBRARIES").unwrap_or(""), false) {
            let dep = ctx.target(&item).filter(|t| t.kind == TargetKind::SharedLibrary);
            if let Some(dep) = dep {
                old.push(target::directory(ctx, dep, &ctx.config, target::ArtifactType::Runtime));
            } else if is_full_path(&item) && item.contains(".so") {
                old.push(crate::util::filename_path(&item));
            }
        }
    }
    let mut new = if on("CMAKE_SKIP_INSTALL_RPATH") { Vec::new() } else { rpath("INSTALL_RPATH") };
    if !on("CMAKE_SKIP_INSTALL_RPATH") && is_on_opt(tgt.props.get("INSTALL_RPATH_USE_LINK_PATH")) {
        new.extend(old.iter().filter(|dir| {
            let path = Path::new(dir);
            !path.starts_with(&ctx.interp.home_source) && !path.starts_with(&ctx.interp.home_binary)
        }).cloned());
    }
    let join = |paths: Vec<String>| {
        let mut unique = Vec::new();
        for path in paths {
            if !unique.contains(&path) { unique.push(path); }
        }
        unique.join(&separator)
    };
    let new = join(new);
    let mut old = join(old);
    if !old.is_empty() { old.push_str(&separator); }
    while old.len() < new.len() { old.push_str(&separator); }
    let old = escape_for_cmake(&old);
    let new = if directory_policy(ctx, tgt.makefile, "CMP0095") == PolicyStatus::New {
        escape_for_cmake(&new)
    } else { format!("\"{new}\"") };
    Some((old, new))
}

pub(super) fn file_set_properties(tgt: &Target, set: &str) -> Option<(String, String, &'static str)> {
    let suffix = |default: &str| if set == default { String::new() } else { format!("_{set}") };
    for (default, prefix, kind) in [("HEADERS", "HEADER", "HEADERS"), ("CXX_MODULES", "CXX_MODULE", "CXX_MODULES")] {
        let suffix = suffix(default);
        if let Some(files) = tgt.props.get(&format!("{prefix}_SET{suffix}")) {
            let dirs = tgt.props.get(&format!("{prefix}_DIRS{suffix}")).unwrap_or("");
            return Some((files.to_owned(), dirs.to_owned(), kind));
        }
    }
    None
}

fn render_file_set(ctx: &GenCtx, out: &mut String, opts: &InstallOptions, tgt: &Target, set: &str, indent: &str) {
    let Some((files, bases, _)) = file_set_properties(tgt, set) else { return; };
    // INTEGRATION: FILE_SET properties and destination accept genex.
    let src = &ctx.interp.makefiles[tgt.makefile].source_dir;
    let resolver = |p: &str| crate::util::collapse_full_path(p, src);
    let bases: Vec<_> = expand_list(&ctx.genex(&bases, Some(tgt), None), false).iter().map(|p| resolver(p)).collect();
    let files = expand_list(&ctx.genex(&files, Some(tgt), None), false);
    let mut groups: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for file in files {
        let file = resolver(&file);
        let relative = bases.iter().find_map(|base| Path::new(&file).strip_prefix(base).ok());
        if let Some(dir) = relative.and_then(Path::parent) {
            groups.entry(dir.to_string_lossy().into_owned()).or_default().push(file);
        }
    }
    let dest = ctx.genex(&opts.destination, None, None);
    for (dir, files) in groups {
        let dest = if dir.is_empty() { dest.clone() } else { format!("{dest}/{dir}") };
        write_copy(out, indent, opts, CopyRule::new(&dest, CopyKind::File, &files));
    }
}
