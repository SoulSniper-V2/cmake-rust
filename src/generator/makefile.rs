//! The Unix Makefiles generator.
//!
//! This is a port of `cmGlobalUnixMakefileGenerator3`,
//! `cmLocalUnixMakefileGenerator3`, `cmMakefileTargetGenerator`, and its
//! executable/library/utility subclasses.  Unlike Ninja, make evaluates the
//! rules in a single working directory (the top-level binary directory), so
//! target names are home-relative while commands are prefixed with `cd` to
//! the owning directory.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fmt::Write as _;
use std::path::Path;

use super::ctx::GenCtx;
use super::custom;
use super::flags;
use super::module_support::ModuleSupport;
use super::osx_bundle::{self, SourceFileType};
use super::target::{self as gt, ArtifactType, SourceKind};
use crate::policy::PolicyStatus;
use crate::state::CustomCommandKind;
use crate::state::{Target, TargetKind};
use crate::util::{self, is_on};

fn dir_support(ctx: &GenCtx<'_>, dir: usize) -> String {
    format!(
        "{}/CMakeFiles/CMakeDirectoryInformation.cmake",
        ctx.interp.makefiles[dir].binary_dir
    )
}

fn shell_escape(_ctx: &GenCtx<'_>, text: &str, _is_source: bool) -> String {
    crate::outconv::escape_for_shell(
        text,
        crate::outconv::FLAG_MAKE
            | crate::outconv::FLAG_IS_UNIX
            | crate::outconv::FLAG_ALLOW_MAKE_VARIABLES,
    )
}

fn shell_path(ctx: &GenCtx<'_>, path: &str) -> String {
    shell_escape(ctx, &crate::cache::convert_to_unix_slashes(path), false)
}

fn shell_path_keep_trailing_slash(ctx: &GenCtx<'_>, path: &str) -> String {
    let mut converted = shell_path(ctx, path);
    if path.ends_with('/') && !converted.ends_with('/') {
        converted.push('/');
    }
    converted
}

/// Convert backslashes without performing CMake's duplicate-slash collapsing.
/// A few upstream Makefile paths intentionally concatenate a directory that
/// already ends in `/` with another `/` and preserve the resulting `//`.
fn shell_path_preserve_slashes(ctx: &GenCtx<'_>, path: &str) -> String {
    shell_escape(ctx, &path.replace('\\', "/"), false)
}

fn top_rule_path_preserve_slashes(ctx: &GenCtx<'_>, path: &str) -> String {
    let home = format!("{}/", ctx.interp.home_binary);
    path.strip_prefix(&home).unwrap_or(path).to_owned()
}

fn maybe_relative_cur_preserve_slashes(ctx: &GenCtx<'_>, dir: usize, path: &str) -> String {
    let cur = format!("{}/", ctx.interp.makefiles[dir].binary_dir);
    if let Some(rest) = path.strip_prefix(&cur) {
        rest.to_owned()
    } else {
        maybe_relative_cur(ctx, dir, path)
    }
}

/// `cmOutputConverter::ConvertToOutputFormat(..., RESPONSE)` for object
/// lists embedded in a link script.  Response-file quoting intentionally
/// quotes leading or embedded hyphens even though the script is read by a
/// shell.
fn response_path(ctx: &GenCtx<'_>, path: &str) -> String {
    let _ = ctx;
    crate::outconv::escape_for_shell(
        &crate::cache::convert_to_unix_slashes(path),
        crate::outconv::FLAG_IS_UNIX | crate::outconv::FLAG_IS_RESPONSE,
    )
}

fn maybe_relative_top(ctx: &GenCtx<'_>, path: &str) -> String {
    custom::maybe_relative_to(ctx, 0, &ctx.interp.home_binary, path)
}

fn maybe_relative_cur(ctx: &GenCtx<'_>, dir: usize, path: &str) -> String {
    let cur = ctx.interp.makefiles[dir].binary_dir.clone();
    custom::maybe_relative_to(ctx, dir, &cur, path)
}

fn rule_launcher(ctx: &GenCtx<'_>, target: &Target, property: &str) -> String {
    let value = target
        .props
        .get(property)
        .or_else(|| ctx.interp.makefiles[target.makefile].props.get(property))
        .or_else(|| ctx.interp.global_props.get(property))
        .filter(|value| !value.is_empty());
    match value {
        Some(value) => format!("{} ", ctx.genex(value, Some(target), None)),
        None => String::new(),
    }
}

fn package_configs(ctx: &GenCtx<'_>) -> (bool, bool) {
    let root = &ctx.interp.home_binary;
    (
        Path::new(&format!("{root}/CPackConfig.cmake")).exists(),
        Path::new(&format!("{root}/CPackSourceConfig.cmake")).exists(),
    )
}

fn safe_object_file_name(text: &str) -> String {
    text.trim_start_matches('/')
        .replace(':', "_")
        .replace("../", "__/")
        .replace(' ', "_")
}

fn remove_unknown_tidy_fixes(dir: &Path, known: &BTreeSet<String>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if entry.file_type().is_ok_and(|kind| kind.is_dir()) {
            remove_unknown_tidy_fixes(&path, known);
        } else if path.extension().is_some_and(|ext| ext == "yaml")
            && path.to_str().is_some_and(|name| !known.contains(name))
        {
            let _ = std::fs::remove_file(path);
        }
    }
}

fn home_relative_output_path(ctx: &GenCtx<'_>, dir: usize) -> String {
    let binary = &ctx.interp.makefiles[dir].binary_dir;
    if binary == &ctx.interp.home_binary {
        String::new()
    } else {
        format!(
            "{}/",
            custom::maybe_relative_to(ctx, 0, &ctx.interp.home_binary, binary)
        )
    }
}

fn quote_output_path(path: &str) -> String {
    let mut result = String::new();
    let mut last = '\0';
    for ch in path.chars() {
        if ch == '"' && last != '\\' {
            result.push('\\');
        }
        result.push(ch);
        last = ch;
    }
    if path.contains(' ') {
        format!("\"{result}\"")
    } else {
        result
    }
}

fn strip_last_extension(path: &str) -> &str {
    path.rfind('.').map_or(path, |pos| &path[..pos])
}

fn makefile_target_version(target: &Target) -> (String, String) {
    let version = target.props.get("VERSION").unwrap_or("");
    let mut parts = version.split('.');
    let major = parts
        .next()
        .and_then(|part| part.parse::<i64>().ok())
        .unwrap_or(0);
    let minor = parts
        .next()
        .and_then(|part| part.parse::<i64>().ok())
        .unwrap_or(0);
    (major.to_string(), minor.to_string())
}

fn apply_verbose_default(ctx: &GenCtx<'_>, out: &mut String) {
    if ctx
        .var(0, "CMAKE_VERBOSE_MAKEFILE")
        .is_some_and(|v| is_on(&v))
    {
        *out = out.replace(
            ".SUFFIXES: .hpux_make_needs_suffix_list\n\n",
            ".SUFFIXES: .hpux_make_needs_suffix_list\n\n# Produce verbose output by default.\nVERBOSE = 1\n\n",
        );
    }
}

fn check_build_system_command(ctx: &GenCtx<'_>, cmakefile: &str, rerun: usize) -> String {
    let compile = if ctx.interp.ignore_compile_warning_as_error {
        "--compile-no-warning-as-error "
    } else {
        ""
    };
    let link = if ctx.interp.ignore_link_warning_as_error {
        "--link-no-warning-as-error "
    } else {
        ""
    };
    format!(
        "$(CMAKE_COMMAND) -S$(CMAKE_SOURCE_DIR) -B$(CMAKE_BINARY_DIR) {compile}{link}--check-build-system {cmakefile} {rerun}"
    )
}

fn ctest_command(ctx: &GenCtx<'_>) -> String {
    let ctest = format!(
        "{}/ctest",
        ctx.interp
            .cmake_command
            .rsplit_once('/')
            .map_or("", |(dir, _)| dir)
    );
    shell_path(ctx, &ctest)
}

fn apply_instrumentation_variable(ctx: &GenCtx<'_>, out: &mut String) {
    if !ctx.interp.instrument_build_hooks {
        return;
    }
    let ctest = ctest_command(ctx);
    let needle = "\n\n# The command to remove a file.\nRM = ";
    let replacement = format!(
        "\n# The CTest executable.\nCTEST_COMMAND = {ctest}\n\n# The command to remove a file.\nRM = "
    );
    *out = out.replacen(needle, &replacement, 1);
}

fn check_build_system_commands(ctx: &GenCtx<'_>, cmakefile: &str, rerun: usize) -> Vec<String> {
    let mut commands = Vec::new();
    if let Some((script, _)) = &ctx.interp.ext.glob_verify {
        commands.push(format!("$(CMAKE_COMMAND) -P {}", shell_path(ctx, script)));
    }
    commands.push(check_build_system_command(ctx, cmakefile, rerun));
    if ctx.interp.instrument_build_hooks {
        commands
            .push("exec $(CTEST_COMMAND) --start-instrumentation $(CMAKE_BINARY_DIR) ;".to_owned());
    }
    commands
}

fn makefile_response_flag(ctx: &GenCtx<'_>, target: &Target, language: &str) -> String {
    ctx.var(
        target.makefile,
        &format!("CMAKE_{language}_RESPONSE_FILE_LINK_FLAG"),
    )
    .filter(|flag| !flag.is_empty())
    .unwrap_or_else(|| "@".to_owned())
}

fn makefile_device_response_flag(ctx: &GenCtx<'_>, target: &Target) -> String {
    ctx.var(target.makefile, "CMAKE_CUDA_RESPONSE_FILE_DEVICE_LINK_FLAG")
        .unwrap_or_else(|| "@".to_owned())
}

fn makefile_cuda_compile_mode(ctx: &GenCtx<'_>, target: &Target) -> String {
    let mut mode = String::new();
    if target
        .props
        .get("CUDA_SEPARABLE_COMPILATION")
        .is_some_and(is_on)
    {
        mode.push_str(&ctx.var_safe(target.makefile, "_CMAKE_CUDA_RDC_FLAG"));
        mode.push(' ');
    }
    let mut normal = true;
    for (property, variable) in [
        ("CUDA_PTX_COMPILATION", "_CMAKE_CUDA_PTX_FLAG"),
        ("CUDA_CUBIN_COMPILATION", "_CMAKE_CUDA_CUBIN_FLAG"),
        ("CUDA_FATBIN_COMPILATION", "_CMAKE_CUDA_FATBIN_FLAG"),
        ("CUDA_OPTIX_COMPILATION", "_CMAKE_CUDA_OPTIX_FLAG"),
    ] {
        if target.props.get(property).is_some_and(is_on) {
            mode.push_str(&ctx.var_safe(target.makefile, variable));
            normal = false;
            break;
        }
    }
    if normal {
        mode.push_str(&ctx.var_safe(target.makefile, "_CMAKE_CUDA_WHOLE_FLAG"));
    }
    mode
}

fn escape_json(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        match ch {
            '"' | '\\' => {
                out.push('\\');
                out.push(ch);
            }
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            _ => out.push(ch),
        }
    }
    out
}

/// `cmMakefileTargetGenerator::WriteObjectRuleFile` ISPC header path logic.
fn ispc_header_for_source(
    ctx: &GenCtx<'_>,
    target: &Target,
    config: &str,
    object_name: &str,
) -> String {
    let suffix = target.props.get("ISPC_HEADER_SUFFIX").unwrap_or_default();
    let mut directory = gt::object_directory(ctx, target, config);
    if let Some(prop) = target.props.get("ISPC_HEADER_DIRECTORY") {
        directory = util::collapse_full_path(
            &format!("{}/{prop}", ctx.interp.makefiles[0].binary_dir),
            "",
        );
    }
    format!(
        "{directory}/{}{suffix}",
        super::ispc::source_stem(object_name)
    )
}

fn makefile_include_response_path(
    ctx: &GenCtx<'_>,
    target: &Target,
    language: &str,
    includes: &str,
) -> Option<(String, String)> {
    if includes.is_empty()
        || !ctx
            .var(
                target.makefile,
                &format!("CMAKE_{language}_USE_RESPONSE_FILE_FOR_INCLUDES"),
            )
            .is_some_and(|value| is_on(&value))
    {
        return None;
    }
    let full = format!(
        "{}/includes_{language}.rsp",
        gt::support_directory(ctx, target)
    );
    let rel_cur = maybe_relative_cur(ctx, target.makefile, &full);
    Some((full, rel_cur))
}

fn makefile_object_entries(
    ctx: &GenCtx<'_>,
    target: &Target,
    objects: &[String],
    externals: &[String],
) -> Vec<String> {
    let pch_extension = ctx.var(target.makefile, "CMAKE_PCH_EXTENSION");
    let dir = target.makefile;
    objects
        .iter()
        .filter(|object| {
            pch_extension
                .as_ref()
                .is_none_or(|extension| !object.ends_with(extension.as_str()))
        })
        .map(|object| response_path(ctx, object))
        .chain(
            externals
                .iter()
                .map(|object| response_path(ctx, &maybe_relative_cur(ctx, dir, object))),
        )
        .chain(
            super::ispc::generated_objects(ctx, target, &ctx.config)
                .iter()
                .map(|object| response_path(ctx, &maybe_relative_cur(ctx, dir, object))),
        )
        .collect()
}

fn makefile_object_strings(entries: &[String], limit: usize) -> Vec<String> {
    let mut strings = Vec::new();
    let mut current = String::new();
    for entry in entries {
        if limit != usize::MAX && current.len() + 1 + entry.len() > limit {
            strings.push(std::mem::take(&mut current));
        }
        if !current.is_empty() {
            current.push(' ');
        }
        current.push_str(entry);
    }
    strings.push(current);
    strings
}

fn makefile_use_response_objects(
    ctx: &GenCtx<'_>,
    target: &Target,
    language: &str,
    entries: &[String],
) -> bool {
    let name = format!("CMAKE_{language}_USE_RESPONSE_FILE_FOR_OBJECTS");
    if let Some(value) = ctx.var(target.makefile, &name)
        && !value.is_empty()
    {
        return is_on(&value);
    }
    let length: usize = entries.iter().map(|entry| entry.len() + 3).sum();
    length as i64 > super::ninja::command_line_length_limit() / 2
}

fn makefile_use_archive_rules(ctx: &GenCtx<'_>, target: &Target, language: &str) -> bool {
    if target.kind != TargetKind::StaticLibrary {
        return false;
    }
    let create_var = format!("CMAKE_{language}_CREATE_STATIC_LIBRARY");
    let have_static_rule = ctx
        .var(target.makefile, &create_var)
        .is_some_and(|value| !value.is_empty());
    !have_static_rule
        && !ctx
            .var_safe(target.makefile, &format!("CMAKE_{language}_ARCHIVE_CREATE"))
            .is_empty()
        && !ctx
            .var_safe(target.makefile, &format!("CMAKE_{language}_ARCHIVE_APPEND"))
            .is_empty()
}

fn makefile_response_plan(
    ctx: &GenCtx<'_>,
    target: &Target,
    objects: &[String],
    externals: &[String],
    language: &str,
) -> (bool, Vec<String>) {
    let entries = makefile_object_entries(ctx, target, objects, externals);
    let use_response = makefile_use_response_objects(ctx, target, language, &entries);
    let strings = if use_response {
        makefile_object_strings(&entries, 131000)
    } else {
        vec![entries.join(" ")]
    };
    (use_response, strings)
}

fn makefile_path(path: &str) -> String {
    let mut result = String::with_capacity(path.len());
    for ch in path.chars() {
        match ch {
            '=' => result.push_str("$(EQUALS)"),
            '$' => result.push_str("$$"),
            '\\' | ' ' | '#' => {
                result.push('\\');
                result.push(ch);
            }
            _ => result.push(ch),
        }
    }
    result
}

/// kwsys `SystemTools::ConvertToUnixOutputPath` for Makefile comments and
/// include paths: only unescaped spaces are escaped on Unix.
fn unix_output_path(path: &str) -> String {
    let mut result = String::with_capacity(path.len());
    let mut escaped = false;
    for ch in path.chars() {
        if ch == ' ' && !escaped {
            result.push('\\');
        }
        result.push(ch);
        escaped = ch == '\\' && !escaped;
        if ch != '\\' {
            escaped = false;
        }
    }
    result
}

fn write_make_rule(
    out: &mut String,
    comment: Option<&str>,
    target: &str,
    depends: &[String],
    commands: &[String],
    symbolic: bool,
) {
    if let Some(comment) = comment {
        for line in comment.lines() {
            let _ = writeln!(out, "# {line}");
        }
    }
    let target = makefile_path(target);
    let space = if target.len() == 1 { " " } else { "" };
    if depends.is_empty() {
        let _ = writeln!(out, "{target}{space}:");
    } else {
        for depend in depends {
            let _ = writeln!(out, "{target}{space}: {}", makefile_path(depend));
        }
    }
    for command in commands {
        if !command.is_empty() {
            let _ = writeln!(out, "	{command}");
        }
    }
    if symbolic {
        let _ = writeln!(out, ".PHONY : {target}");
    }
    out.push('\n');
}

fn build_file_header(_name: &str) -> String {
    format!(
        "# CMAKE generated file: DO NOT EDIT!\n# Generated by \"Unix Makefiles\" Generator, CMake Version 4.1\n\n# Delete rule output on recipe failure.\n.DELETE_ON_ERROR:\n\n#=============================================================================\n# Special targets provided by cmake.\n\n# Disable implicit rules so canonical targets will work.\n.SUFFIXES:\n\n# Disable VCS-based implicit rules.\n% : %,v\n\n# Disable VCS-based implicit rules.\n% : RCS/%\n\n# Disable VCS-based implicit rules.\n% : RCS/%,v\n\n# Disable VCS-based implicit rules.\n% : SCCS/s.%\n\n# Disable VCS-based implicit rules.\n% : s.%\n\n.SUFFIXES: .hpux_make_needs_suffix_list\n\n# Command-line flag to silence nested $(MAKE).\n$(VERBOSE)MAKESILENT = -s\n\n#Suppress display of executed commands.\n$(VERBOSE).SILENT:\n\n# A target that is always out of date.\ncmake_force:\n.PHONY : cmake_force\n\n#=============================================================================\n# Set environment variables for the build.\n\n# The shell in which to execute make rules.\nSHELL = /bin/sh\n\n# The CMake executable.\nCMAKE_COMMAND = {name}\n\n# The command to remove a file.\nRM = {name} -E rm -f\n\n# Escaping for special characters.\nEQUALS = =\n\n# The top-level source directory on which CMake was run.\nCMAKE_SOURCE_DIR = @CMAKE_SOURCE_DIR@\n\n# The top-level build directory on which CMake was run.\nCMAKE_BINARY_DIR = @CMAKE_BINARY_DIR@\n\n",
        name = "@CMAKE_COMMAND@"
    )
}

#[derive(Default)]
struct ImplicitDepends {
    cmake: BTreeMap<String, BTreeMap<String, Vec<String>>>,
    compiler: BTreeMap<String, BTreeMap<String, Vec<String>>>,
    output_pairs: Vec<(String, String)>,
}

impl ImplicitDepends {
    fn add(&mut self, lang: &str, obj: &str, src: &str, compiler: bool) {
        let map = if compiler {
            &mut self.compiler
        } else {
            &mut self.cmake
        };
        map.entry(lang.to_owned())
            .or_default()
            .entry(obj.to_owned())
            .or_default()
            .push(src.to_owned());
    }
    #[allow(dead_code)]
    fn enabled(&self) -> bool {
        !self.cmake.is_empty() || !self.compiler.is_empty()
    }
}

#[derive(Default)]
struct TargetProgress {
    total: usize,
    var_by_file: HashMap<String, usize>,
    all: Vec<usize>,
    rules: Vec<usize>,
    marks: Vec<usize>,
}

pub struct MakefileGenerator<'a> {
    ctx: &'a GenCtx<'a>,
    progress: Vec<(String, TargetProgress)>,
    implicit: HashMap<String, ImplicitDepends>,
    compile_commands: Vec<String>,
    commands_visited: HashMap<(usize, String), HashSet<(usize, usize)>>,
    tidy_fixes_dirs: BTreeSet<String>,
    tidy_fixes_files: BTreeSet<String>,
    rule_hashes: BTreeMap<String, String>,
}

impl<'a> MakefileGenerator<'a> {
    pub fn new(ctx: &'a GenCtx<'a>) -> Self {
        Self {
            ctx,
            progress: Vec::new(),
            implicit: HashMap::new(),
            compile_commands: Vec::new(),
            commands_visited: HashMap::new(),
            tidy_fixes_dirs: BTreeSet::new(),
            tidy_fixes_files: BTreeSet::new(),
            rule_hashes: BTreeMap::new(),
        }
    }

    fn color(&self) -> bool {
        if self.ctx.interp.in_try_compile {
            return false;
        }
        match self.ctx.var(0, "CMAKE_COLOR_MAKEFILE") {
            Some(value) => crate::util::is_on(&value),
            None => self
                .ctx
                .var(0, "CMAKE_COLOR_DIAGNOSTICS")
                .is_some_and(|value| crate::util::is_on(&value)),
        }
    }

    /// `cmGlobalGenerator::GlobalSettingIsOn("CMAKE_SUPPRESS_REGENERATION")`,
    /// inverted as the Makefile generators use it.
    fn regenerate(&self) -> bool {
        !self
            .ctx
            .var(0, "CMAKE_SUPPRESS_REGENERATION")
            .is_some_and(|value| is_on(&value))
    }

    /// `cmGlobalGenerator::Generate`'s local-generator loop as driven by
    /// `cmGlobalUnixMakefileGenerator3::Generate`: `after_local` runs after
    /// each local generator (its install rules and test files), and
    /// `after_locals` once the loop is done (the build export sets), before
    /// the Makefile generator's own trailing files.
    pub fn generate_with(
        &mut self,
        after_local: &mut dyn FnMut(usize),
        after_locals: &mut dyn FnMut(),
    ) {
        super::check_target_properties(self.ctx);
        for dir in 0..self.ctx.interp.makefiles.len() {
            self.generate_directory(dir);
            after_local(dir);
        }
        after_locals();
        self.check_rule_hashes();
        if !self.compile_commands.is_empty() {
            let content = format!("[\n{}\n]", self.compile_commands.join(",\n"));
            let path = format!("{}/compile_commands.json", self.ctx.interp.home_binary);
            let _ = crate::fsutil::write_generated(Path::new(&path), &content);
        }
        for dir in &self.tidy_fixes_dirs {
            remove_unknown_tidy_fixes(Path::new(dir), &self.tidy_fixes_files);
        }
        self.write_progress_marks();
        self.write_main_makefile2();
        self.write_target_directories();
        self.write_main_cmakefile();
        self.write_top_makefile();
    }

    fn visit_target_order(
        &self,
        key: &str,
        wanted: &HashSet<String>,
        visiting: &mut HashSet<String>,
        emitted: &mut HashSet<String>,
        ordered: &mut Vec<String>,
    ) {
        if emitted.contains(key) || !visiting.insert(key.to_owned()) {
            return;
        }
        if let Some(target) = self.ctx.interp.ext.targets.get(key) {
            for depend in super::link::target_depends(self.ctx, target).iter() {
                self.visit_target_order(&depend.key, wanted, visiting, emitted, ordered);
            }
        }
        visiting.remove(key);
        emitted.insert(key.to_owned());
        if wanted.contains(key) {
            ordered.push(key.to_owned());
        }
    }

    /// `cmGlobalGenerator::GetLocalGeneratorTargetsInOrder`: target creation
    /// order, rearranged so direct dependencies are generated first.
    fn dependency_ordered_keys(&self, keys: &[String]) -> Vec<String> {
        let wanted: HashSet<String> = keys.iter().cloned().collect();
        let mut visiting = HashSet::new();
        let mut emitted = HashSet::new();
        let mut ordered = Vec::new();
        for key in keys {
            self.visit_target_order(key, &wanted, &mut visiting, &mut emitted, &mut ordered);
        }
        ordered
    }

    fn seed_commands_visited(&mut self, dir: usize, key: &str, target: &Target) {
        let mut seed = HashSet::new();
        for depend in super::link::target_depends(self.ctx, target).iter() {
            if let Some(dep) = self.ctx.interp.ext.targets.get(&depend.key)
                && dep.makefile == dir
                && let Some(visited) = self.commands_visited.get(&(dir, depend.key.clone()))
            {
                seed.extend(visited.iter().copied());
            }
        }
        self.commands_visited
            .entry((dir, key.to_owned()))
            .or_default()
            .extend(seed);
    }

    fn generate_directory(&mut self, dir: usize) {
        self.write_directory_information(dir);
        let keys = self.ctx.interp.makefiles[dir].targets.clone();
        for key in self.dependency_ordered_keys(&keys) {
            let Some(target) = self.ctx.interp.ext.targets.get(&key) else {
                continue;
            };
            if target.makefile != dir || !is_in_build_system(target) {
                continue;
            }
            self.target_mut_progress(target);
            self.seed_commands_visited(dir, &key, target);
            match target.kind {
                TargetKind::Executable
                | TargetKind::StaticLibrary
                | TargetKind::SharedLibrary
                | TargetKind::ModuleLibrary
                | TargetKind::ObjectLibrary => self.write_target(target, &key),
                TargetKind::InterfaceLibrary | TargetKind::Utility => {
                    self.write_utility_target(target, &key)
                }
                _ => {}
            }
        }
        self.write_local_makefile(dir);
    }

    fn write_directory_information(&self, dir: usize) {
        let _rel = if dir == 0 {
            String::new()
        } else {
            home_relative_output_path(self.ctx, dir)
                .trim_end_matches('/')
                .to_owned()
        };
        let mut out = String::from(
            "# CMAKE generated file: DO NOT EDIT!\n# Generated by \"Unix Makefiles\" Generator, CMake Version 4.1\n\n",
        );
        let (top_source, top_binary) = custom::relative_path_tops(self.ctx, dir);
        let _ = writeln!(
            out,
            "# Relative path conversion top directories.\nset(CMAKE_RELATIVE_PATH_TOP_SOURCE \"{top_source}\")\nset(CMAKE_RELATIVE_PATH_TOP_BINARY \"{top_binary}\")",
        );
        out.push_str("\n# Force unix paths in dependencies.\nset(CMAKE_FORCE_UNIX_PATHS 1)\n\n");
        let scan = self.ctx.interp.makefiles[dir]
            .props
            .get("INCLUDE_REGULAR_EXPRESSION")
            .unwrap_or("^.*$");
        let complain = self.ctx.interp.makefiles[dir]
            .props
            .get("COMPLAIN_REGULAR_EXPRESSION")
            .unwrap_or("^$");
        out.push_str(&format!(
            "\n# The C and CXX include file regular expressions for this directory.\nset(CMAKE_C_INCLUDE_REGEX_SCAN \"{}\")\nset(CMAKE_C_INCLUDE_REGEX_COMPLAIN \"{}\")\nset(CMAKE_CXX_INCLUDE_REGEX_SCAN ${{CMAKE_C_INCLUDE_REGEX_SCAN}})\nset(CMAKE_CXX_INCLUDE_REGEX_COMPLAIN ${{CMAKE_C_INCLUDE_REGEX_COMPLAIN}})\n",
            escape_cmake(scan),
            escape_cmake(complain)
        ));
        let path = dir_support(self.ctx, dir);
        let _ = crate::fsutil::write_if_different(Path::new(&path), &out);
    }

    fn target_mut_progress(&mut self, target: &Target) -> &mut TargetProgress {
        let name = target.name.clone();
        if !self.progress.iter().any(|(n, _)| n == &name) {
            self.progress
                .push((name.clone(), TargetProgress::default()));
        }
        let pos = self.progress.iter().position(|(n, _)| n == &name).unwrap();
        &mut self.progress[pos].1
    }

    fn add_progress_file(&mut self, target: &Target, file: String) -> usize {
        let p = self.target_mut_progress(target);
        if let Some(number) = p.var_by_file.get(&file) {
            return *number;
        }
        p.total += 1;
        p.var_by_file.insert(file.clone(), p.total);
        if !p.rules.contains(&p.total) {
            p.all.push(p.total);
        }
        p.total
    }

    fn echo(&self, text: &str, color: &str, progress: Option<usize>) -> String {
        let color = if self.color() { color } else { "" };
        let escaped = crate::outconv::escape_for_shell(
            text,
            crate::outconv::FLAG_MAKE | crate::outconv::FLAG_IS_UNIX,
        );
        if color.is_empty() && progress.is_none() {
            return format!("echo {escaped}");
        }
        let mut command =
            String::from("$(CMAKE_COMMAND) -E cmake_echo_color \"--switch=$(COLOR)\"");
        if !color.is_empty() {
            command.push(' ');
            command.push_str(color);
        }
        if let Some(number) = progress {
            let _ = write!(
                command,
                " --progress-dir={} --progress-num=$(CMAKE_PROGRESS_{number})",
                shell_path(
                    self.ctx,
                    &format!("{}/CMakeFiles", self.ctx.interp.home_binary)
                )
            );
        }
        let _ = write!(command, " {escaped}");
        command
    }

    fn write_target(&mut self, target: &Target, target_key: &str) {
        let config = self.ctx.config.clone();
        let dir = target.makefile;
        let _cur_bin = self.ctx.interp.makefiles[dir].binary_dir.clone();
        let rel_home = home_relative_output_path(self.ctx, dir);
        let support = gt::support_directory(self.ctx, target);
        let support_rel_cur = maybe_relative_cur(self.ctx, dir, &support);
        let support_rel_top = format!("{rel_home}{support_rel_cur}");
        let mut impl_depends = ImplicitDepends::default();
        let sources = gt::sources(self.ctx, target, &config);
        self.check_cxx_module_status(target, &sources);
        let mut build = build_file_header("");
        apply_verbose_default(self.ctx, &mut build);
        build = build
            .replace(
                "@CMAKE_COMMAND@",
                &shell_path(self.ctx, &self.ctx.interp.cmake_command),
            )
            .replace(
                "@CMAKE_SOURCE_DIR@",
                &shell_path(self.ctx, &self.ctx.interp.home_source),
            )
            .replace(
                "@CMAKE_BINARY_DIR@",
                &shell_path(self.ctx, &self.ctx.interp.home_binary),
            );
        apply_instrumentation_variable(self.ctx, &mut build);
        // Dependency includes.
        // `cmMakefileTargetGenerator::WriteTargetBuildRules`: the
        // compiler_depend.make include and placeholder files depend on the
        // project-wide toggle, not on any target language.
        let depend_toggle = self
            .ctx
            .var(target.makefile, "CMAKE_DEPENDS_USE_COMPILER")
            .is_none_or(|v| is_on(&v));
        let language = gt::linker_language(self.ctx, target, &self.ctx.config);
        // cmComputeLinkDepends::EntriesProcessing: the first link-information
        // computation for a target whose policy is WARN emits the CMP0156
        // developer warning, in target generation order.
        if target
            .policies
            .get(crate::policy::index_of("CMP0156").unwrap())
            == Some(crate::policy::PolicyStatus::Warn)
            && !self.ctx.interp.in_try_compile
            && self.ctx.interp.policy_optional_warning(
                self.ctx
                    .var(target.makefile, "CMAKE_POLICY_WARNING_CMP0156")
                    .as_deref(),
            )
        {
            self.ctx.warning(
                target.backtrace.clone(),
                format!(
                    "{}
Since the policy is not set, legacy libraries de-duplication strategy will be applied.",
                    crate::policy::policy_warning(crate::policy::index_of("CMP0156").unwrap())
                ),
            );
        }
        let mut compiler_depends =
            depend_toggle || flags::link_dependency_file(self.ctx, target, &language).is_some();
        let _ = &mut compiler_depends;
        compiler_depends |= self.compiler_depends_enabled(target);
        for source in sources.iter() {
            if source.kind != SourceKind::ObjectSource {
                continue;
            }
            let use_compiler = self.use_compiler_depends(target, &source.language);
            compiler_depends |= use_compiler;
            let object = format!("{}/{}", support, source.object_name);
            impl_depends.add(&source.language, &object, &source.path, use_compiler);
        }
        if compiler_depends {
            let _ = writeln!(
                build,
                "# Include any dependencies generated for this target.\ninclude {}\n# Include any dependencies generated by the compiler for this target.\ninclude {}\n",
                makefile_path(&format!("{support_rel_top}/depend.make")),
                makefile_path(&format!("{support_rel_top}/compiler_depend.make"))
            );
        } else {
            let _ = writeln!(
                build,
                "# Include any dependencies generated for this target.\ninclude {}\n",
                makefile_path(&format!("{support_rel_top}/depend.make"))
            );
        }
        let _ = writeln!(
            build,
            "# Include the progress variables for this target.\ninclude {}\n\n# Include the compile flags for this target's objects.\ninclude {}\n",
            makefile_path(&format!("{support_rel_top}/progress.make")),
            makefile_path(&format!("{support_rel_top}/flags.make"))
        );

        // cmCommonTargetGenerator::HaveRequiredLanguages: object rules are
        // written only for languages enabled in the top-level project.
        for language in sources
            .iter()
            .filter(|source| source.kind == SourceKind::ObjectSource)
            .map(|source| source.language.clone())
            .collect::<BTreeSet<_>>()
        {
            if !self
                .ctx
                .interp
                .langs
                .enabled
                .iter()
                .any(|enabled| enabled == &language)
            {
                self.ctx.error(
                    gt::directory_backtrace(self.ctx, target.makefile),
                    format!(
                        "The language {language} was requested for compilation but was not enabled.  To enable a language it needs to be specified in a 'project' or 'enable_language' command in the root CMakeLists.txt"
                    ),
                );
            }
        }

        // Custom commands and objects.
        let mut object_paths = Vec::new();
        let external_paths: Vec<String> = sources
            .iter()
            .filter(|s| s.kind == SourceKind::ExternalObject)
            .map(|s| s.path.clone())
            .collect();
        let mut custom_outputs = Vec::new();
        let mut object_extra_clean = Vec::new();
        let (codegen_outputs, custom_clean) = self.write_custom_commands(
            target,
            target_key,
            dir,
            &support_rel_cur,
            &support_rel_top,
            &mut build,
            &mut custom_outputs,
            &mut impl_depends,
        );
        let codegen = format!("{support_rel_top}/codegen");
        let codegen_depends: Vec<String> = codegen_outputs
            .iter()
            .map(|output| self.top_rule_path(output))
            .collect();
        write_make_rule(&mut build, None, &codegen, &codegen_depends, &[], true);
        let mut mac_content_folders = BTreeSet::new();
        let (osx_extra_files, osx_clean) = self.write_mac_osx_content_statements(
            target,
            &sources,
            &mut mac_content_folders,
            &mut build,
        );
        for source in sources
            .iter()
            .filter(|s| s.kind == SourceKind::ObjectSource)
        {
            let object_dir = support.clone();
            let obj_rel_cur = format!(
                "{}/{}",
                maybe_relative_cur(self.ctx, dir, &object_dir),
                source.object_name
            );
            let obj_rel_top = format!("{rel_home}{obj_rel_cur}");
            // cmMakefileTargetGenerator creates the object file directory at
            // generate time so compiler-generated depfiles can be written.
            let obj_abs = format!(
                "{}/{obj_rel_cur}",
                self.ctx.interp.makefiles[dir].binary_dir
            );
            if let Some(parent) = Path::new(&obj_abs).parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            object_paths.push(obj_rel_cur.clone());
            let progress = self.add_progress_file(target, obj_rel_top.clone());
            self.write_object_rule(
                target,
                source,
                dir,
                &obj_rel_cur,
                &obj_rel_top,
                progress,
                compiler_depends,
                &mut impl_depends,
                &mut object_extra_clean,
                &mut build,
            );
        }

        if target.kind == TargetKind::ObjectLibrary {
            self.write_object_library_rule(target, &object_paths, &support_rel_top, &mut build);
        } else {
            self.write_link(
                target,
                &object_paths,
                &external_paths,
                &osx_extra_files,
                &mut mac_content_folders,
                &support_rel_cur,
                &support_rel_top,
                compiler_depends,
                &mut build,
            );
        }
        self.write_clean_and_depend(
            target,
            &support,
            &support_rel_cur,
            &support_rel_top,
            &custom_outputs,
            &mut build,
        );
        let path = format!("{support}/build.make");
        let _ = crate::fsutil::write_if_different(Path::new(&path), &build);
        self.write_flags(target);
        self.write_depend_info(target, &impl_depends);
        self.write_dependency_placeholders(&support, compiler_depends, &target.name);
        let mut extra_clean = osx_clean;
        extra_clean.extend(object_extra_clean);
        extra_clean.extend(
            super::ispc::generated_objects(self.ctx, target, &self.ctx.config)
                .iter()
                .map(|object| maybe_relative_cur(self.ctx, dir, object)),
        );
        self.write_clean_files(target, &object_paths, &custom_clean, &extra_clean);
        self.implicit.insert(target.name.clone(), impl_depends);
    }

    /// Unix Makefiles have no C++ module scanning support; upstream rejects
    /// scannable module targets before writing their rules.
    fn check_cxx_module_status(&self, target: &Target, sources: &[gt::SourceInfo]) {
        let modules = ModuleSupport::new(self.ctx, target, &self.ctx.config);
        let scannable = gt::have_cxx20_module_sources(target)
            || sources
                .iter()
                .any(|source| source.language == "CXX" && modules.need_dyndep_for_source(source));
        if scannable {
            self.ctx.error(
                self.ctx.dir_backtrace(target.makefile),
                format!(
                    "The target named \"{}\" has C++ sources that may use modules, but modules are not supported by this generator:\n  Unix Makefiles\nModules are supported only by Ninja, Ninja Multi-Config, and Visual Studio generators for VS 17.4 and newer.  See the cmake-cxxmodules(7) manual for details.  Use the CMAKE_CXX_SCAN_FOR_MODULES variable to enable or disable scanning.",
                    target.name
                ),
            );
        }
    }

    fn compiler_depends_enabled(&self, target: &Target) -> bool {
        gt::sources(self.ctx, target, &self.ctx.config)
            .iter()
            .any(|source| self.use_compiler_depends(target, &source.language))
            || flags::link_dependency_file(
                self.ctx,
                target,
                &gt::linker_language(self.ctx, target, &self.ctx.config),
            )
            .is_some()
    }

    fn use_compiler_depends(&self, target: &Target, lang: &str) -> bool {
        // `cmMakefileTargetGenerator::WriteObjectRuleFiles`: compiler
        // dependency generation is controlled by the per-language support
        // variable the compiler modules set.
        self.ctx
            .var(
                target.makefile,
                &format!("CMAKE_{lang}_DEPENDS_USE_COMPILER"),
            )
            .is_some_and(|v| is_on(&v))
    }

    fn depfile_for_source(
        &self,
        target: &Target,
        source: &gt::SourceInfo,
        object_rel_cur: &str,
    ) -> Option<(String, String)> {
        let lang = &source.language;
        let dep_flags = self
            .ctx
            .var_safe(target.makefile, &format!("CMAKE_DEPFILE_FLAGS_{lang}"));
        if dep_flags.is_empty() {
            return None;
        }
        let dep_path = if self
            .ctx
            .var(
                target.makefile,
                &format!("CMAKE_{lang}_DEPFILE_EXTENSION_REPLACE"),
            )
            .is_some_and(|v| is_on(&v))
        {
            let name = source
                .object_name
                .rsplit('/')
                .next()
                .unwrap_or(&source.object_name);
            let stem = name.rsplit_once('.').map_or(name, |(stem, _)| stem);
            format!(
                "{}/{stem}.d",
                object_rel_cur.rsplit_once('/').map_or("", |(d, _)| d)
            )
        } else {
            format!("{object_rel_cur}.d")
        };
        Some((dep_path, dep_flags))
    }

    /// `cmCommonTargetGenerator::GenerateCodeCheckRules` for Unix Makefiles.
    fn code_check(&mut self, target: &Target, source: &gt::SourceInfo) -> String {
        let lang = source.language.as_str();
        let mut options = Vec::new();
        let mut needs_source = false;
        for (property, option) in [
            ("INCLUDE_WHAT_YOU_USE", "iwyu"),
            ("CLANG_TIDY", "tidy"),
            ("CPPLINT", "cpplint"),
            ("CPPCHECK", "cppcheck"),
            ("ICSTAT", "icstat"),
        ] {
            if property != "CLANG_TIDY" && !matches!(lang, "C" | "CXX") {
                continue;
            }
            let Some(raw) = target.props.get(&format!("{lang}_{property}")) else {
                continue;
            };
            let mut command = self.ctx.genex(raw, Some(target), Some(lang));
            if command.is_empty() {
                continue;
            }
            let driver_mode = |tool: &str| {
                self.ctx
                    .var(target.makefile, &format!("CMAKE_{lang}_{tool}_DRIVER_MODE"))
                    .filter(|mode| !mode.is_empty())
                    .unwrap_or_else(|| if lang == "C" { "gcc" } else { "g++" }.into())
            };
            match option {
                "iwyu" if !command.contains("--driver-mode=") => {
                    let _ = write!(command, ";--driver-mode={}", driver_mode(property));
                }
                "tidy" => {
                    let mut export_fixes = String::new();
                    if let Some(dir_value) = target
                        .props
                        .get(&format!("{lang}_CLANG_TIDY_EXPORT_FIXES_DIR"))
                        .filter(|dir| !dir.is_empty())
                    {
                        let binary = &self.ctx.interp.makefiles[target.makefile].binary_dir;
                        let dir = if Path::new(dir_value).is_absolute() {
                            util::collapse_full_path(dir_value, "")
                        } else {
                            util::collapse_full_path(&format!("{binary}/{dir_value}"), "")
                        };
                        let support = gt::support_directory(self.ctx, target);
                        let support_rel = maybe_relative_top(self.ctx, &support);
                        let safe = safe_object_file_name(&support_rel);
                        let fixes = util::collapse_full_path(
                            &format!("{dir}/{safe}/{}.yaml", source.object_name),
                            "",
                        );
                        let _ = std::fs::create_dir_all(util::filename_path(&fixes));
                        self.tidy_fixes_dirs.insert(dir);
                        self.tidy_fixes_files.insert(fixes.clone());
                        let fixes_rel = maybe_relative_cur(self.ctx, target.makefile, &fixes);
                        let _ = write!(export_fixes, ";--export-fixes={fixes_rel}");
                    }
                    let _ = write!(
                        command,
                        ";--extra-arg-before=--driver-mode={}{export_fixes}",
                        driver_mode(property)
                    );
                }
                "icstat" => {
                    if !command.contains("--checks=") {
                        command.push_str(";--checks=cstat_sel_checks.txt");
                    }
                    if !command.contains("--db=") {
                        command.push_str(";--db=cstat.db");
                    }
                }
                _ => {}
            }
            needs_source |= matches!(option, "tidy" | "cpplint" | "cppcheck");
            options.push(format!(" --{option}={}", shell_path(self.ctx, &command)));
        }
        if options.is_empty() {
            return String::new();
        }
        let mut command = String::from("$(CMAKE_COMMAND) -E __run_co_compile");
        let launcher = flags::compiler_launcher_list(self.ctx, target, lang);
        if !launcher.is_empty() {
            let _ = write!(command, " --launcher={}", shell_path(self.ctx, &launcher));
        }
        command.push_str(&options.join(""));
        if needs_source {
            let _ = write!(command, " --source={}", shell_path(self.ctx, &source.path));
        }
        command.push_str(" -- ");
        command
    }

    #[allow(clippy::too_many_arguments)]
    fn write_object_rule(
        &mut self,
        target: &Target,
        source: &gt::SourceInfo,
        dir: usize,
        obj_rel_cur: &str,
        obj_rel_top: &str,
        progress: usize,
        compiler_depends: bool,
        impl_depends: &mut ImplicitDepends,
        clean_files: &mut Vec<String>,
        out: &mut String,
    ) {
        let mut depends = vec![format!("{}/flags.make", self.support_rel(target, dir))];
        let target_includes =
            flags::compile_target_vars(self.ctx, target, &self.ctx.config, &source.language)
                .includes;
        if let Some((_, include_rsp)) =
            makefile_include_response_path(self.ctx, target, &source.language, &target_includes)
        {
            depends.push(format!(
                "{}{}",
                home_relative_output_path(self.ctx, dir),
                include_rsp
            ));
        }
        // cmMakefileTargetGenerator::WriteObjectDependRules: the source,
        // then its OBJECT_DEPENDS; PCH and ISPC headers follow.
        depends.push(self.top_rule_path(&source.path));
        if let Some(object_deps) = self.source_prop(target, &source.path, "OBJECT_DEPENDS") {
            depends.extend(
                util::expand_list(&object_deps, false)
                    .iter()
                    .map(|dep| self.top_rule_path(dep)),
            );
        }
        let ispc_header = if source.language == "ISPC" {
            ispc_header_for_source(self.ctx, target, &self.ctx.config, &source.object_name)
        } else {
            String::new()
        };
        let source_compiler_depends = self.use_compiler_depends(target, &source.language);
        let properties = self.source_props(target, &source.path);
        let pch_archs = super::pch::pch_archs(self.ctx, target, &self.ctx.config, &source.language);
        let mut pch_sources = std::collections::HashMap::new();
        for arch in &pch_archs {
            let pch_source =
                super::pch::pch_source(self.ctx, target, &self.ctx.config, &source.language, arch);
            if !pch_source.is_empty() {
                pch_sources.insert(pch_source, arch.clone());
            }
        }
        if !pch_sources.is_empty() && properties.get("SKIP_PRECOMPILE_HEADERS").is_none() {
            let object_full = format!(
                "{}/{}",
                gt::support_directory(self.ctx, target),
                source.object_name
            );
            for arch in &pch_archs {
                let header = super::pch::pch_header(
                    self.ctx,
                    target,
                    &self.ctx.config,
                    &source.language,
                    arch,
                );
                if !header.is_empty() {
                    depends.push(self.top_rule_path(&header));
                    impl_depends.add(
                        &source.language,
                        &object_full,
                        &header,
                        source_compiler_depends,
                    );
                }
                if !pch_sources.contains_key(&source.path) {
                    let file = super::pch::pch_file(
                        self.ctx,
                        target,
                        &self.ctx.config,
                        &source.language,
                        arch,
                    );
                    if !file.is_empty() {
                        depends.push(self.top_rule_path(&file));
                    }
                }
            }
        }
        if source.language != "ISPC" {
            for header in super::ispc::generated_headers(self.ctx, target, &self.ctx.config) {
                depends.push(self.top_rule_path(&header));
            }
        }
        let depinfo = if source_compiler_depends {
            self.depfile_for_source(target, source, obj_rel_cur)
        } else {
            None
        };
        let _ = compiler_depends;
        if depinfo.is_some() {
            depends.push(format!(
                "{}/compiler_depend.ts",
                self.support_rel(target, dir)
            ));
        }
        let dep_flags_expanded = depinfo.as_ref().map(|(dep_path, dep_flags)| {
            let dep_target = crate::outconv::escape_for_shell(
                &makefile_path(obj_rel_top),
                crate::outconv::FLAG_MAKE | crate::outconv::FLAG_IS_UNIX,
            );
            let dep_vars = vec![
                ("DEP_FILE", shell_path(self.ctx, dep_path)),
                ("DEP_TARGET", dep_target),
            ];
            flags::expand_rule_for_step(
                self.ctx,
                target,
                &source.language,
                flags::RuleStep::Compile,
                dep_flags,
                &dep_vars,
            )
        });
        let lang = &source.language;
        let mut commands = Vec::new();
        let echo_text = format!("Building {lang} object {obj_rel_top}");
        commands.push(format!(
            "@{}",
            self.echo(&echo_text, "--green", Some(progress))
        ));
        let template = self
            .ctx
            .var(target.makefile, &format!("CMAKE_{lang}_COMPILE_OBJECT"))
            .unwrap_or_default();
        let source_shell = shell_path(self.ctx, &source.path);
        let object_shell = shell_path(self.ctx, obj_rel_cur);
        let compile_pdb = gt::target_compile_pdb(self.ctx, target, &self.ctx.config);
        let target_pdb = if matches!(
            target.kind,
            TargetKind::Executable
                | TargetKind::StaticLibrary
                | TargetKind::SharedLibrary
                | TargetKind::ModuleLibrary
        ) {
            let names = gt::names(self.ctx, target, &self.ctx.config);
            format!(
                "{}/{}",
                gt::pdb_directory(self.ctx, target, &self.ctx.config),
                names.pdb
            )
        } else {
            String::new()
        };
        for pdb in [&compile_pdb, &target_pdb] {
            if !pdb.is_empty()
                && let Some(parent) = Path::new(pdb).parent()
            {
                let _ = std::fs::create_dir_all(parent);
            }
        }
        let compile_pdb_shell = shell_path_keep_trailing_slash(
            self.ctx,
            &maybe_relative_cur(self.ctx, dir, &compile_pdb),
        );
        let target_pdb_shell = shell_path(self.ctx, &target_pdb);
        let defines_var = self.object_defines_var(target, source, &properties);
        let includes_var = self.object_includes_var(target, source, &properties);
        let mut flags_var = self.object_flags_var(target, source, &properties);
        if let Some(expanded) = &dep_flags_expanded {
            flags_var.push(' ');
            flags_var.push_str(expanded);
        }
        let vars = vec![
            ("DEFINES", defines_var),
            ("INCLUDES", includes_var),
            ("FLAGS", flags_var),
            ("OBJECT", object_shell.clone()),
            ("SOURCE", source_shell),
            ("TARGET_COMPILE_PDB", compile_pdb_shell),
            ("TARGET_PDB", target_pdb_shell),
            (
                "CUDA_COMPILE_MODE",
                if lang == "CUDA" {
                    makefile_cuda_compile_mode(self.ctx, target)
                } else {
                    String::new()
                },
            ),
            ("TARGET_NAME", target.name.clone()),
            ("TARGET_TYPE", target.kind.type_name().to_owned()),
            (
                "TARGET_LABELS",
                target
                    .props
                    .get("LABELS")
                    .map_or_else(String::new, |v| v.replace(';', ",")),
            ),
            (
                "ISPC_HEADER",
                shell_path_preserve_slashes(self.ctx, &ispc_header),
            ),
            ("LANGUAGE", lang.to_owned()),
            ("CONFIG", self.ctx.config.clone()),
            (
                "CMAKE_CURRENT_BINARY_DIR",
                shell_path(self.ctx, &self.ctx.interp.makefiles[dir].binary_dir),
            ),
        ];
        let mut command = flags::expand_rule_for_step(
            self.ctx,
            target,
            lang,
            flags::RuleStep::Compile,
            &template,
            &vars,
        );
        if depinfo.is_some()
            && self
                .ctx
                .var_safe(target.makefile, &format!("CMAKE_{lang}_DEPFILE_FORMAT"))
                == "msvc"
            && let Some((dep_path, _)) = &depinfo
        {
            let prefix = self.ctx.var_safe(
                target.makefile,
                &format!("CMAKE_{lang}_CL_SHOWINCLUDES_PREFIX"),
            );
            command = format!(
                "$(CMAKE_COMMAND) -E cmake_cl_compile_depends --dep-file={} --working-dir={} --filter-prefix={} -- {command}",
                shell_path(self.ctx, dep_path),
                shell_path(self.ctx, &self.ctx.interp.makefiles[dir].binary_dir),
                shell_path(self.ctx, &prefix),
            );
        }
        let skip_linting = properties.get("SKIP_LINTING").is_some_and(is_on);
        let code_check = if skip_linting {
            String::new()
        } else {
            self.code_check(target, source)
        };
        if !code_check.is_empty() {
            command = format!("{code_check}{command}");
        } else {
            let compiler_launcher = flags::compiler_launcher(self.ctx, target, lang);
            if !compiler_launcher.is_empty() {
                command = format!("{compiler_launcher}{command}");
            }
        }
        let rule_launcher = rule_launcher(self.ctx, target, "RULE_LAUNCH_COMPILE");
        if !rule_launcher.is_empty() {
            let expanded_launcher = flags::expand_rule_for_step(
                self.ctx,
                target,
                lang,
                flags::RuleStep::Compile,
                rule_launcher.trim_end(),
                &vars,
            );
            command = format!("{expanded_launcher} {command}");
        }
        let lang_can_export = matches!(
            lang.as_str(),
            "C" | "CXX" | "OBJC" | "OBJCXX" | "Fortran" | "CUDA" | "ISPC" | "HIP" | "ASM"
        );
        if lang_can_export
            && target
                .props
                .get("EXPORT_COMPILE_COMMANDS")
                .is_some_and(is_on)
        {
            let target_vars = flags::compile_target_vars(self.ctx, target, &self.ctx.config, lang);
            let mut exported = command.clone();
            let replacements = [
                (format!("$({lang}_FLAGS)"), target_vars.flags_untrimmed),
                (format!("$({lang}_DEFINES)"), target_vars.defines),
                (format!("$({lang}_INCLUDES)"), target_vars.includes),
            ];
            for (from, to) in replacements {
                exported = exported.replace(&from, &to);
            }
            for variable in ["CMAKE_START_TEMP_FILE", "CMAKE_END_TEMP_FILE"] {
                if let Some(value) = self.ctx.var(target.makefile, variable)
                    && !value.is_empty()
                {
                    exported = exported.replace(&value, "");
                }
            }
            if let Some(expanded) = &dep_flags_expanded {
                exported = exported.replacen(&format!(" {expanded}"), "", 1);
            }
            let working_directory = &self.ctx.interp.makefiles[dir].binary_dir;
            self.compile_commands.push(format!(
                "{{\n  \"directory\": \"{}\",\n  \"command\": \"{}\",\n  \"file\": \"{}\",\n  \"output\": \"{}\"\n}}",
                escape_json(working_directory),
                escape_json(&exported),
                escape_json(&util::collapse_full_path(
                    &source.path,
                    &self.ctx.interp.home_binary
                )),
                escape_json(obj_rel_top),
            ));
        }
        if dir != 0 {
            let cur = shell_path(self.ctx, &self.ctx.interp.makefiles[dir].binary_dir);
            command = format!("cd {cur} && {command}");
        }
        commands.push(command);
        write_make_rule(out, None, obj_rel_top, &depends, &commands, false);

        // `WriteMakeRule` multi-output handling: additional object outputs are
        // driven by the primary object and touched only when they exist.
        let mut extra_outputs = Vec::new();
        if let Some(outputs) = properties.get("OBJECT_OUTPUTS") {
            let evaluated = self.ctx.genex(outputs, None, None);
            if !evaluated.is_empty() {
                extra_outputs.extend(util::expand_list(&evaluated, false));
            }
        }
        if !ispc_header.is_empty() {
            extra_outputs.push(ispc_header.clone());
        }
        for output in extra_outputs {
            let output_rule = top_rule_path_preserve_slashes(self.ctx, &output);
            clean_files.push(output.clone());
            write_make_rule(
                out,
                None,
                &output_rule,
                std::slice::from_ref(&obj_rel_top.to_owned()),
                &[format!(
                    "@$(CMAKE_COMMAND) -E touch_nocreate {}",
                    shell_path_preserve_slashes(self.ctx, &output_rule)
                )],
                false,
            );
            impl_depends
                .output_pairs
                .push((output, obj_rel_top.to_owned()));
        }
        self.write_preprocess_and_assembly(target, source, dir, obj_rel_cur, obj_rel_top, out);
    }

    fn support_rel(&self, target: &Target, dir: usize) -> String {
        let support = gt::support_directory(self.ctx, target);
        let rel_cur = maybe_relative_cur(self.ctx, dir, &support);
        format!("{}{}", home_relative_output_path(self.ctx, dir), rel_cur)
    }

    fn write_preprocess_and_assembly(
        &self,
        target: &Target,
        source: &gt::SourceInfo,
        dir: usize,
        obj_rel_cur: &str,
        obj_rel_top: &str,
        out: &mut String,
    ) {
        let lang = &source.language;
        if !matches!(
            lang.as_str(),
            "C" | "CXX" | "OBJC" | "OBJCXX" | "Fortran" | "CUDA" | "ISPC" | "HIP" | "ASM"
        ) {
            return;
        }
        let preprocess_target = strip_last_extension(obj_rel_top).to_owned() + ".i";
        let asm_target = strip_last_extension(obj_rel_top).to_owned() + ".s";
        let source_shell = shell_path(self.ctx, &source.path);
        for (target_name, verb, action, _extra) in [
            (
                preprocess_target,
                "Preprocessing",
                "PREPROCESS",
                format!(
                    "> {}",
                    shell_path(
                        self.ctx,
                        &format!("{}.i", strip_last_extension(obj_rel_cur))
                    )
                ),
            ),
            (
                asm_target,
                "Compiling",
                "ASSEMBLE",
                format!(
                    "-o {}",
                    shell_path(
                        self.ctx,
                        &format!("{}.s", strip_last_extension(obj_rel_cur))
                    )
                ),
            ),
        ] {
            let template = self.ctx.var_safe(
                target.makefile,
                &format!(
                    "CMAKE_{lang}_CREATE_{}_SOURCE",
                    if action == "PREPROCESS" {
                        "PREPROCESSED"
                    } else {
                        "ASSEMBLY"
                    }
                ),
            );
            let echo_output = format!(
                "{}.{}",
                strip_last_extension(obj_rel_cur),
                if action == "PREPROCESS" { "i" } else { "s" }
            );
            let echo_text = format!(
                "{verb} {lang} source {} {}",
                if action == "PREPROCESS" {
                    "to"
                } else {
                    "to assembly"
                },
                echo_output
            );
            let vars = vec![
                (
                    "DEFINES",
                    self.object_defines_var(
                        target,
                        source,
                        &self.source_props(target, &source.path),
                    ),
                ),
                (
                    "INCLUDES",
                    self.object_includes_var(
                        target,
                        source,
                        &self.source_props(target, &source.path),
                    ),
                ),
                (
                    "FLAGS",
                    self.object_flags_var(target, source, &self.source_props(target, &source.path)),
                ),
                ("SOURCE", source_shell.clone()),
                (
                    "OBJECT",
                    shell_path(
                        self.ctx,
                        &format!(
                            "{}.{}",
                            strip_last_extension(obj_rel_cur),
                            if action == "PREPROCESS" { "i" } else { "s" }
                        ),
                    ),
                ),
                (
                    "PREPROCESSED_SOURCE",
                    shell_path(
                        self.ctx,
                        &format!("{}.i", strip_last_extension(obj_rel_cur)),
                    ),
                ),
                (
                    "ASSEMBLY_SOURCE",
                    shell_path(
                        self.ctx,
                        &format!("{}.s", strip_last_extension(obj_rel_cur)),
                    ),
                ),
            ];
            let rule_var = format!(
                "CMAKE_{lang}_CREATE_{}_SOURCE",
                if action == "PREPROCESS" {
                    "PREPROCESSED"
                } else {
                    "ASSEMBLY"
                }
            );
            let mut command = if template.is_empty() {
                format!("$(CMAKE_COMMAND) -E cmake_unimplemented_variable {rule_var}")
            } else {
                flags::expand_rule_for_step(
                    self.ctx,
                    target,
                    lang,
                    flags::RuleStep::Compile,
                    &template,
                    &vars,
                )
            };
            if dir != 0 {
                command = format!(
                    "cd {} && {command}",
                    shell_path(self.ctx, &self.ctx.interp.makefiles[dir].binary_dir)
                );
            }
            write_make_rule(
                out,
                None,
                &target_name,
                &["cmake_force".into()],
                &[
                    format!("@{}", self.echo(&echo_text, "--green", None)),
                    command,
                ],
                false,
            );
        }
    }

    fn source_props(&self, target: &Target, path: &str) -> crate::props::Props {
        crate::commands::target::find_known_source(self.ctx.interp, target.makefile, path)
            .map(|index| {
                self.ctx.interp.ext.sources[&target.makefile][index]
                    .props
                    .clone()
            })
            .unwrap_or_default()
    }

    fn object_defines_var(
        &self,
        target: &Target,
        source: &gt::SourceInfo,
        properties: &crate::props::Props,
    ) -> String {
        let extra =
            flags::source_define_flags(self.ctx, target, &self.ctx.config, source, properties);
        let base = format!("$({}_DEFINES)", source.language);
        if extra.is_empty() {
            base
        } else {
            format!("{base} {extra}")
        }
    }

    fn object_includes_var(
        &self,
        target: &Target,
        source: &gt::SourceInfo,
        properties: &crate::props::Props,
    ) -> String {
        let extra =
            flags::source_include_flags(self.ctx, target, &self.ctx.config, source, properties);
        let base = format!("$({}_INCLUDES)", source.language);
        if extra.is_empty() {
            base
        } else {
            format!("{extra} {base}")
        }
    }

    fn object_flags_var(
        &self,
        target: &Target,
        source: &gt::SourceInfo,
        properties: &crate::props::Props,
    ) -> String {
        let rule_flags =
            flags::source_rule_flags(self.ctx, target, &self.ctx.config, source, properties);
        let mut parts = Vec::new();
        if !rule_flags.before_language.is_empty() {
            parts.push(rule_flags.before_language);
        }
        let mut filter_arch = String::new();
        for arch in super::pch::pch_archs(self.ctx, target, &self.ctx.config, &source.language) {
            let pch_source =
                super::pch::pch_source(self.ctx, target, &self.ctx.config, &source.language, &arch);
            if pch_source == source.path {
                filter_arch = arch;
                break;
            }
        }
        parts.push(format!("$({}_FLAGS{filter_arch})", source.language));
        if !rule_flags.after_language.is_empty() {
            parts.push(rule_flags.after_language);
        }
        parts.join(" ")
    }

    fn source_prop(&self, target: &Target, path: &str, prop: &str) -> Option<String> {
        self.source_props(target, path).get(prop).map(str::to_owned)
    }

    fn attached_command(&self, dir: usize, path: &str) -> Option<(usize, usize)> {
        let index = crate::commands::target::find_known_source(self.ctx.interp, dir, path)?;
        self.ctx.interp.ext.sources[&dir][index]
            .custom_command
            .as_ref()?;
        Some((dir, index))
    }

    fn top_rule_path(&self, path: &str) -> String {
        if util::is_full_path(path)
            && let Ok(rel) = Path::new(path).strip_prefix(&self.ctx.interp.home_binary)
        {
            return rel.to_string_lossy().replace('\\', "/");
        }
        path.to_owned()
    }

    #[allow(clippy::too_many_arguments)]
    fn write_custom_commands(
        &mut self,
        target: &Target,
        target_key: &str,
        dir: usize,
        support_rel_cur: &str,
        support_rel_top: &str,
        out: &mut String,
        custom_outputs: &mut Vec<String>,
        implicit: &mut ImplicitDepends,
    ) -> (Vec<String>, Vec<String>) {
        let _ = support_rel_cur;
        let mut codegen_outputs = Vec::new();
        let mut custom_clean = Vec::new();
        let sources = gt::sources(self.ctx, target, &self.ctx.config);
        let cmp0113_new = crate::policy::index_of("CMP0113").is_some_and(|index| {
            target.policies.get(index).unwrap_or(PolicyStatus::Warn) == PolicyStatus::New
        });
        let mut local_visited = HashSet::new();
        for source in sources
            .iter()
            .filter(|source| source.kind == SourceKind::CustomCommandOutput)
        {
            let Some((cc_dir, cc_index)) = self.attached_command(target.makefile, &source.path)
            else {
                continue;
            };
            if cmp0113_new {
                let visited = self
                    .commands_visited
                    .entry((dir, target_key.to_owned()))
                    .or_default();
                if !visited.insert((cc_dir, cc_index)) {
                    continue;
                }
            } else if !local_visited.insert((cc_dir, cc_index)) {
                continue;
            }
            let (spec, command_implicit_depends) = {
                let command = self.ctx.interp.ext.sources[&cc_dir][cc_index]
                    .custom_command
                    .as_ref()
                    .expect("attached custom command");
                (
                    custom::spec_from_command(self.ctx, command),
                    command.implicit_depends.clone(),
                )
            };
            let evaluated =
                custom::evaluate_spec_cfg(self.ctx, &spec, &self.ctx.config, &self.ctx.config);
            if !self.ctx.interp.makefiles[target.makefile]
                .props
                .get("CLEAN_NO_CUSTOM")
                .is_some_and(is_on)
            {
                for file in evaluated.outputs.iter().chain(evaluated.byproducts.iter()) {
                    custom_clean.push(maybe_relative_cur(self.ctx, dir, file));
                }
            }
            for output in &evaluated.outputs {
                custom_outputs.push(output.clone());
            }
            let mut depends: Vec<String> = evaluated
                .depends
                .iter()
                .map(|dep| self.top_rule_path(dep))
                .collect();
            if !evaluated.full_depfile.is_empty() {
                depends.push(format!("{support_rel_top}/compiler_depend.ts"));
            }
            let mut commands = Vec::new();
            let comment = evaluated.description(self.ctx, target.makefile, "");
            if !comment.is_empty() {
                let progress = self.add_progress_file(
                    target,
                    self.top_rule_path(evaluated.outputs.first().map(String::as_str).unwrap_or("")),
                );
                commands.push(format!(
                    "@{}",
                    self.echo(&comment, "--blue --bold", Some(progress))
                ));
            }
            commands.extend(self.custom_command_lines(
                &evaluated,
                target,
                &self.ctx.interp.home_binary,
            ));
            if evaluated.outputs.is_empty() {
                continue;
            }
            let first = self.top_rule_path(&evaluated.outputs[0]);
            let hash_content = self.custom_command_hash_content(&evaluated, target);
            let hash = format!(
                "{:x}",
                <md5::Md5 as md5::Digest>::digest(hash_content.as_bytes())
            );
            self.rule_hashes
                .insert(maybe_relative_top(self.ctx, &evaluated.outputs[0]), hash);
            write_make_rule(out, None, &first, &depends, &commands, false);
            for output in evaluated.outputs.iter().skip(1) {
                let extra = self.top_rule_path(output);
                write_make_rule(
                    out,
                    None,
                    &extra,
                    std::slice::from_ref(&first),
                    &[format!(
                        "@$(CMAKE_COMMAND) -E touch_nocreate {}",
                        shell_path(self.ctx, &extra)
                    )],
                    false,
                );
                implicit
                    .output_pairs
                    .push((output.clone(), evaluated.outputs[0].clone()));
            }
            for (language, file) in &command_implicit_depends {
                let obj = util::collapse_full_path(
                    &evaluated.outputs[0],
                    &self.ctx.interp.makefiles[dir].binary_dir,
                );
                let src =
                    util::collapse_full_path(file, &self.ctx.interp.makefiles[dir].binary_dir);
                implicit.add(language, &obj, &src, false);
            }
            if evaluated.codegen {
                codegen_outputs.push(evaluated.outputs[0].clone());
            }
            if !evaluated.full_depfile.is_empty() {
                let obj = util::collapse_full_path(
                    &evaluated.outputs[0],
                    &self.ctx.interp.makefiles[dir].binary_dir,
                );
                implicit.add("CUSTOM", &obj, &evaluated.full_depfile, true);
            }
        }
        (codegen_outputs, custom_clean)
    }

    /// The rule-content input to `cmGlobalGenerator::AddRuleHash`: the
    /// working directory followed by each shell-converted command without a
    /// launcher, before the directory-change wrapper and jobserver prefix.
    fn custom_command_hash_content(
        &self,
        evaluated: &custom::EvaluatedCommand,
        target: &Target,
    ) -> String {
        let makefile = target.makefile;
        let working_directory = if evaluated.working_directory.is_empty() {
            self.ctx.interp.makefiles[makefile].binary_dir.clone()
        } else {
            evaluated.working_directory.clone()
        };
        let mut content = working_directory;
        for (index, argv) in evaluated.command_lines.iter().enumerate() {
            if argv.first().is_none_or(|program| program.is_empty()) {
                continue;
            }
            let mut line = shell_path(self.ctx, &argv[0]);
            evaluated.append_arguments(self.ctx, index, &mut line);
            content.push_str(&line);
        }
        content
    }

    fn custom_command_lines(
        &self,
        evaluated: &custom::EvaluatedCommand,
        target: &Target,
        relative_dir: &str,
    ) -> Vec<String> {
        let makefile = target.makefile;
        let working_directory = if evaluated.working_directory.is_empty() {
            self.ctx.interp.makefiles[makefile].binary_dir.clone()
        } else {
            evaluated.working_directory.clone()
        };
        let launcher = {
            let value = target
                .props
                .get("RULE_LAUNCH_CUSTOM")
                .or_else(|| {
                    self.ctx.interp.makefiles[makefile]
                        .props
                        .get("RULE_LAUNCH_CUSTOM")
                })
                .or_else(|| self.ctx.interp.global_props.get("RULE_LAUNCH_CUSTOM"))
                .filter(|value| !value.is_empty());
            match value {
                Some(value) => {
                    let binary_dir = &self.ctx.interp.makefiles[makefile].binary_dir;
                    let output = evaluated
                        .outputs
                        .iter()
                        .map(|output| {
                            let path = if evaluated.working_directory.is_empty() {
                                maybe_relative_cur(self.ctx, makefile, output)
                            } else {
                                output.clone()
                            };
                            shell_path(self.ctx, &path)
                        })
                        .collect::<Vec<_>>()
                        .join(",");
                    let expanded = value
                        .replace("<OUTPUT>", &output)
                        .replace("<TARGET_NAME>", &target.name)
                        .replace(
                            "<CMAKE_CURRENT_BINARY_DIR>",
                            &shell_path(self.ctx, binary_dir),
                        )
                        .replace("<ROLE>", "custom");
                    format!("{} ", expanded)
                }
                None => String::new(),
            }
        };
        let mut lines = Vec::new();
        for (index, argv) in evaluated.command_lines.iter().enumerate() {
            if argv.first().is_none_or(|program| program.is_empty()) {
                continue;
            }
            // cmLocalUnixMakefileGenerator3::AppendCustomCommand: collapse
            // "/./", and make the program relative to the current binary
            // directory only when that is the working directory; a path that
            // lands in it gets a leading "./".
            let mut program = argv[0].replace("/./", "/");
            let had_slash = program.contains('/');
            if evaluated.working_directory.is_empty() {
                program = maybe_relative_cur(self.ctx, makefile, &program);
            }
            if had_slash && !program.contains('/') {
                program = format!("./{program}");
            }
            let program = shell_path(self.ctx, &program);
            let mut line = format!("{launcher}{program}");
            evaluated.append_arguments(self.ctx, index, &mut line);
            lines.push(line);
        }
        if working_directory != relative_dir {
            let prefix = format!("cd {} && ", shell_path(self.ctx, &working_directory));
            for line in &mut lines {
                *line = format!("{prefix}{line}");
            }
        }
        if evaluated.job_server_aware {
            for line in &mut lines {
                *line = format!("+{line}");
            }
        }
        lines
    }

    /// `cmOSXBundleGenerator::GenerateMacOSXContentStatements` driven over
    /// the target's header and extra source kinds, with Makefile copy rules
    /// from `cmMakefileTargetGenerator::MacOSXContentGeneratorType`.
    fn write_mac_osx_content_statements(
        &mut self,
        target: &Target,
        sources: &[gt::SourceInfo],
        mac_content_folders: &mut BTreeSet<String>,
        out: &mut String,
    ) -> (Vec<String>, Vec<String>) {
        let mut extra_files = Vec::new();
        let mut clean_files = Vec::new();
        if !matches!(
            target.kind,
            TargetKind::StaticLibrary
                | TargetKind::SharedLibrary
                | TargetKind::ModuleLibrary
                | TargetKind::ObjectLibrary
                | TargetKind::Executable
        ) {
            return (extra_files, clean_files);
        }
        let config = self.ctx.config.clone();
        for kind in [SourceKind::Header, SourceKind::Extra] {
            for source in sources.iter().filter(|source| source.kind == kind) {
                let flags =
                    osx_bundle::target_source_file_flags(self.ctx, target, &source.path, None);
                if flags.kind == SourceFileType::Normal || !target.is_bundle_on_apple() {
                    continue;
                }
                let macdir = osx_bundle::init_mac_osx_content_directory(
                    self.ctx,
                    target,
                    &flags.mac_folder,
                    &config,
                    mac_content_folders,
                );
                let output_full = format!("{}/{}", macdir, util::filename_name(&source.path));
                clean_files.push(maybe_relative_cur(self.ctx, target.makefile, &output_full));
                let output = maybe_relative_top(self.ctx, &output_full);
                let echo_text = format!("Copying OS X content {output}");
                let copy = if Path::new(&source.path).is_dir() {
                    "copy_directory"
                } else {
                    "copy"
                };
                let command = format!(
                    "$(CMAKE_COMMAND) -E {copy} {} {}",
                    shell_path(self.ctx, &source.path),
                    shell_path(self.ctx, &output)
                );
                write_make_rule(
                    out,
                    None,
                    &output,
                    std::slice::from_ref(&source.path),
                    &[
                        format!("@{}", self.echo(&echo_text, "--green", None)),
                        command,
                    ],
                    false,
                );
                extra_files.push(output);
            }
        }
        (extra_files, clean_files)
    }

    fn write_object_library_rule(
        &mut self,
        target: &Target,
        objects: &[String],
        support_rel_top: &str,
        out: &mut String,
    ) {
        let dir = target.makefile;
        let mut depends: Vec<String> = objects
            .iter()
            .map(|object| {
                if util::is_full_path(object) {
                    object.clone()
                } else {
                    format!("{}{}", home_relative_output_path(self.ctx, dir), object)
                }
            })
            .collect();
        depends.push(format!("{support_rel_top}/build.make"));
        let mut commands = Vec::new();
        for command in target
            .custom_commands
            .iter()
            .filter(|command| command.kind == CustomCommandKind::PostBuild)
        {
            let spec = custom::spec_from_command(self.ctx, command);
            let evaluated =
                custom::evaluate_spec_cfg(self.ctx, &spec, &self.ctx.config, &self.ctx.config);
            commands.extend(self.custom_command_lines(
                &evaluated,
                target,
                &self.ctx.interp.makefiles[dir].binary_dir,
            ));
        }
        if dir != 0 {
            let cur = shell_path(self.ctx, &self.ctx.interp.makefiles[dir].binary_dir);
            for command in commands.iter_mut() {
                *command = format!("cd {cur} && {command}");
            }
        }
        write_make_rule(out, None, &target.name, &depends, &commands, true);
        write_make_rule(
            out,
            Some("Rule to build all files generated by this target."),
            &format!("{support_rel_top}/build"),
            std::slice::from_ref(&target.name),
            &[],
            true,
        );
    }

    /// `cmMakefileExecutableTargetGenerator::WriteNvidiaDeviceExecutableRule`
    /// and `cmMakefileLibraryTargetGenerator::WriteNvidiaDeviceLibraryRules`.
    /// Returns the current-binary-relative device-link object.
    #[allow(clippy::too_many_arguments)]
    fn write_device_link_rule(
        &mut self,
        target: &Target,
        objects: &[String],
        externals: &[String],
        language: &str,
        support_rel_cur: &str,
        support_rel_top: &str,
        out: &mut String,
    ) -> Option<String> {
        let config = self.ctx.config.clone();
        let dir = target.makefile;
        if !super::link::requires_device_linking(self.ctx, target, &config, language) {
            return None;
        }
        // Clang CUDA uses a multi-architecture fatbinary registration pipeline
        // rather than the NVIDIA driver device-link rule.  The NVIDIA branch
        // below is the upstream behavior for NVCC and NVIDIA driver-mode
        // clang toolchains, which are the toolchains represented in the
        // project corpus.
        if self.ctx.var_safe(target.makefile, "CMAKE_CUDA_COMPILER_ID") == "Clang" {
            return None;
        }
        let object_extension = self
            .ctx
            .var_safe(target.makefile, "CMAKE_CUDA_OUTPUT_EXTENSION");
        let device_rel_cur = format!("{support_rel_cur}/cmake_device_link{object_extension}");
        let device_rel_top = format!(
            "{}{}",
            home_relative_output_path(self.ctx, dir),
            device_rel_cur
        );
        let support = gt::support_directory(self.ctx, target);

        let mut device_line =
            super::link::makefile_device_link_line(self.ctx, target, &config, language);
        for (fragment, _) in &mut device_line.lib_fragments {
            fragment.push(' ');
        }
        let mut depends: Vec<String> = objects
            .iter()
            .map(|object| format!("{}{}", home_relative_output_path(self.ctx, dir), object))
            .collect();
        depends.extend(
            externals
                .iter()
                .map(|object| maybe_relative_top(self.ctx, object)),
        );
        depends.push(format!("{support_rel_top}/build.make"));
        if flags::link_dependency_file(self.ctx, target, language).is_some() {
            depends.push(format!("{support_rel_top}/compiler_depend.ts"));
        }
        depends.extend(
            device_line
                .depends
                .iter()
                .map(|path| maybe_relative_top(self.ctx, path)),
        );
        if let Some(mdi) = gt::module_definition_info(self.ctx, target, &config) {
            depends.extend(mdi.sources.iter().map(|source| (*source).to_owned()));
        }
        depends.extend(
            gt::sources(self.ctx, target, &config)
                .iter()
                .filter(|source| source.kind == SourceKind::Manifest)
                .map(|source| maybe_relative_top(self.ctx, &source.path)),
        );
        depends.extend(
            device_line
                .link_depends
                .iter()
                .map(|path| maybe_relative_top(self.ctx, path)),
        );

        let response_flag = makefile_device_response_flag(self.ctx, target);
        let entries = makefile_object_entries(self.ctx, target, objects, externals);
        let use_response_objects =
            makefile_use_response_objects(self.ctx, target, "CUDA", &entries);
        let object_strings = if use_response_objects {
            makefile_object_strings(&entries, 131000)
        } else {
            vec![entries.join(" ")]
        };
        let mut object_rsp_depends = Vec::new();
        let object_list = if use_response_objects {
            let mut references = Vec::new();
            for (index, contents) in object_strings.iter().enumerate() {
                let name = format!("deviceObjects{}.rsp", index + 1);
                let path = format!("{support}/{name}");
                let _ =
                    crate::fsutil::write_if_different(Path::new(&path), &format!("{contents}\n"));
                object_rsp_depends.push(format!("{support_rel_top}/{name}"));
                references.push(format!(
                    "{response_flag}{}",
                    shell_path(self.ctx, &maybe_relative_cur(self.ctx, dir, &path))
                ));
            }
            references.join(" ")
        } else {
            entries.join(" ")
        };

        let mut libraries = makefile_link_libraries(&device_line);
        let use_response_libraries = self
            .ctx
            .var(
                target.makefile,
                "CMAKE_CUDA_USE_RESPONSE_FILE_FOR_LIBRARIES",
            )
            .is_some_and(|value| !value.is_empty() && is_on(&value))
            && !libraries.trim_start().is_empty();
        if use_response_libraries {
            let path = format!("{support}/deviceLinkLibs.rsp");
            let _ = crate::fsutil::write_if_different(Path::new(&path), &format!("{libraries}\n"));
            depends.push(format!("{support_rel_top}/deviceLinkLibs.rsp"));
            libraries = format!(
                "{response_flag}{}",
                shell_path(self.ctx, &maybe_relative_cur(self.ctx, dir, &path))
            );
        }
        depends.extend(object_rsp_depends);
        depends.push(format!("{support_rel_top}/dlink.txt"));

        let dependency_ipo = super::link::link_information(self.ctx, target, &config, language)
            .map(|info| {
                info.items.iter().any(|item| {
                    item.target
                        .as_deref()
                        .and_then(|key| self.ctx.interp.ext.targets.get(key))
                        .is_some_and(|dependency| {
                            dependency.kind == TargetKind::StaticLibrary
                                && !dependency
                                    .props
                                    .get("CUDA_RESOLVE_DEVICE_SYMBOLS")
                                    .is_some_and(util::is_on)
                                && dependency
                                    .props
                                    .get("CUDA_SEPARABLE_COMPILATION")
                                    .is_some_and(util::is_on)
                                && flags::ipo_enabled(self.ctx, dependency, &config, "CUDA")
                        })
                })
            })
            .unwrap_or(false);
        let device_flags = flags::device_link_flag_vars(self.ctx, target, &config, dependency_ipo);
        let rule_var = if target.kind == TargetKind::Executable {
            "CMAKE_CUDA_DEVICE_LINK_EXECUTABLE"
        } else {
            "CMAKE_CUDA_DEVICE_LINK_LIBRARY"
        };
        let template = self.ctx.var_safe(target.makefile, rule_var);
        if template.is_empty() {
            self.ctx.system_error(format!(
                "Error required internal CMake variable not set, cmake may not be built correctly.\nMissing variable is:\n{rule_var}"
            ));
        }
        let device_shell = shell_path(self.ctx, &device_rel_cur);
        let object_dir = shell_path(self.ctx, support_rel_cur);
        let compile_pdb = gt::target_compile_pdb(self.ctx, target, &config);
        let compile_pdb_shell = shell_path_keep_trailing_slash(
            self.ctx,
            &maybe_relative_cur(self.ctx, dir, &compile_pdb),
        );
        let vars = vec![
            ("LANGUAGE", "CUDA".to_owned()),
            ("TARGET_NAME", target.name.clone()),
            ("TARGET_TYPE", target.kind.type_name().to_owned()),
            (
                "CMAKE_CURRENT_BINARY_DIR",
                shell_path(self.ctx, &self.ctx.interp.makefiles[dir].binary_dir),
            ),
            ("OBJECTS", object_list),
            ("TARGET", device_shell.clone()),
            ("TARGET_FILE", device_shell),
            ("OBJECT_DIR", object_dir),
            ("LINK_LIBRARIES", libraries),
            ("LINK_FLAGS", device_flags.link_flags),
            (
                "LANGUAGE_COMPILE_FLAGS",
                device_flags.language_compile_flags,
            ),
            ("TARGET_COMPILE_PDB", compile_pdb_shell),
            ("CONFIG", config.clone()),
        ];
        let launcher = rule_launcher(self.ctx, target, "RULE_LAUNCH_LINK");
        let mut script = String::new();
        for part in template.split(';').filter(|part| !part.is_empty()) {
            let expanded = flags::expand_rule_for_step(
                self.ctx,
                target,
                "CUDA",
                flags::RuleStep::Link,
                &format!("{launcher}{part}"),
                &vars,
            );
            if !expanded.is_empty() && !expanded.starts_with(':') {
                script.push_str(&expanded);
                script.push('\n');
            }
        }
        let script_path = format!("{support}/dlink.txt");
        let _ = crate::fsutil::write_generated(Path::new(&script_path), &script);

        let progress = self.add_progress_file(target, device_rel_top.clone());
        let mut commands = vec![format!(
            "@{}",
            self.echo(
                &format!(
                    "Linking CUDA device code {}",
                    shell_path(self.ctx, &device_rel_cur)
                ),
                "--green --bold",
                Some(progress),
            ),
        )];
        let mut link_command = format!(
            "$(CMAKE_COMMAND) -E cmake_link_script {support_rel_cur}/dlink.txt --verbose=$(VERBOSE)"
        );
        if dir != 0 {
            let cur = shell_path(self.ctx, &self.ctx.interp.makefiles[dir].binary_dir);
            link_command = format!("cd {cur} && {link_command}");
        }
        commands.push(link_command);
        write_make_rule(out, None, &device_rel_top, &depends, &commands, false);
        write_make_rule(
            out,
            Some("Rule to build all files generated by this target."),
            &format!("{support_rel_top}/build"),
            std::slice::from_ref(&device_rel_top),
            &[],
            true,
        );
        Some(device_rel_cur)
    }

    #[allow(clippy::too_many_arguments)]
    fn write_link(
        &mut self,
        target: &Target,
        objects: &[String],
        externals: &[String],
        extra_files: &[String],
        mac_content_folders: &mut BTreeSet<String>,
        support_rel_cur: &str,
        support_rel_top: &str,
        _compiler_depends: bool,
        out: &mut String,
    ) {
        let config = self.ctx.config.clone();
        let dir = target.makefile;
        let language = gt::linker_language(self.ctx, target, &config);
        let varname = target
            .name
            .replace('.', "_")
            .replace('-', "__")
            .replace('+', "___");
        let pch_extension = self.ctx.var(target.makefile, "CMAKE_PCH_EXTENSION");
        let variable_objects: Vec<&String> = objects
            .iter()
            .filter(|object| {
                pch_extension
                    .as_ref()
                    .is_none_or(|extension| !object.ends_with(extension.as_str()))
            })
            .collect();
        let write_objects_variable = |out: &mut String, name: &str, entries: &[String]| {
            let _ = write!(out, "{varname}{name} =");
            for object in entries {
                let _ = write!(
                    out,
                    " \\
\"{}\"",
                    quote_output_path(object)
                );
            }
            out.push_str("\n\n");
        };
        let needs_device_link =
            super::link::requires_device_linking(self.ctx, target, &config, &language)
                && self.ctx.var_safe(target.makefile, "CMAKE_CUDA_COMPILER_ID") != "Clang";
        if needs_device_link {
            let _ = writeln!(out, "# Object files for target {}", target.name);
            write_objects_variable(
                out,
                "_OBJECTS",
                &variable_objects
                    .iter()
                    .map(|o| (*o).clone())
                    .collect::<Vec<_>>(),
            );
            let _ = writeln!(out, "# External object files for target {}", target.name);
            write_objects_variable(out, "_EXTERNAL_OBJECTS", externals);
        }
        let device_link_object = self.write_device_link_rule(
            target,
            objects,
            externals,
            &language,
            support_rel_cur,
            support_rel_top,
            out,
        );
        let _ = writeln!(out, "# Object files for target {}", target.name);
        write_objects_variable(
            out,
            "_OBJECTS",
            &variable_objects
                .iter()
                .map(|o| (*o).clone())
                .collect::<Vec<_>>(),
        );
        let _ = writeln!(out, "# External object files for target {}", target.name);
        write_objects_variable(out, "_EXTERNAL_OBJECTS", externals);
        let names = gt::names(self.ctx, target, &config);
        let directory = gt::directory(self.ctx, target, &config, ArtifactType::Runtime);
        // cmOSXBundleGenerator: create the bundle layout at generate time.
        if target.is_app_bundle_on_apple() {
            osx_bundle::create_app_bundle(self.ctx, target, &names.output, &directory, &config);
        } else if target.is_framework_on_apple() {
            let mut skip = osx_bundle::SkipParts::default();
            if gt::has_import_library(self.ctx, target, &config) {
                skip.text_stubs = false;
            }
            osx_bundle::create_framework(
                self.ctx,
                target,
                &names.output,
                &directory,
                &config,
                skip,
                mac_content_folders,
            );
        } else if target.is_cf_bundle_on_apple() {
            osx_bundle::create_cf_bundle(self.ctx, target, &names.output, &directory, &config);
        }
        let target_full = gt::full_path(self.ctx, target, &config, ArtifactType::Runtime, true);
        let output_full = gt::full_path(self.ctx, target, &config, ArtifactType::Runtime, false);
        let target_rel_cur = maybe_relative_cur(self.ctx, dir, &target_full);
        let target_rel_top = maybe_relative_top(self.ctx, &target_full);
        let mut depends: Vec<String> = objects
            .iter()
            .map(|o| format!("{}{}", home_relative_output_path(self.ctx, dir), o))
            .collect();
        depends.extend(externals.iter().map(|o| maybe_relative_top(self.ctx, o)));
        depends.push(format!("{support_rel_top}/build.make"));
        let line = if target.kind == TargetKind::StaticLibrary {
            super::link::LinkLine::default()
        } else {
            super::link::makefile_link_line(self.ctx, target, &config, &language)
        };
        let use_archive_rules = makefile_use_archive_rules(self.ctx, target, &language);
        let (mut use_response_objects, object_strings) =
            makefile_response_plan(self.ctx, target, objects, externals, &language);
        if use_archive_rules {
            use_response_objects = false;
        }
        if target.kind != TargetKind::StaticLibrary {
            if flags::link_dependency_file(self.ctx, target, &language).is_some() {
                depends.push(format!("{support_rel_top}/compiler_depend.ts"));
            }
            depends.extend(line.depends.iter().map(|p| maybe_relative_top(self.ctx, p)));
        }
        if let Some(device_object) = &device_link_object {
            depends.push(format!(
                "{}{}",
                home_relative_output_path(self.ctx, dir),
                device_object
            ));
        }
        if let Some(mdi) = gt::module_definition_info(self.ctx, target, &config) {
            depends.extend(mdi.sources.iter().map(|source| (*source).to_owned()));
        }
        depends.extend(
            gt::sources(self.ctx, target, &config)
                .iter()
                .filter(|source| source.kind == SourceKind::Manifest)
                .map(|source| maybe_relative_top(self.ctx, &source.path)),
        );
        let libraries_text = makefile_link_libraries(&line);
        let use_response_libraries = target.kind != TargetKind::StaticLibrary
            && self
                .ctx
                .var(
                    target.makefile,
                    &format!("CMAKE_{language}_USE_RESPONSE_FILE_FOR_LIBRARIES"),
                )
                .is_some_and(|value| !value.is_empty() && is_on(&value))
            && !libraries_text.trim_start().is_empty();
        if use_response_libraries {
            depends.push(format!("{support_rel_top}/linkLibs.rsp"));
        }
        if use_response_objects {
            for index in 1..=object_strings.len() {
                depends.push(format!("{support_rel_top}/objects{index}.rsp"));
            }
        }
        let link_script = format!("{support_rel_cur}/link.txt");
        depends.push(format!("{support_rel_top}/link.txt"));

        let progress = self.add_progress_file(target, target_rel_top.clone());
        let visible = match target.kind {
            TargetKind::Executable => "executable",
            TargetKind::StaticLibrary => "static library",
            TargetKind::SharedLibrary => "shared library",
            TargetKind::ModuleLibrary if target.is_cf_bundle_on_apple() => "CFBundle shared module",
            TargetKind::ModuleLibrary => "shared module",
            TargetKind::ObjectLibrary => "object library",
            _ => "target",
        };
        let display = maybe_relative_cur(self.ctx, dir, &output_full);
        let mut commands = vec![format!(
            "@{}",
            self.echo(
                &format!(
                    "Linking {language} {visible} {}",
                    shell_path(self.ctx, &display)
                ),
                "--green --bold",
                Some(progress),
            ),
        )];
        if target.kind == TargetKind::StaticLibrary {
            commands.push(format!(
                "$(CMAKE_COMMAND) -P {support_rel_cur}/cmake_clean_target.cmake"
            ));
        }
        for stage in [CustomCommandKind::PreBuild, CustomCommandKind::PreLink] {
            for command in target
                .custom_commands
                .iter()
                .filter(|command| command.kind == stage)
            {
                let spec = custom::spec_from_command(self.ctx, command);
                let evaluated =
                    custom::evaluate_spec_cfg(self.ctx, &spec, &self.ctx.config, &self.ctx.config);
                if let Some(comment) = &evaluated.comment
                    && !comment.is_empty()
                {
                    commands.push(format!("@{}", self.echo(comment, "--blue --bold", None)));
                }
                commands.extend(self.custom_command_lines(
                    &evaluated,
                    target,
                    &self.ctx.interp.makefiles[dir].binary_dir,
                ));
            }
        }
        commands.push(format!(
            "$(CMAKE_COMMAND) -E cmake_link_script {link_script} --verbose=$(VERBOSE)"
        ));
        if names.output != names.real && !target.is_framework_on_apple() {
            let symlink_rule = if target.kind == TargetKind::Executable {
                "cmake_symlink_executable"
            } else {
                "cmake_symlink_library"
            };
            let real = shell_path(self.ctx, &target_rel_cur);
            let so = shell_path(
                self.ctx,
                &maybe_relative_cur(
                    self.ctx,
                    dir,
                    &format!("{directory}/{}", names.shared_object),
                ),
            );
            let output = shell_path(self.ctx, &maybe_relative_cur(self.ctx, dir, &output_full));
            if target.kind == TargetKind::Executable {
                commands.push(format!(
                    "$(CMAKE_COMMAND) -E {symlink_rule} {real} {output}"
                ));
            } else {
                commands.push(format!(
                    "$(CMAKE_COMMAND) -E {symlink_rule} {real} {so} {output}"
                ));
            }
        }
        for command in target
            .custom_commands
            .iter()
            .filter(|command| command.kind == CustomCommandKind::PostBuild)
        {
            let spec = custom::spec_from_command(self.ctx, command);
            let evaluated =
                custom::evaluate_spec_cfg(self.ctx, &spec, &self.ctx.config, &self.ctx.config);
            if let Some(comment) = &evaluated.comment
                && !comment.is_empty()
            {
                commands.push(format!("@{}", self.echo(comment, "--blue --bold", None)));
            }
            commands.extend(self.custom_command_lines(
                &evaluated,
                target,
                &self.ctx.interp.makefiles[dir].binary_dir,
            ));
        }
        if dir != 0 {
            let cur = shell_path(self.ctx, &self.ctx.interp.makefiles[dir].binary_dir);
            for command in commands.iter_mut().skip(1) {
                *command = format!("cd {cur} && {command}");
            }
        }
        write_make_rule(out, None, &target_rel_top, &depends, &commands, false);
        if output_full != target_full {
            for artifact in [names.shared_object.clone(), names.output.clone()] {
                if artifact.is_empty() || artifact == names.real {
                    continue;
                }
                let full = format!("{directory}/{artifact}");
                let rel = maybe_relative_top(self.ctx, &full);
                // Libraries write their names as extra outputs of one
                // WriteMakeRule (touch_nocreate, MaybeRelativeToTopBinDir);
                // cmMakefileExecutableTargetGenerator writes the symlink name
                // as its own rule with no commands.
                let touch = if target.kind == TargetKind::Executable {
                    Vec::new()
                } else {
                    vec![format!(
                        "@$(CMAKE_COMMAND) -E touch_nocreate {}",
                        shell_path(self.ctx, &rel)
                    )]
                };
                write_make_rule(
                    out,
                    None,
                    &rel,
                    std::slice::from_ref(&target_rel_top),
                    &touch,
                    false,
                );
            }
        }
        self.write_link_script(
            target,
            objects,
            externals,
            &language,
            support_rel_cur,
            &line,
        );
        let driver = format!(
            "{support_rel_top}/{}",
            if need_relink_before_install(self.ctx, target, &language) {
                "preinstall"
            } else {
                "build"
            }
        );
        let output_driver = maybe_relative_top(self.ctx, &output_full);
        let mut driver_depends = vec![output_driver];
        let mut extra_files = extra_files.to_vec();
        extra_files.sort();
        extra_files.dedup();
        driver_depends.extend(extra_files);
        write_make_rule(
            out,
            Some("Rule to build all files generated by this target."),
            &driver,
            &driver_depends,
            &[],
            true,
        );
    }

    /// `cmCommonTargetGenerator::AppendOSXVerFlag`.
    fn append_osx_ver_flag(
        &self,
        flags_text: &mut String,
        language: &str,
        name: &str,
        so: bool,
        target: &Target,
    ) {
        let Some(flag) = self
            .ctx
            .var(
                target.makefile,
                &format!("CMAKE_{language}_OSX_{name}_VERSION_FLAG"),
            )
            .filter(|flag| !flag.is_empty())
        else {
            return;
        };
        let (major, minor, patch) = gt::target_version_fallback(
            target,
            &format!("MACHO_{name}_VERSION"),
            if so { "SOVERSION" } else { "VERSION" },
        );
        if major > 0 || minor > 0 || patch > 0 {
            if !flags_text.is_empty() {
                flags_text.push(' ');
            }
            flags_text.push_str(&format!("{flag}{major}.{minor}.{patch}"));
        }
    }

    fn write_link_script(
        &self,
        target: &Target,
        objects: &[String],
        externals: &[String],
        language: &str,
        _support_rel_cur: &str,
        line: &super::link::LinkLine,
    ) {
        let config = self.ctx.config.clone();
        let dir = target.makefile;
        let names = gt::names(self.ctx, target, &config);
        let target_full = gt::full_path(self.ctx, target, &config, ArtifactType::Runtime, true);
        let target_shell = shell_path(self.ctx, &maybe_relative_cur(self.ctx, dir, &target_full));
        let entries = makefile_object_entries(self.ctx, target, objects, externals);
        let (mut use_response_objects, object_strings) =
            makefile_response_plan(self.ctx, target, objects, externals, language);
        if makefile_use_archive_rules(self.ctx, target, language) {
            use_response_objects = false;
        }
        let response_flag = makefile_response_flag(self.ctx, target, language);
        let support = gt::support_directory(self.ctx, target);
        let mut object_list = if use_response_objects {
            let mut references = Vec::new();
            for (index, contents) in object_strings.iter().enumerate() {
                let name = format!("objects{}.rsp", index + 1);
                let path = format!("{support}/{name}");
                let _ =
                    crate::fsutil::write_if_different(Path::new(&path), &format!("{contents}\n"));
                references.push(format!(
                    "{response_flag}{}",
                    shell_path(self.ctx, &maybe_relative_cur(self.ctx, dir, &path))
                ));
            }
            references.join(" ")
        } else {
            entries.join(" ")
        };
        if super::link::requires_device_linking(self.ctx, target, &config, language)
            && self.ctx.var_safe(target.makefile, "CMAKE_CUDA_COMPILER_ID") != "Clang"
        {
            let object_extension = self
                .ctx
                .var_safe(target.makefile, "CMAKE_CUDA_OUTPUT_EXTENSION");
            let support_rel = maybe_relative_cur(self.ctx, dir, &support);
            let device_object = shell_path(
                self.ctx,
                &format!("{support_rel}/cmake_device_link{object_extension}"),
            );
            object_list.push(' ');
            object_list.push_str(&device_object);
        }
        let mut create_var = match target.kind {
            TargetKind::Executable => {
                let with_exports = format!("CMAKE_{language}_LINK_EXECUTABLE_WITH_EXPORTS");
                if target.is_executable_with_exports()
                    && self.ctx.var(target.makefile, &with_exports).is_some()
                {
                    with_exports
                } else {
                    format!("CMAKE_{language}_LINK_EXECUTABLE")
                }
            }
            TargetKind::StaticLibrary => format!("CMAKE_{language}_CREATE_STATIC_LIBRARY"),
            TargetKind::SharedLibrary => {
                if target.is_archived_aix_shared_library() {
                    format!("CMAKE_{language}_CREATE_SHARED_LIBRARY_ARCHIVE")
                } else {
                    format!("CMAKE_{language}_CREATE_SHARED_LIBRARY")
                }
            }
            TargetKind::ModuleLibrary => format!("CMAKE_{language}_CREATE_SHARED_MODULE"),
            _ => String::new(),
        };
        let ipo = flags::ipo_enabled(self.ctx, target, &config, language);
        let feature_rule = |var: String| {
            if ipo {
                let ipo_var = format!("{var}_IPO");
                if self.ctx.var(target.makefile, &ipo_var).is_some() {
                    return ipo_var;
                }
            }
            var
        };
        if target.kind == TargetKind::StaticLibrary {
            create_var = feature_rule(create_var);
        }
        let mut template = self
            .ctx
            .var(target.makefile, &create_var)
            .unwrap_or_default();
        if template.is_empty() && target.kind != TargetKind::StaticLibrary && !create_var.is_empty()
        {
            self.ctx.system_error(format!(
                "Error required internal CMake variable not set, cmake may not be built correctly.\nMissing variable is:\n{create_var}"
            ));
        }
        if template.is_empty() && target.kind == TargetKind::StaticLibrary {
            let create = self
                .ctx
                .var_safe(target.makefile, &format!("CMAKE_{language}_ARCHIVE_CREATE"));
            let finish = self
                .ctx
                .var_safe(target.makefile, &format!("CMAKE_{language}_ARCHIVE_FINISH"));
            template = format!("{create};{finish}");
        }
        let link_vars = flags::link_language_flag_vars(self.ctx, target, &config, language);
        let flags = if target.kind == TargetKind::Executable {
            link_vars.flags.clone()
        } else {
            let mut s = link_vars.language_compile_flags.clone();
            flags::append(&mut s, &link_vars.arch_flags);
            s
        };
        let mut link_flags = flags::makefile_link_flags(self.ctx, target, &config, language);
        if matches!(
            target.kind,
            TargetKind::SharedLibrary | TargetKind::ModuleLibrary
        ) {
            self.append_osx_ver_flag(&mut link_flags, language, "COMPATIBILITY", true, target);
            self.append_osx_ver_flag(&mut link_flags, language, "CURRENT", false, target);
        }
        let object_dir = shell_path(
            self.ctx,
            &maybe_relative_cur(self.ctx, dir, &gt::support_directory(self.ctx, target)),
        );
        let mut libraries = makefile_link_libraries(line);
        let use_response_libraries = target.kind != TargetKind::StaticLibrary
            && self
                .ctx
                .var(
                    target.makefile,
                    &format!("CMAKE_{language}_USE_RESPONSE_FILE_FOR_LIBRARIES"),
                )
                .is_some_and(|value| !value.is_empty() && is_on(&value))
            && !libraries.trim_start().is_empty();
        if use_response_libraries {
            let path = format!("{support}/linkLibs.rsp");
            let _ = crate::fsutil::write_if_different(Path::new(&path), &format!("{libraries}\n"));
            libraries = format!(
                "{response_flag}{}",
                shell_path(self.ctx, &maybe_relative_cur(self.ctx, dir, &path))
            );
        }
        let target_implib = if names.import_library.is_empty() {
            String::new()
        } else {
            shell_path(
                self.ctx,
                &maybe_relative_cur(
                    self.ctx,
                    dir,
                    &gt::full_path(self.ctx, target, &config, ArtifactType::Import, false),
                ),
            )
        };
        let target_pdb = if matches!(
            target.kind,
            TargetKind::Executable
                | TargetKind::StaticLibrary
                | TargetKind::SharedLibrary
                | TargetKind::ModuleLibrary
        ) {
            format!(
                "{}/{}",
                gt::pdb_directory(self.ctx, target, &config),
                names.pdb
            )
        } else {
            String::new()
        };
        let target_pdb_shell = shell_path(self.ctx, &target_pdb);
        let (version_major, version_minor) = makefile_target_version(target);
        let mut vars = vec![
            ("LANGUAGE", language.to_owned()),
            ("TARGET_NAME", target.name.clone()),
            ("TARGET_TYPE", target.kind.type_name().to_owned()),
            (
                "TARGET_LABELS",
                target
                    .props
                    .get("LABELS")
                    .map_or_else(String::new, |v| v.replace(';', ",")),
            ),
            (
                "CMAKE_CURRENT_BINARY_DIR",
                shell_path(self.ctx, &self.ctx.interp.makefiles[dir].binary_dir),
            ),
            ("OBJECTS", object_list.clone()),
            ("TARGET", target_shell.clone()),
            ("TARGET_FILE", target_shell.clone()),
            ("OBJECT_DIR", object_dir),
            ("FLAGS", flags),
            (
                "LANGUAGE_COMPILE_FLAGS",
                if target.kind == TargetKind::Executable {
                    link_vars.language_compile_flags.clone()
                } else {
                    let mut combined = link_vars.language_compile_flags.clone();
                    flags::append(&mut combined, &link_vars.arch_flags);
                    combined
                },
            ),
            ("ARCH_FLAGS", link_vars.arch_flags.clone()),
            ("LINK_FLAGS", link_flags),
            ("LINK_LIBRARIES", libraries),
            ("LINK_PATH", line.framework_path.clone() + &line.link_path),
            ("CONFIG", config.clone()),
            ("TARGET_PDB", target_pdb_shell),
            ("TARGET_IMPLIB", target_implib),
            ("TARGET_VERSION_MAJOR", version_major),
            ("TARGET_VERSION_MINOR", version_minor),
            (
                "AIX_EXPORTS",
                if target.platform.aix
                    && target
                        .props
                        .get("AIX_EXPORT_ALL_SYMBOLS")
                        .is_some_and(|value| !is_on(value))
                {
                    "-n".to_owned()
                } else {
                    String::new()
                },
            ),
            ("MANIFESTS", String::new()),
        ];
        if target.kind == TargetKind::SharedLibrary || target.kind == TargetKind::ModuleLibrary {
            let has_soname = gt::has_soname(self.ctx, target, &config);
            vars.push((
                "SONAME_FLAG",
                if has_soname {
                    self.ctx.var_safe(
                        target.makefile,
                        &format!("CMAKE_SHARED_LIBRARY_SONAME_{language}_FLAG"),
                    )
                } else {
                    String::new()
                },
            ));
            let soname = if has_soname {
                shell_path(self.ctx, &names.shared_object)
            } else {
                String::new()
            };
            vars.push(("TARGET_SONAME", soname.clone()));
            vars.push(("SONAME", soname));
        }
        if target.kind == TargetKind::SharedLibrary {
            let install_name_dir =
                super::install_name::install_name_dir_for_build_tree(self.ctx, target, &config);
            vars.push((
                "TARGET_INSTALLNAME_DIR",
                if install_name_dir.is_empty() {
                    String::new()
                } else {
                    shell_path_keep_trailing_slash(self.ctx, &install_name_dir)
                },
            ));
        }
        let mut text = String::new();
        let archive_create_var = feature_rule(format!("CMAKE_{language}_ARCHIVE_CREATE"));
        let archive_append_var = feature_rule(format!("CMAKE_{language}_ARCHIVE_APPEND"));
        let archive_finish_var = feature_rule(format!("CMAKE_{language}_ARCHIVE_FINISH"));
        let archive_create = self.ctx.var_safe(target.makefile, &archive_create_var);
        let archive_append = self.ctx.var_safe(target.makefile, &archive_append_var);
        let archive_finish = self.ctx.var_safe(target.makefile, &archive_finish_var);
        let use_archive_rules = target.kind == TargetKind::StaticLibrary
            && self
                .ctx
                .var(target.makefile, &create_var)
                .is_none_or(|value| value.is_empty())
            && !archive_create.is_empty()
            && !archive_append.is_empty();
        let launcher = rule_launcher(self.ctx, target, "RULE_LAUNCH_LINK");
        if let Some(mdi) = gt::module_definition_info(self.ctx, target, &config)
            && mdi.def_file_generated
        {
            let obj_list_file = format!("{}.objs", mdi.def_file);
            let mut command = format!(
                "{} -E __create_def {} {}",
                shell_path(self.ctx, &self.ctx.interp.cmake_command),
                shell_path(self.ctx, &maybe_relative_cur(self.ctx, dir, &mdi.def_file)),
                shell_path(self.ctx, &maybe_relative_cur(self.ctx, dir, &obj_list_file)),
            );
            let nm = self.ctx.var_safe(target.makefile, "CMAKE_NM");
            if !nm.is_empty() {
                command.push_str(&format!(" --nm={}", shell_path(self.ctx, &nm)));
            }
            text.push_str(&command);
            text.push('\n');
            let mut list = String::new();
            if mdi.windows_export_all_symbols {
                for object in objects.iter().chain(externals.iter()) {
                    if object.ends_with(".obj") {
                        list.push_str(object);
                        list.push('\n');
                    }
                }
            }
            for source in &mdi.sources {
                list.push_str(source);
                list.push('\n');
            }
            let _ = crate::fsutil::write_generated(Path::new(&obj_list_file), &list);
        }
        if use_archive_rules {
            let limit = (super::ninja::command_line_length_limit() / 2).max(1) as usize;
            let chunks = makefile_object_strings(&entries, limit);
            let expand_part = |part: &str, objects_value: String, text: &mut String| {
                let mut part_vars = vars.clone();
                for (name, value) in part_vars.iter_mut() {
                    if *name == "OBJECTS" {
                        *value = objects_value.clone();
                    }
                }
                let expanded = flags::expand_rule_for_step(
                    self.ctx,
                    target,
                    language,
                    flags::RuleStep::Link,
                    &format!("{launcher}{part}"),
                    &part_vars,
                );
                if !expanded.is_empty() && !expanded.starts_with(':') {
                    text.push_str(&expanded);
                    text.push('\n');
                }
            };
            let mut chunks_iter = chunks.into_iter();
            if let Some(first) = chunks_iter.next() {
                for part in archive_create.split(';').filter(|part| !part.is_empty()) {
                    expand_part(part, first.clone(), &mut text);
                }
                for chunk in chunks_iter {
                    for part in archive_append.split(';').filter(|part| !part.is_empty()) {
                        expand_part(part, chunk.clone(), &mut text);
                    }
                }
            }
            for part in archive_finish.split(';').filter(|part| !part.is_empty()) {
                expand_part(part, String::new(), &mut text);
            }
        } else {
            for part in template.split(';').filter(|s| !s.is_empty()) {
                let expanded = flags::expand_rule_for_step(
                    self.ctx,
                    target,
                    language,
                    flags::RuleStep::Link,
                    &format!("{launcher}{part}"),
                    &vars,
                );
                if !expanded.is_empty() && !expanded.starts_with(':') {
                    text.push_str(&expanded);
                    text.push('\n');
                }
            }
            let use_lwyu = matches!(
                target.kind,
                TargetKind::Executable | TargetKind::SharedLibrary | TargetKind::ModuleLibrary
            ) && target.props.get("LINK_WHAT_YOU_USE").is_some_and(is_on)
                && !self
                    .ctx
                    .var_safe(
                        target.makefile,
                        &format!("CMAKE_{language}_LINK_WHAT_YOU_USE_FLAG"),
                    )
                    .is_empty();
            if use_lwyu
                && let Some(checker) = self
                    .ctx
                    .var(target.makefile, "CMAKE_LINK_WHAT_YOU_USE_CHECK")
                && !checker.is_empty()
            {
                let check = format!(
                    "{} -E __run_co_compile --lwyu={} --source={target_shell}",
                    shell_path(self.ctx, &self.ctx.interp.cmake_command),
                    shell_path(self.ctx, &checker),
                );
                let expanded = flags::expand_rule_for_step(
                    self.ctx,
                    target,
                    language,
                    flags::RuleStep::Link,
                    &format!("{launcher}{check}"),
                    &vars,
                );
                if !expanded.is_empty() && !expanded.starts_with(':') {
                    text.push_str(&expanded);
                    text.push('\n');
                }
            }
        }
        let path = format!("{}/link.txt", gt::support_directory(self.ctx, target));
        let _ = crate::fsutil::write_if_different(Path::new(&path), &text);
    }

    fn write_clean_and_depend(
        &mut self,
        target: &Target,
        support: &str,
        support_rel_cur: &str,
        support_rel_top: &str,
        custom_outputs: &[String],
        out: &mut String,
    ) {
        let clean = format!("{support_rel_top}/clean");
        let mut command = format!("$(CMAKE_COMMAND) -P {support_rel_cur}/cmake_clean.cmake");
        if target.makefile != 0 {
            command = format!(
                "cd {} && {command}",
                shell_path(
                    self.ctx,
                    &self.ctx.interp.makefiles[target.makefile].binary_dir
                )
            );
        }
        write_make_rule(out, None, &clean, &[], &[command], true);
        let dep_target = format!("{support_rel_top}/depend");
        let mut depend_inputs: Vec<String> = custom_outputs
            .iter()
            .map(|output| self.top_rule_path(output))
            .collect();
        depend_inputs.sort();
        depend_inputs.dedup();
        let mf = &self.ctx.interp.makefiles[target.makefile];
        let mut command = String::new();
        command.push_str(&format!(
            "cd {} && ",
            shell_path(self.ctx, &self.ctx.interp.home_binary)
        ));
        let _ = write!(
            command,
            "$(CMAKE_COMMAND) -E cmake_depends \"Unix Makefiles\" {} {} {} {} {}",
            shell_path(self.ctx, &self.ctx.interp.home_source),
            shell_path(self.ctx, &mf.source_dir),
            shell_path(self.ctx, &self.ctx.interp.home_binary),
            shell_path(self.ctx, &mf.binary_dir),
            shell_path(self.ctx, &format!("{support}/DependInfo.cmake")),
        );
        if self.color() {
            command.push_str(" \"--color=$(COLOR)\"");
        }
        write_make_rule(out, None, &dep_target, &depend_inputs, &[command], true);
    }

    fn write_flags(&self, target: &Target) {
        let config = self.ctx.config.clone();
        let mut out = String::from(
            "# CMAKE generated file: DO NOT EDIT!\n# Generated by \"Unix Makefiles\" Generator, CMake Version 4.1\n\n",
        );
        let source_list = gt::sources(self.ctx, target, &config);
        let mut languages_set = std::collections::BTreeSet::new();
        gt::target_languages(self.ctx, target, &config, &source_list, &mut languages_set);
        let languages: Vec<String> = languages_set
            .into_iter()
            .filter(|lang| lang != "None")
            .collect();
        for lang in &languages {
            let compiler = self
                .ctx
                .var_safe(target.makefile, &format!("CMAKE_{lang}_COMPILER"));
            let _ = writeln!(
                out,
                "# compile {lang} with {}",
                shell_path(self.ctx, &compiler)
            );
        }
        for lang in &languages {
            let vars = flags::compile_target_vars(self.ctx, target, &config, lang);
            let _ = writeln!(out, "{lang}_DEFINES = {}\n", vars.defines);
            let mut includes = vars.includes.clone();
            if let Some((full, rel_cur)) =
                makefile_include_response_path(self.ctx, target, lang, &includes)
            {
                let _ =
                    crate::fsutil::write_if_different(Path::new(&full), &format!("{includes}\n"));
                let response_flag = self
                    .ctx
                    .var(target.makefile, &format!("CMAKE_{lang}_RESPONSE_FILE_FLAG"))
                    .filter(|flag| !flag.is_empty())
                    .unwrap_or_else(|| "@".to_owned());
                includes = format!("{response_flag}{rel_cur}");
            }
            let _ = writeln!(out, "{lang}_INCLUDES = {}\n", includes);
            let mut architectures = flags::apple_archs(self.ctx, target, &config, lang);
            architectures.push(String::new());
            for arch in architectures {
                let vars =
                    flags::compile_target_vars_for_arch(self.ctx, target, &config, lang, &arch);
                let _ = writeln!(out, "{lang}_FLAGS{arch} = {}\n", vars.flags_untrimmed);
            }
        }
        let dir = target.makefile;
        let rel_home = home_relative_output_path(self.ctx, dir);
        let support = gt::support_directory(self.ctx, target);
        for source in source_list
            .iter()
            .filter(|source| source.kind == SourceKind::ObjectSource)
        {
            let properties = self.source_props(target, &source.path);
            let rule_flags =
                flags::source_rule_flags(self.ctx, target, &config, source, &properties);
            let obj_rel_cur = format!(
                "{}/{}",
                maybe_relative_cur(self.ctx, dir, &support),
                source.object_name
            );
            let obj_rel_top = format!("{rel_home}{obj_rel_cur}");
            if let Some(value) = rule_flags.custom_flags {
                let _ = writeln!(out, "# Custom flags: {obj_rel_top}_FLAGS = {value}\n");
            }
            if let Some(value) = rule_flags.custom_options {
                let _ = writeln!(out, "# Custom options: {obj_rel_top}_OPTIONS = {value}\n");
            }
            if let Some(value) = rule_flags.pch_options {
                let _ = writeln!(out, "# PCH options: {obj_rel_top}_OPTIONS = {value}\n");
            }
            if let Some(value) = rule_flags.custom_includes {
                let _ = writeln!(
                    out,
                    "# Custom include directories: {obj_rel_top}_INCLUDE_DIRECTORIES = {value}\n"
                );
            }
            if let Some(value) = rule_flags.custom_defines {
                let _ = writeln!(out, "# Custom defines: {obj_rel_top}_DEFINES = {value}\n");
            }
            if let Some(value) = rule_flags.custom_config_defines {
                let upper = self.ctx.config.to_ascii_uppercase();
                let _ = writeln!(
                    out,
                    "# Custom defines: {obj_rel_top}_DEFINES_{upper} = {value}\n"
                );
            }
        }
        let path = format!("{}/flags.make", gt::support_directory(self.ctx, target));
        let _ = crate::fsutil::write_if_different(Path::new(&path), &out);
    }

    fn have_fortran_sources(&self, target: &Target) -> bool {
        fn visit(ctx: &GenCtx<'_>, target: &Target, config: &str, depth: usize) -> bool {
            if depth > 32 {
                return false;
            }
            let sources = gt::sources(ctx, target, config);
            for source in sources.iter() {
                if source.language == "Fortran" {
                    return true;
                }
                if let Some(library) = &source.object_library
                    && let Some(library) = gt::target_in_directory(ctx, target.makefile, library)
                    && visit(ctx, library, config, depth + 1)
                {
                    return true;
                }
            }
            false
        }
        visit(self.ctx, target, &self.ctx.config, 0)
    }

    fn write_depend_info(&self, target: &Target, implicit: &ImplicitDepends) {
        let dir = target.makefile;
        let config = self.ctx.config.clone();
        let project_only = self
            .ctx
            .var(dir, "CMAKE_DEPENDS_IN_PROJECT_ONLY")
            .is_some_and(|v| is_on(&v));
        let mut out = format!(
            "\n# Consider dependencies only in project.\nset(CMAKE_DEPENDS_IN_PROJECT_ONLY {})\n\n# The set of languages for which implicit dependencies are needed:\nset(CMAKE_DEPENDS_LANGUAGES\n",
            if project_only { "ON" } else { "OFF" }
        );
        let has_fortran = self.have_fortran_sources(target);
        let mut fortran_listed = false;
        for lang in implicit.cmake.keys() {
            let _ = writeln!(out, "  \"{lang}\"");
            if lang == "Fortran" {
                fortran_listed = true;
            }
        }
        if has_fortran && !fortran_listed {
            let _ = writeln!(out, "  \"Fortran\"");
        }
        out.push_str("  )\n");

        if !implicit.cmake.is_empty() {
            out.push_str("# The set of files for implicit dependencies of each language:\n");
            for (lang, objects) in &implicit.cmake {
                let _ = writeln!(out, "set(CMAKE_DEPENDS_CHECK_{lang}");
                for (obj, srcs) in objects {
                    for src in srcs {
                        let _ =
                            writeln!(out, "  \"{}\" \"{}\"", escape_cmake(src), escape_cmake(obj));
                    }
                }
                out.push_str("  )\n");
                let cid = self
                    .ctx
                    .var_safe(target.makefile, &format!("CMAKE_{lang}_COMPILER_ID"));
                if !cid.is_empty() {
                    let _ = writeln!(out, "set(CMAKE_{lang}_COMPILER_ID \"{cid}\")");
                }
                if lang == "Fortran" {
                    let sep = self
                        .ctx
                        .var_safe(target.makefile, "CMAKE_Fortran_SUBMODULE_SEP");
                    let ext = self
                        .ctx
                        .var_safe(target.makefile, "CMAKE_Fortran_SUBMODULE_EXT");
                    let _ = writeln!(
                        out,
                        "set(CMAKE_Fortran_SUBMODULE_SEP \"{sep}\")\nset(CMAKE_Fortran_SUBMODULE_EXT \"{ext}\")"
                    );
                }
                let defines = flags::compile_define_items(self.ctx, target, &config, lang);
                if !defines.is_empty() {
                    let _ = writeln!(
                        out,
                        "\n# Preprocessor definitions for this target.\nset(CMAKE_TARGET_DEFINITIONS_{lang}"
                    );
                    for (define, _) in &defines {
                        let _ = writeln!(out, "  \"{}\"", escape_cmake(define));
                    }
                    out.push_str("  )\n");
                }
                let _ = writeln!(
                    out,
                    "\n# The include file search paths:\nset(CMAKE_{lang}_TARGET_INCLUDE_PATH"
                );
                let mut includes: Vec<String> =
                    flags::compile_include_items(self.ctx, target, &config, lang)
                        .into_iter()
                        .map(|(path, _, _)| path)
                        .collect();
                if project_only {
                    let home_source = self.ctx.interp.home_source.clone();
                    let home_binary = self.ctx.interp.home_binary.clone();
                    includes.retain(|path| {
                        crate::util::is_subdirectory(path, &home_source)
                            || crate::util::is_subdirectory(path, &home_binary)
                    });
                }
                for include in includes {
                    let _ = writeln!(
                        out,
                        "  \"{}\"",
                        escape_cmake(&maybe_relative_top(self.ctx, &include))
                    );
                }
                out.push_str("  )\n");
            }
            // `IMPLICIT_DEPENDS_INCLUDE_TRANSFORM` rules, directory first.
            let mut transforms: Vec<String> = Vec::new();
            if let Some(value) = self
                .ctx
                .interp
                .makefiles
                .get(dir)
                .and_then(|mf| mf.props.get("IMPLICIT_DEPENDS_INCLUDE_TRANSFORM"))
            {
                transforms.extend(crate::util::expand_list(value, false));
            }
            if let Some(value) = target.props.get("IMPLICIT_DEPENDS_INCLUDE_TRANSFORM") {
                transforms.extend(crate::util::expand_list(value, false));
            }
            if !transforms.is_empty() {
                out.push_str("\nset(CMAKE_INCLUDE_TRANSFORMS\n");
                for rule in &transforms {
                    let _ = writeln!(out, "  {}", escape_cmake(rule));
                }
                out.push_str("  )\n");
            }
        }

        // The dependency files managed by the compiler, in std::map language
        // order with CUSTOM and LINK as pseudo-languages.
        out.push_str("\n# The set of dependency files which are needed:\nset(CMAKE_DEPENDS_DEPENDENCY_FILES\n");
        let mut rows: std::collections::BTreeMap<String, Vec<String>> =
            std::collections::BTreeMap::new();
        for (lang, objects) in &implicit.compiler {
            if lang == "CUSTOM" {
                continue;
            }
            let format = self
                .ctx
                .var_safe(target.makefile, &format!("CMAKE_{lang}_DEPFILE_FORMAT"));
            let mut lang_rows = Vec::new();
            for (obj, srcs) in objects {
                let obj_rel = maybe_relative_top(self.ctx, obj);
                for src in srcs {
                    lang_rows.push(format!(
                        "  \"{}\" \"{obj_rel}\" \"{}\" \"{obj_rel}.d\"",
                        escape_cmake(src),
                        escape_cmake(&format),
                    ));
                }
            }
            rows.insert(lang.clone(), lang_rows);
        }
        if let Some(objects) = implicit.compiler.get("CUSTOM") {
            let mut custom_rows = Vec::new();
            for (obj, srcs) in objects {
                let obj_rel = maybe_relative_top(self.ctx, obj);
                for src in srcs {
                    custom_rows.push(format!(
                        "  \"\" \"{obj_rel}\" \"custom\" \"{}\"",
                        escape_cmake(&maybe_relative_top(self.ctx, src))
                    ));
                }
            }
            rows.insert("CUSTOM".into(), custom_rows);
        }
        let language = gt::linker_language(self.ctx, target, &config);
        if let Some(depfile) = flags::link_dependency_file(self.ctx, target, &language) {
            let dep_full = if util::is_full_path(&depfile) {
                depfile.clone()
            } else {
                format!("{}/{depfile}", self.ctx.interp.makefiles[dir].binary_dir)
            };
            let dep_rel = maybe_relative_top(self.ctx, &dep_full);
            let output = maybe_relative_top(
                self.ctx,
                &gt::full_path(self.ctx, target, &config, ArtifactType::Runtime, false),
            );
            let format = self.ctx.var_safe(
                target.makefile,
                &format!("CMAKE_{language}_LINKER_DEPFILE_FORMAT"),
            );
            rows.insert(
                "LINK".into(),
                vec![format!("  \"\" \"{output}\" \"{format}\" \"{dep_rel}\"")],
            );
        }
        // CUSTOM is stored inside the compiler map; rows are already ordered
        // by language key through the BTreeMap.
        for lang_rows in rows.values() {
            for row in lang_rows {
                out.push_str(row);
                out.push('\n');
            }
        }
        out.push_str("  )\n");

        let names = gt::names(self.ctx, target, &config);
        let mut output_pairs: std::collections::BTreeSet<(String, String)> =
            implicit.output_pairs.iter().cloned().collect();
        if names.output != names.real
            && (target.kind == TargetKind::SharedLibrary
                || target.kind == TargetKind::ModuleLibrary)
        {
            let directory = gt::directory(self.ctx, target, &config, ArtifactType::Runtime);
            let real = format!("{directory}/{}", names.real);
            for name in [&names.output, &names.shared_object] {
                if !name.is_empty() && *name != names.real {
                    output_pairs.insert((format!("{directory}/{name}"), real.clone()));
                }
            }
        }
        if !output_pairs.is_empty() {
            out.push_str("\n# Pairs of files generated by the same build rule.\nset(CMAKE_MULTIPLE_OUTPUT_PAIRS\n");
            for (extra, first) in output_pairs {
                let _ = writeln!(
                    out,
                    "  \"{}\" \"{}\"",
                    escape_cmake(&extra),
                    escape_cmake(&first)
                );
            }
            out.push_str("  )\n\n");
        }
        let mut linked = String::new();
        let mut forward_linked = String::new();
        for (dep, forward) in self.fortran_linked_target_dirs(target) {
            let entry = format!("  \"{dep}/DependInfo.cmake\"\n");
            if forward {
                forward_linked.push_str(&entry);
            }
            linked.push_str(&entry);
        }
        out.push_str("\n# Targets to which this target links which contain Fortran sources.\nset(CMAKE_Fortran_TARGET_LINKED_INFO_FILES\n");
        out.push_str(&linked);
        out.push_str("  )\n\n# Targets to which this target links which contain Fortran sources.\nset(CMAKE_Fortran_TARGET_FORWARD_LINKED_INFO_FILES\n");
        out.push_str(&forward_linked);
        out.push_str("  )\n");
        let module_dir = flags::fortran_module_dir(self.ctx, target).unwrap_or_default();
        let _ = writeln!(
            out,
            "\n# Fortran module output directory.\nset(CMAKE_Fortran_TARGET_MODULE_DIR \"{module_dir}\")"
        );
        let path = format!(
            "{}/DependInfo.cmake",
            gt::support_directory(self.ctx, target)
        );
        let _ = crate::fsutil::write_if_different(Path::new(&path), &out);
    }

    /// `cmCommonTargetGenerator::GetLinkedTargetDirectories` for Fortran:
    /// non-imported linked targets with Fortran sources that build before
    /// this target, as (support-directory, forward) entries in link order.
    fn fortran_linked_target_dirs(&self, target: &Target) -> Vec<(String, bool)> {
        let language = gt::linker_language(self.ctx, target, &self.ctx.config);
        let _ = super::link::link_information(self.ctx, target, &self.ctx.config, &language);
        if !self
            .ctx
            .interp
            .langs
            .enabled
            .iter()
            .any(|lang| lang == "Fortran")
        {
            return Vec::new();
        }
        let mut result: Vec<(String, bool)> = Vec::new();
        let mut seen = std::collections::HashSet::new();
        let source_object_libraries: std::collections::HashSet<String> =
            gt::sources(self.ctx, target, &self.ctx.config)
                .iter()
                .filter_map(|source| source.object_library.clone())
                .collect();
        for depend in super::link::target_depends(self.ctx, target).iter() {
            if !depend.link {
                continue;
            }
            let Some(dep) = self.ctx.interp.ext.targets.get(&depend.key) else {
                continue;
            };
            if dep.imported || dep.kind == TargetKind::InterfaceLibrary {
                continue;
            }
            let forward = source_object_libraries.contains(&dep.name);
            if self.have_fortran_sources(dep) && seen.insert(dep.name.clone()) {
                result.push((gt::support_directory(self.ctx, dep), forward));
            }
        }
        for library in source_object_libraries {
            if let Some(dep) = gt::target_in_directory(self.ctx, target.makefile, &library)
                && !dep.imported
                && dep.kind != TargetKind::InterfaceLibrary
                && self.have_fortran_sources(dep)
                && seen.insert(dep.name.clone())
            {
                result.push((gt::support_directory(self.ctx, dep), true));
            }
        }
        result
    }

    fn write_dependency_placeholders(&self, support: &str, compiler: bool, target_name: &str) {
        let depend = &format!(
            "# Empty dependencies file for {target_name}.\n# This may be replaced when dependencies are built.\n"
        );
        let _ =
            crate::fsutil::write_if_different(Path::new(&format!("{support}/depend.make")), depend);
        if compiler {
            let compiler_make = &format!(
                "# Empty compiler generated dependencies file for {target_name}.\n# This may be replaced when dependencies are built.\n"
            );
            let _ = crate::fsutil::write_if_different(
                Path::new(&format!("{support}/compiler_depend.make")),
                compiler_make,
            );
            let ts = &format!(
                "# CMAKE generated file: DO NOT EDIT!\n# Timestamp file for compiler generated dependencies management for {target_name}.\n"
            );
            let _ = crate::fsutil::write_if_different(
                Path::new(&format!("{support}/compiler_depend.ts")),
                ts,
            );
        }
    }

    fn write_clean_files(
        &self,
        target: &Target,
        objects: &[String],
        custom_clean: &[String],
        extra_clean: &[String],
    ) {
        let config = self.ctx.config.clone();
        let dir = target.makefile;
        let mf = &self.ctx.interp.makefiles[dir];
        let names = gt::names(self.ctx, target, &config);
        let directory = if matches!(
            target.kind,
            TargetKind::Utility | TargetKind::ObjectLibrary | TargetKind::InterfaceLibrary
        ) {
            String::new()
        } else {
            gt::directory(self.ctx, target, &config, ArtifactType::Runtime)
        };
        let mut clean_set = std::collections::BTreeSet::new();

        // Legacy directory property (cmMakefileTargetGenerator::WriteTargetBuildRules).
        if let Some(value) = mf.props.get("ADDITIONAL_MAKE_CLEAN_FILES") {
            let evaluated = self.ctx.genex(value, Some(target), None);
            for file in util::expand_list(&evaluated, false) {
                clean_set.insert(file);
            }
        }
        // Target clean files are collapsed against the current binary directory
        // before entering cmMakefileTargetGenerator::CleanFiles.  Keeping the
        // absolute form here matters for the byte order of the final set.
        if let Some(value) = target.props.get("ADDITIONAL_CLEAN_FILES") {
            let evaluated = self.ctx.genex(value, Some(target), None);
            for file in util::expand_list(&evaluated, false) {
                clean_set.insert(util::collapse_full_path(&file, &mf.binary_dir));
            }
        }

        let language = gt::linker_language(self.ctx, target, &config);
        if flags::link_dependency_file(self.ctx, target, &language).is_some() {
            // WriteTargetLinkDependRules stores the full path in CleanFiles;
            // conversion to the current binary directory happens only when
            // the clean script is written.  The full path also affects the
            // byte ordering of the sorted clean set.
            clean_set.insert(format!(
                "{}/link.d",
                gt::support_directory(self.ctx, target)
            ));
        }
        if super::link::requires_device_linking(self.ctx, target, &config, &language)
            && self.ctx.var_safe(target.makefile, "CMAKE_CUDA_COMPILER_ID") != "Clang"
        {
            let object_extension = self
                .ctx
                .var_safe(target.makefile, "CMAKE_CUDA_OUTPUT_EXTENSION");
            clean_set.insert(format!(
                "{}/cmake_device_link{object_extension}",
                maybe_relative_cur(self.ctx, dir, &gt::support_directory(self.ctx, target))
            ));
        }
        let clean_sources = gt::sources(self.ctx, target, &config);
        let object_sources: Vec<&gt::SourceInfo> = clean_sources
            .iter()
            .filter(|source| source.kind == SourceKind::ObjectSource)
            .collect();
        for (index, object) in objects.iter().enumerate() {
            clean_set.insert(object.clone());
            if let Some(source) = object_sources.get(index)
                && self.use_compiler_depends(target, &source.language)
                && let Some((depfile, _)) = self.depfile_for_source(target, source, object)
            {
                clean_set.insert(depfile);
            }
        }

        // Custom command outputs and byproducts are cleaned unless the directory
        // opts out with CLEAN_NO_CUSTOM.
        for file in custom_clean {
            clean_set.insert(file.clone());
        }
        for file in extra_clean {
            clean_set.insert(file.clone());
        }

        if !mf.props.get("CLEAN_NO_CUSTOM").is_some_and(is_on) {
            for command in target.custom_commands.iter().filter(|command| {
                matches!(
                    command.kind,
                    CustomCommandKind::PreBuild
                        | CustomCommandKind::PreLink
                        | CustomCommandKind::PostBuild
                )
            }) {
                let spec = custom::spec_from_command(self.ctx, command);
                let evaluated =
                    custom::evaluate_spec_cfg(self.ctx, &spec, &self.ctx.config, &self.ctx.config);
                for file in evaluated.byproducts.iter() {
                    clean_set.insert(maybe_relative_cur(self.ctx, dir, file));
                }
            }
        }

        let mut artifacts = Vec::new();
        if target.kind == TargetKind::Utility {
            // Utility targets have no linked artifact.  Targets with a utility
            // command get a nominal symbolic output at CMakeFiles/<target>.
            if crate::commands::custom_actions::utility_command(self.ctx.interp, &target.name)
                .is_some()
            {
                artifacts.push(format!("{}/CMakeFiles/{}", mf.binary_dir, target.name));
            }
        } else if !matches!(
            target.kind,
            TargetKind::ObjectLibrary | TargetKind::InterfaceLibrary
        ) {
            artifacts.push(format!("{directory}/{}", names.pdb));
            artifacts.push(gt::full_path(
                self.ctx,
                target,
                &config,
                ArtifactType::Runtime,
                false,
            ));
            if !names.shared_object.is_empty() {
                artifacts.push(format!("{directory}/{}", names.shared_object));
            }
            artifacts.push(gt::full_path(
                self.ctx,
                target,
                &config,
                ArtifactType::Runtime,
                true,
            ));
            if !names.import_library.is_empty() {
                let import_directory =
                    gt::directory(self.ctx, target, &config, ArtifactType::Import);
                artifacts.push(format!("{import_directory}/{}", names.import_library));
            }
        }
        for artifact in artifacts {
            clean_set.insert(maybe_relative_cur(self.ctx, dir, &artifact));
        }

        let mut files = String::new();
        if !clean_set.is_empty() {
            files.push_str("file(REMOVE_RECURSE\n");
            for file in clean_set {
                let converted = if util::is_full_path(&file) {
                    maybe_relative_cur_preserve_slashes(self.ctx, dir, &file)
                } else {
                    file
                };
                let _ = writeln!(
                    files,
                    "  \"{}\"",
                    escape_cmake(&converted.replace('\\', "/"))
                );
            }
            files.push_str(")\n");
        }
        let found = gt::sources(self.ctx, target, &config);
        let mut langset = std::collections::BTreeSet::new();
        gt::target_languages(self.ctx, target, &config, &found, &mut langset);
        let langs: Vec<String> = langset.into_iter().collect();
        let support = gt::support_directory(self.ctx, target);
        let support_rel_cur = maybe_relative_cur(self.ctx, dir, &support);
        let _ = writeln!(
            files,
            "\n# Per-language clean rules from dependency scanning.\nforeach(lang {})\n  include({support_rel_cur}/cmake_clean_${{lang}}.cmake OPTIONAL)\nendforeach()",
            langs.join(" ")
        );
        let _ = crate::fsutil::write_if_different(
            Path::new(&format!("{support}/cmake_clean.cmake")),
            &files,
        );
        if target.kind == TargetKind::StaticLibrary {
            let real = maybe_relative_cur(self.ctx, dir, &format!("{directory}/{}", names.real));
            let target_clean = format!(
                "file(REMOVE_RECURSE\n  \"{}\"\n)\n",
                crate::cache::convert_to_unix_slashes(&real)
            );
            let _ = crate::fsutil::write_if_different(
                Path::new(&format!("{support}/cmake_clean_target.cmake")),
                &target_clean,
            );
        }
    }

    fn write_utility_target(&mut self, target: &Target, target_key: &str) {
        let dir = target.makefile;
        let support = gt::support_directory(self.ctx, target);
        let support_rel_cur = maybe_relative_cur(self.ctx, dir, &support);
        let support_rel_top = format!(
            "{}{}",
            home_relative_output_path(self.ctx, dir),
            support_rel_cur
        );
        let mut build = build_file_header("");
        apply_verbose_default(self.ctx, &mut build);
        build = build
            .replace(
                "@CMAKE_COMMAND@",
                &shell_path(self.ctx, &self.ctx.interp.cmake_command),
            )
            .replace(
                "@CMAKE_SOURCE_DIR@",
                &shell_path(self.ctx, &self.ctx.interp.home_source),
            )
            .replace(
                "@CMAKE_BINARY_DIR@",
                &shell_path(self.ctx, &self.ctx.interp.home_binary),
            );
        apply_instrumentation_variable(self.ctx, &mut build);
        let _ = writeln!(build, "# Utility rule file for {}.\n", target.name);
        let _ = writeln!(
            build,
            "# Include any custom commands dependencies for this target.\ninclude {}\n\n# Include the progress variables for this target.\ninclude {}\n",
            makefile_path(&format!("{support_rel_top}/compiler_depend.make")),
            makefile_path(&format!("{support_rel_top}/progress.make"))
        );
        let mut custom_outputs = Vec::new();
        let mut implicit = ImplicitDepends::default();
        let (codegen_outputs, custom_clean) = self.write_custom_commands(
            target,
            target_key,
            dir,
            &support_rel_cur,
            &support_rel_top,
            &mut build,
            &mut custom_outputs,
            &mut implicit,
        );
        custom_outputs.sort();
        custom_outputs.dedup();
        write_make_rule(
            &mut build,
            None,
            &format!("{support_rel_top}/codegen"),
            &codegen_outputs
                .iter()
                .map(|output| self.top_rule_path(output))
                .collect::<Vec<_>>(),
            &[],
            true,
        );
        let mut utility_depends = Vec::new();
        let mut utility_commands = Vec::new();
        for stage in [CustomCommandKind::PreBuild, CustomCommandKind::PostBuild] {
            for command in target
                .custom_commands
                .iter()
                .filter(|command| command.kind == stage)
            {
                let spec = custom::spec_from_command(self.ctx, command);
                let evaluated =
                    custom::evaluate_spec_cfg(self.ctx, &spec, &self.ctx.config, &self.ctx.config);
                utility_depends.extend(evaluated.depends.iter().map(|dep| self.top_rule_path(dep)));
                if let Some(comment) = &evaluated.comment
                    && !comment.is_empty()
                {
                    utility_commands
                        .push(format!("@{}", self.echo(comment, "--blue --bold", None)));
                }
                utility_commands.extend(self.custom_command_lines(
                    &evaluated,
                    target,
                    &self.ctx.interp.makefiles[dir].binary_dir,
                ));
            }
        }
        utility_depends.extend(
            custom_outputs
                .iter()
                .map(|output| self.top_rule_path(output)),
        );
        utility_depends.push(format!("{support_rel_top}/build.make"));
        write_make_rule(
            &mut build,
            None,
            &target.name,
            &utility_depends,
            &utility_commands,
            true,
        );
        write_make_rule(
            &mut build,
            Some("Rule to build all files generated by this target."),
            &format!("{support_rel_top}/build"),
            std::slice::from_ref(&target.name),
            &[],
            true,
        );
        self.write_clean_and_depend(
            target,
            &support,
            &support_rel_cur,
            &support_rel_top,
            &[],
            &mut build,
        );
        let _ =
            crate::fsutil::write_if_different(Path::new(&format!("{support}/build.make")), &build);

        // Utility targets use dependency scanning only for their custom
        // commands.  The DependInfo file is intentionally empty of languages.
        self.write_depend_info(target, &implicit);
        let depend = "# Empty custom commands generated dependencies file for ".to_string()
            + &target.name
            + ".\n# This may be replaced when dependencies are built.\n";
        let _ = crate::fsutil::write_if_different(
            Path::new(&format!("{support}/compiler_depend.make")),
            &depend,
        );
        let ts = "# CMAKE generated file: DO NOT EDIT!\n# Timestamp file for custom commands dependencies management for ".to_string()
            + &target.name
            + ".\n";
        let _ = crate::fsutil::write_if_different(
            Path::new(&format!("{support}/compiler_depend.ts")),
            &ts,
        );
        let _ =
            crate::fsutil::write_if_different(Path::new(&format!("{support}/progress.make")), "\n");
        self.write_clean_files(target, &[], &custom_clean, &[]);
    }

    /// `cmGlobalGenerator::CheckRuleHashes` and `WriteRuleHashes`.
    fn check_rule_hashes(&mut self) {
        let home = self.ctx.interp.home_binary.clone();
        let path = format!("{home}/CMakeFiles/CMakeRuleHashes.txt");
        if let Ok(existing) = std::fs::read_to_string(&path) {
            for line in existing.lines() {
                if line.len() < 34 || line.starts_with('#') {
                    continue;
                }
                let (hash, file) = line.split_at(32);
                let file = &file[1..];
                if let Some(current) = self.rule_hashes.get(file) {
                    if current != hash {
                        let _ = std::fs::remove_file(util::collapse_full_path(file, &home));
                    }
                } else {
                    let full = util::collapse_full_path(file, &home);
                    if Path::new(&full).exists() {
                        self.rule_hashes
                            .entry(file.to_owned())
                            .or_insert_with(|| hash.to_owned());
                    }
                }
            }
        }
        if self.rule_hashes.is_empty() {
            let _ = std::fs::remove_file(&path);
            return;
        }
        let mut out = String::from(
            "# Hashes of file build rules.
",
        );
        for (file, hash) in &self.rule_hashes {
            let _ = writeln!(out, "{hash} {file}");
        }
        let _ = crate::fsutil::write_generated(Path::new(&path), &out);
    }

    fn write_progress_marks(&mut self) {
        let total: usize = self.progress.iter().map(|(_, p)| p.total).sum();
        let mut ordered: Vec<(String, String, usize)> = self
            .progress
            .iter()
            .enumerate()
            .filter_map(|(index, (name, _))| {
                self.ctx.target(name).map(|target| {
                    (
                        name.clone(),
                        self.ctx.interp.makefiles[target.makefile]
                            .binary_dir
                            .clone(),
                        index,
                    )
                })
            })
            .collect();
        ordered.sort();
        let mut current = 0usize;
        let total_actions = total;
        for (_, _, index) in ordered {
            let total_actions_for_target = self.progress[index].1.total;
            let Some((name, _)) = self.progress.get(index) else {
                continue;
            };
            let Some(target) = self.ctx.target(name) else {
                continue;
            };
            let support = gt::support_directory(self.ctx, target);
            let mut text = String::new();
            let mut marks = Vec::new();
            for action in 1..=total_actions_for_target {
                let before = ((action - 1 + current) * 100) / total_actions.max(1);
                let absolute = action + current;
                let after = (absolute * 100) / total_actions.max(1);
                text.push_str(&format!("CMAKE_PROGRESS_{action} = "));
                if total_actions <= 100 {
                    text.push_str(&absolute.to_string());
                    marks.push(absolute);
                } else if after > before {
                    text.push_str(&after.to_string());
                    marks.push(after);
                }
                text.push('\n');
            }
            text.push('\n');
            current += total_actions_for_target;
            self.progress[index].1.marks = marks;
            let _ = crate::fsutil::write_if_different(
                Path::new(&format!("{support}/progress.make")),
                &text,
            );
        }

        // `cmGlobalUnixMakefileGenerator3::InitializeProgressMarks`: first
        // seed each directory and its parents with targets belonging to the
        // seeding directory, together with those targets' direct
        // dependencies.  Exclusion for this phase is relative to the
        // target's own directory.
        let count = self.ctx.interp.makefiles.len();
        let mut directory_targets_map: Vec<Vec<String>> = vec![Vec::new(); count];
        let mut directory_target_sets: Vec<HashSet<String>> = vec![HashSet::new(); count];
        for index in 0..count {
            for key in self.ctx.interp.makefiles[index].targets.clone() {
                let Some(target) = self.ctx.interp.ext.targets.get(&key) else {
                    continue;
                };
                if target.kind == TargetKind::GlobalTarget || self.target_is_excluded(index, target)
                {
                    continue;
                }
                let depends = super::link::target_depends(self.ctx, target)
                    .iter()
                    .map(|depend| depend.key.clone())
                    .collect::<Vec<_>>();
                let mut at = Some(index);
                while let Some(dir) = at {
                    if directory_target_sets[dir].insert(key.clone()) {
                        directory_targets_map[dir].push(key.clone());
                    }
                    for depend in &depends {
                        if directory_target_sets[dir].insert(depend.clone()) {
                            directory_targets_map[dir].push(depend.clone());
                        }
                    }
                    at = self.ctx.interp.makefiles[dir].parent;
                }
            }
        }

        // `cmGlobalUnixMakefileGenerator3::CountProgressMarksInAll`: count
        // seeds that are not excluded relative to the requested directory,
        // then count their dependencies recursively without further
        // exclusion filtering.
        for (dir, targetset) in directory_targets_map.iter().enumerate() {
            let mut emitted = std::collections::HashSet::new();
            let mut count = 0usize;
            for key in targetset {
                if let Some(target) = self.ctx.interp.ext.targets.get(key)
                    && !self.target_is_excluded(dir, target)
                {
                    count += self.count_progress_marks_in_target(target, &mut emitted);
                }
            }
            let path = if dir == 0 {
                format!("{}/CMakeFiles/progress.marks", self.ctx.interp.home_binary)
            } else {
                let binary = &self.ctx.interp.makefiles[dir].binary_dir;
                let _ = std::fs::create_dir_all(format!("{binary}/CMakeFiles"));
                format!("{binary}/CMakeFiles/progress.marks")
            };
            let _ = crate::fsutil::write_if_different(Path::new(&path), &format!("{count}\n"));
        }
    }

    /// `cmGlobalGenerator::IsExcludedFromAllInConfig` for the
    /// single-configuration Makefile generator.  An explicit target
    /// property overrides exclusion inherited from its directory.
    fn target_is_excluded(&self, root: usize, target: &Target) -> bool {
        if !is_in_build_system(target) {
            return true;
        }
        if let Some(value) = target.props.get("EXCLUDE_FROM_ALL") {
            let value = self.ctx.genex(value, Some(target), None);
            return is_on(&value);
        }
        self.directory_is_excluded(root, target.makefile)
    }

    /// `cmGlobalGenerator::IsDirectoryExcludedFromAll`: exclusion is
    /// relative to the root directory, so the root never excludes itself.
    fn directory_is_excluded(&self, root: usize, dir: usize) -> bool {
        let mut at = Some(dir);
        while let Some(cur) = at {
            if cur == root {
                return false;
            }
            if self.ctx.interp.makefiles[cur]
                .props
                .get("EXCLUDE_FROM_ALL")
                .is_some_and(is_on)
            {
                return true;
            }
            at = self.ctx.interp.makefiles[cur].parent;
        }
        false
    }

    fn progress_numbers(&self, name: &str) -> Vec<usize> {
        self.progress
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, p)| p.marks.clone())
            .unwrap_or_default()
    }

    fn count_progress_marks_in_target(
        &self,
        target: &Target,
        emitted: &mut std::collections::HashSet<String>,
    ) -> usize {
        if !emitted.insert(target.name.clone()) {
            return 0;
        }
        let mut count = self
            .progress
            .iter()
            .find(|(n, _)| n == &target.name)
            .map(|(_, p)| p.marks.len())
            .unwrap_or(0);
        for depend in super::link::target_depends(self.ctx, target).iter() {
            if let Some(dep) = self.ctx.interp.ext.targets.get(&depend.key)
                && is_in_build_system(dep)
            {
                count += self.count_progress_marks_in_target(dep, emitted);
            }
        }
        count
    }

    fn progress_count_in_target(&self, target: &Target) -> usize {
        self.count_progress_marks_in_target(target, &mut std::collections::HashSet::new())
    }

    /// `cmGlobalGenerator::WriteSummary(target)`: Labels.txt/Labels.json in
    /// the target's support directory when any labels apply.
    fn write_labels(&self, target: &Target, directory: &str) {
        let interp = self.ctx.interp;
        let mut seen = HashSet::new();
        let sources = self
            .ctx
            .configs
            .iter()
            .flat_map(|config| {
                gt::sources(self.ctx, target, config)
                    .iter()
                    .cloned()
                    .collect::<Vec<_>>()
            })
            .filter(|source| seen.insert(source.path.clone()))
            .map(|source| {
                let labels = crate::commands::target::find_known_source(
                    interp,
                    target.makefile,
                    &source.path,
                )
                .and_then(|index| {
                    interp.ext.sources[&target.makefile][index]
                        .props
                        .get("LABELS")
                        .map(str::to_owned)
                });
                (source.path.clone(), labels)
            })
            .collect();
        self.write_labels_files(
            &target.name,
            target.props.get("LABELS"),
            target.makefile,
            sources,
            directory,
        );
    }

    /// The shared body of `WriteSummary(target)`.  Default global targets
    /// have no LABELS property or sources, but still get the files when
    /// their directory has labels.
    fn write_labels_files(
        &self,
        name: &str,
        target_labels: Option<&str>,
        makefile: usize,
        sources: Vec<(String, Option<String>)>,
        directory: &str,
    ) {
        use crate::commands::string::json::{Json, write_json_ostream};
        let interp = self.ctx.interp;
        let file = format!("{directory}/Labels.txt");
        let json_file = format!("{directory}/Labels.json");
        let directory_labels = interp.makefiles[makefile].props.get("LABELS");
        let cmake_directory_labels = self.ctx.var(makefile, "CMAKE_DIRECTORY_LABELS");
        if target_labels.is_none() && directory_labels.is_none() && cmake_directory_labels.is_none()
        {
            let _ = std::fs::remove_file(&file);
            let _ = std::fs::remove_file(&json_file);
            return;
        }
        let text = |value: &str| Json::Str(crate::bytes::encode(value).into_owned());
        let mut out = String::new();
        let mut all_labels = Vec::new();
        let labels = util::expand_list(target_labels.unwrap_or_default(), false);
        if !labels.is_empty() {
            out.push_str("# Target labels\n");
            for label in labels {
                out.push_str(&format!(" {label}\n"));
                all_labels.push(text(&label));
            }
        }
        let directory_list = util::expand_list(directory_labels.unwrap_or_default(), false);
        let cmake_list =
            util::expand_list(cmake_directory_labels.as_deref().unwrap_or_default(), false);
        if !directory_list.is_empty() || !cmake_list.is_empty() {
            out.push_str("# Directory labels\n");
        }
        for label in directory_list.iter().chain(&cmake_list) {
            out.push_str(&format!(" {label}\n"));
            all_labels.push(text(label));
        }
        out.push_str("# Source files and their labels\n");
        let mut source_json = Vec::new();
        for (path, labels) in sources {
            out.push_str(&path);
            out.push('\n');
            let mut entry = BTreeMap::new();
            entry.insert(b"file".to_vec(), text(&path));
            if let Some(labels) = labels {
                let mut list = Vec::new();
                for label in util::expand_list(&labels, false) {
                    out.push_str(&format!(" {label}\n"));
                    list.push(text(&label));
                }
                entry.insert(b"labels".to_vec(), Json::Array(list));
            }
            source_json.push(Json::Object(entry));
        }
        let mut target_json = BTreeMap::new();
        target_json.insert(b"labels".to_vec(), Json::Array(all_labels));
        target_json.insert(b"name".to_vec(), text(name));
        let mut root = BTreeMap::new();
        root.insert(b"sources".to_vec(), Json::Array(source_json));
        root.insert(b"target".to_vec(), Json::Object(target_json));
        let _ = std::fs::create_dir_all(directory);
        let _ = crate::fsutil::write_generated(Path::new(&file), &out);
        let _ = crate::fsutil::write_generated(
            Path::new(&json_file),
            &write_json_ostream(&Json::Object(root)),
        );
    }

    fn write_target_directories(&self) {
        let mut out = String::new();
        let testing = self
            .ctx
            .var(0, "CMAKE_TESTING_ENABLED")
            .is_some_and(|v| crate::util::is_on(&v));
        let install_enabled = self
            .ctx
            .interp
            .ext
            .misc
            .contains_key("install-target-enabled")
            && !self
                .ctx
                .var(0, "CMAKE_SKIP_INSTALL_RULES")
                .is_some_and(|v| crate::util::is_on(&v));
        let (before_globals, after_globals) = self.ctx.targets_around_globals();
        for (mf_index, mf) in self.ctx.interp.makefiles.iter().enumerate() {
            for targets in [&before_globals, &after_globals] {
                for target in targets.iter().filter(|target| target.makefile == mf_index) {
                    if is_in_build_system(target)
                        && matches!(
                            target.kind,
                            TargetKind::Executable
                                | TargetKind::StaticLibrary
                                | TargetKind::SharedLibrary
                                | TargetKind::ModuleLibrary
                                | TargetKind::ObjectLibrary
                                | TargetKind::InterfaceLibrary
                                | TargetKind::Utility
                        )
                    {
                        let directory = gt::support_directory(self.ctx, target);
                        self.write_labels(target, &directory);
                        let _ = writeln!(out, "{directory}");
                    }
                }
                if std::ptr::eq(targets, &before_globals) {
                    let mut names = vec![];
                    let (package, package_source) = package_configs(self.ctx);
                    if package {
                        names.push("package");
                    }
                    if package_source {
                        names.push("package_source");
                    }
                    if testing {
                        names.push("test");
                    }
                    names.extend(["edit_cache", "rebuild_cache"]);
                    if install_enabled {
                        names.extend(["list_install_components", "install", "install/local"]);
                        if install_strip_enabled(self.ctx) {
                            names.push("install/strip");
                        }
                    }
                    for name in names {
                        let directory = format!("{}/CMakeFiles/{name}.dir", mf.binary_dir);
                        self.write_labels_files(name, None, mf_index, Vec::new(), &directory);
                        let _ = writeln!(out, "{directory}");
                    }
                }
            }
        }
        let path = format!(
            "{}/CMakeFiles/TargetDirectories.txt",
            self.ctx.interp.home_binary
        );
        let _ = crate::fsutil::write_if_different(Path::new(&path), &out);
    }

    fn write_main_cmakefile(&self) {
        if !self.regenerate() {
            return;
        }
        let mut out = String::from(
            "# CMAKE generated file: DO NOT EDIT!\n# Generated by \"Unix Makefiles\" Generator, CMake Version 4.1\n\n# The generator used is:\nset(CMAKE_DEPENDS_GENERATOR \"Unix Makefiles\")\n\n# The top level Makefile was generated from the following files:\nset(CMAKE_MAKEFILE_DEPENDS\n  \"CMakeCache.txt\"\n",
        );
        let mut lfiles = Vec::new();
        for mf in self.ctx.interp.makefiles.iter() {
            lfiles.extend(mf.list_files.iter().cloned());
        }
        if let Some(extra) = self.ctx.interp.ext.misc.get("cmake-depend-files") {
            lfiles.extend(crate::util::expand_list(extra, false));
        }
        lfiles.extend(
            self.ctx
                .cmake_depend_files
                .borrow()
                .iter()
                .map(|(_, file)| file.clone()),
        );
        if let Some((script, stamp)) = &self.ctx.interp.ext.glob_verify {
            lfiles.push(script.clone());
            lfiles.push(stamp.clone());
        }
        lfiles.sort();
        lfiles.dedup();
        for file in lfiles {
            let _ = writeln!(out, "  \"{}\"", maybe_relative_top(self.ctx, &file));
        }
        out.push_str("  )\n\n# The corresponding makefile is:\nset(CMAKE_MAKEFILE_OUTPUTS\n  \"Makefile\"\n  \"CMakeFiles/cmake.check_cache\"\n  )\n\n# Byproducts of CMake generate step:\nset(CMAKE_MAKEFILE_PRODUCTS\n");
        for (dir, mf) in self.ctx.interp.makefiles.iter().enumerate() {
            for file in &mf.output_files {
                let _ = writeln!(out, "  \"{}\"", maybe_relative_top(self.ctx, file));
            }
            for (output_dir, file) in self.ctx.cmake_output_files.borrow().iter() {
                if *output_dir == dir {
                    let _ = writeln!(out, "  \"{}\"", maybe_relative_top(self.ctx, file));
                }
            }
            let info = dir_support(self.ctx, dir);
            let _ = writeln!(out, "  \"{}\"", maybe_relative_top(self.ctx, &info));
        }
        out.push_str(
            "  )\n\n# Dependency information for all targets:\nset(CMAKE_DEPEND_INFO_FILES\n",
        );
        for dir in self.ctx.interp.makefiles.iter() {
            for key in &dir.targets {
                if let Some(target) = self.ctx.interp.ext.targets.get(key)
                    && is_in_build_system(target)
                    && matches!(
                        target.kind,
                        TargetKind::Executable
                            | TargetKind::StaticLibrary
                            | TargetKind::SharedLibrary
                            | TargetKind::ModuleLibrary
                            | TargetKind::ObjectLibrary
                            | TargetKind::InterfaceLibrary
                            | TargetKind::Utility
                    )
                {
                    let support = gt::support_directory(self.ctx, target);
                    let _ = writeln!(
                        out,
                        "  \"{}\"",
                        maybe_relative_top(self.ctx, &format!("{support}/DependInfo.cmake"))
                    );
                }
            }
        }
        out.push_str("  )\n");
        let path = format!("{}/CMakeFiles/Makefile.cmake", self.ctx.interp.home_binary);
        let _ = crate::fsutil::write_generated(Path::new(&path), &out);
        let check = "# This file is generated by cmake for dependency checking of the CMakeCache.txt file\n";
        let _ = crate::fsutil::write_if_different(
            Path::new(&format!(
                "{}/CMakeFiles/cmake.check_cache",
                self.ctx.interp.home_binary
            )),
            check,
        );
    }

    fn write_directory_clean_script(&self, dir: usize) -> Option<String> {
        let mf = &self.ctx.interp.makefiles[dir];
        let value = mf.props.get("ADDITIONAL_CLEAN_FILES")?;
        let evaluated = self.ctx.genex_full(value, dir, None, None, true, &[]);
        let files = util::expand_list(&evaluated, false);
        if files.is_empty() {
            return None;
        }
        let cleanfile = format!("{}/CMakeFiles/cmake_directory_clean.cmake", mf.binary_dir);
        let mut out = String::from("file(REMOVE_RECURSE\n");
        for file in files {
            let collapsed = util::collapse_full_path(&file, &mf.binary_dir);
            let relative = maybe_relative_top(self.ctx, &collapsed);
            let _ = writeln!(out, "  \"{}\"", escape_cmake(&relative));
        }
        out.push_str(")\n");
        let _ = crate::fsutil::write_if_different(Path::new(&cleanfile), &out);
        Some(format!(
            "$(CMAKE_COMMAND) -P {}",
            shell_path(self.ctx, &maybe_relative_top(self.ctx, &cleanfile))
        ))
    }

    fn write_main_makefile2(&self) {
        let regenerate = self.regenerate();
        let mut out = build_file_header("");
        apply_verbose_default(self.ctx, &mut out);
        out = out
            .replace("# Delete rule output on recipe failure.\n.DELETE_ON_ERROR:\n\n", "# Default target executed when no arguments are given to make.\ndefault_target: all\n.PHONY : default_target\n\n")
            .replace("@CMAKE_COMMAND@", &shell_path(self.ctx, &self.ctx.interp.cmake_command))
            .replace("@CMAKE_SOURCE_DIR@", &shell_path(self.ctx, &self.ctx.interp.home_source))
            .replace("@CMAKE_BINARY_DIR@", &shell_path(self.ctx, &self.ctx.interp.home_binary));
        apply_instrumentation_variable(self.ctx, &mut out);
        let mut targets = Vec::new();
        for mf in self.ctx.interp.makefiles.iter() {
            for key in &mf.targets {
                if let Some(t) = self.ctx.interp.ext.targets.get(key) {
                    if is_in_build_system(t)
                        && matches!(
                            t.kind,
                            TargetKind::Executable
                                | TargetKind::StaticLibrary
                                | TargetKind::SharedLibrary
                                | TargetKind::ModuleLibrary
                                | TargetKind::ObjectLibrary
                                | TargetKind::InterfaceLibrary
                                | TargetKind::Utility
                        )
                    {
                        targets.push(t);
                    }
                }
            }
        }
        let rel = |t: &Target| {
            format!(
                "{}{}",
                home_relative_output_path(self.ctx, t.makefile),
                maybe_relative_cur(self.ctx, t.makefile, &gt::support_directory(self.ctx, t))
            )
        };
        // `cmGlobalCommonGenerator::ComputeDirectoryTargets`: per directory,
        // non-global in-build-system targets with their explicit exclusion.
        // An explicitly unexcluded target is also appended to every ancestor
        // directory rule.
        let count = self.ctx.interp.makefiles.len();
        let mut directory_targets: Vec<Vec<(&Target, bool)>> = vec![Vec::new(); count];
        for index in 0..count {
            for key in &self.ctx.interp.makefiles[index].targets {
                let Some(target) = self.ctx.interp.ext.targets.get(key) else {
                    continue;
                };
                if target.kind == TargetKind::GlobalTarget || !is_in_build_system(target) {
                    continue;
                }
                let mut excluded = false;
                if let Some(value) = target.props.get("EXCLUDE_FROM_ALL") {
                    excluded = is_on(&self.ctx.genex(value, Some(target), None));
                    if !excluded {
                        let mut parent = self.ctx.interp.makefiles[index].parent;
                        while let Some(dir) = parent {
                            directory_targets[dir].push((target, false));
                            parent = self.ctx.interp.makefiles[dir].parent;
                        }
                    }
                }
                directory_targets[index].push((target, excluded));
            }
        }
        let mut dirs: Vec<usize> = (0..self.ctx.interp.makefiles.len()).collect();
        // ComputeDirectoryTargets keys a std::map on the full current binary
        // directory, so directories come out in byte order of that path.
        dirs.sort_by(|a, b| {
            let makefiles = &self.ctx.interp.makefiles;
            makefiles[*a]
                .binary_dir
                .as_bytes()
                .cmp(makefiles[*b].binary_dir.as_bytes())
        });
        for dir in dirs {
            if dir == 0 {
                out.push_str("#=============================================================================\n# Directory level rules for the build root directory\n\n");
            } else {
                let rel_dir = home_relative_output_path(self.ctx, dir)
                    .trim_end_matches('/')
                    .to_owned();
                let _ = writeln!(
                    out,
                    "#=============================================================================\n# Directory level rules for directory {}\n",
                    unix_output_path(&rel_dir)
                );
            }
            let dir_targets = &directory_targets[dir];
            let children = self.ctx.interp.makefiles[dir].children.clone();
            for (rule, suffix) in [
                ("all", "all"),
                ("codegen", "codegen"),
                ("preinstall", "preinstall"),
                ("clean", "clean"),
            ] {
                let commands = if rule == "clean" {
                    self.write_directory_clean_script(dir).into_iter().collect()
                } else {
                    Vec::new()
                };
                let mut deps = Vec::new();
                for (target, excluded) in dir_targets.iter() {
                    if rule != "clean" && *excluded {
                        continue;
                    }
                    if rule == "preinstall" {
                        let language = gt::linker_language(self.ctx, target, &self.ctx.config);
                        if !need_relink_before_install(self.ctx, target, &language) {
                            continue;
                        }
                    }
                    deps.push(format!("{}/{suffix}", rel(target)));
                }
                for child in &children {
                    if rule != "clean"
                        && self.ctx.interp.makefiles[*child]
                            .props
                            .get("EXCLUDE_FROM_ALL")
                            .is_some_and(is_on)
                    {
                        continue;
                    }
                    let child_rel = home_relative_output_path(self.ctx, *child)
                        .trim_end_matches('/')
                        .to_owned();
                    deps.push(format!("{child_rel}/{suffix}"));
                }
                let comment = if dir == 0 {
                    format!("The main recursive \"{rule}\" target.")
                } else {
                    format!("Recursive \"{rule}\" directory target.")
                };
                let name = if dir == 0 {
                    rule.to_owned()
                } else {
                    format!(
                        "{}/{}",
                        home_relative_output_path(self.ctx, dir).trim_end_matches('/'),
                        rule
                    )
                };
                write_make_rule(&mut out, Some(&comment), &name, &deps, &commands, true);
            }
        }
        for t in &targets {
            let r = rel(t);
            let _ = writeln!(
                out,
                "#=============================================================================\n# Target rules for target {r}\n"
            );
            let deps: Vec<String> = super::link::target_depends(self.ctx, t)
                .iter()
                .filter_map(|d| self.ctx.interp.ext.targets.get(&d.key))
                .map(|d| format!("{}/all", rel(d)))
                .collect();
            let nums = self.progress_numbers(&t.name);
            let numtext = nums
                .iter()
                .map(usize::to_string)
                .collect::<Vec<_>>()
                .join(",");
            let progress_dir = shell_path(
                self.ctx,
                &format!("{}/CMakeFiles", self.ctx.interp.home_binary),
            );
            let mut commands = vec![
                format!(
                    "$(MAKE) $(MAKESILENT) -f {} {}",
                    shell_path(self.ctx, &format!("{r}/build.make")),
                    shell_path(self.ctx, &format!("{r}/depend"))
                ),
                format!(
                    "$(MAKE) $(MAKESILENT) -f {} {}",
                    shell_path(self.ctx, &format!("{r}/build.make")),
                    shell_path(self.ctx, &format!("{r}/build"))
                ),
            ];
            let target_messages = self
                .ctx
                .interp
                .global_props
                .get("TARGET_MESSAGES")
                .is_none_or(is_on);
            if target_messages {
                commands.push(format!(
                    "@$(CMAKE_COMMAND) -E cmake_echo_color \"--switch=$(COLOR)\" --progress-dir={progress_dir} --progress-num={numtext} \"Built target {}\"",
                    t.name
                ));
            }
            write_make_rule(
                &mut out,
                Some("All Build rule for target."),
                &format!("{r}/all"),
                &deps,
                &commands,
                true,
            );
            let regenerate_dep: Vec<String> = if regenerate {
                vec!["cmake_check_build_system".into()]
            } else {
                Vec::new()
            };
            write_make_rule(
                &mut out,
                Some("Build rule for subdir invocation for target."),
                &format!("{r}/rule"),
                &regenerate_dep,
                &[
                    format!(
                        "$(CMAKE_COMMAND) -E cmake_progress_start {progress_dir} {}",
                        self.progress_count_in_target(t)
                    ),
                    format!(
                        "$(MAKE) $(MAKESILENT) -f CMakeFiles/Makefile2 {}",
                        shell_path(self.ctx, &format!("{r}/all"))
                    ),
                    format!("$(CMAKE_COMMAND) -E cmake_progress_start {progress_dir} 0"),
                ],
                true,
            );
            write_make_rule(
                &mut out,
                Some("Convenience name for target."),
                &t.name,
                &[format!("{r}/rule")],
                &[],
                true,
            );
            let codegen_deps: Vec<String> = super::link::target_depends(self.ctx, t)
                .iter()
                .filter_map(|d| self.ctx.interp.ext.targets.get(&d.key))
                .filter(|d| t.codegen_dependencies.contains(&d.name))
                .map(|d| format!("{}/all", rel(d)))
                .collect();
            write_make_rule(
                &mut out,
                Some("codegen rule for target."),
                &format!("{r}/codegen"),
                &codegen_deps,
                &[
                    format!(
                        "$(MAKE) $(MAKESILENT) -f {} {}",
                        shell_path(self.ctx, &format!("{r}/build.make")),
                        shell_path(self.ctx, &format!("{r}/codegen"))
                    ),
                    format!(
                        "@$(CMAKE_COMMAND) -E cmake_echo_color \"--switch=$(COLOR)\" --progress-dir={progress_dir} --progress-num={numtext} \"Finished codegen for target {}\"",
                        t.name
                    ),
                ],
                true,
            );
            write_make_rule(
                &mut out,
                Some("clean rule for target."),
                &format!("{r}/clean"),
                &[],
                &[format!(
                    "$(MAKE) $(MAKESILENT) -f {} {}",
                    shell_path(self.ctx, &format!("{r}/build.make")),
                    shell_path(self.ctx, &format!("{r}/clean"))
                )],
                true,
            );
        }
        out.push_str("#=============================================================================\n# Special targets to cleanup operation of make.\n\n");
        if regenerate {
            let check_commands =
                check_build_system_commands(self.ctx, "CMakeFiles/Makefile.cmake", 0)
                    .iter()
                    .map(|command| format!("\t{command}\n"))
                    .collect::<String>();
            out.push_str(&format!(
                "# Special rule to run CMake to check the build system integrity.\n# No rule that depends on this can have commands that come from listfiles\n# because they might be regenerated.\ncmake_check_build_system:\n{check_commands}.PHONY : cmake_check_build_system\n\n"
            ));
        }
        let path = format!("{}/CMakeFiles/Makefile2", self.ctx.interp.home_binary);
        let _ = crate::fsutil::write_generated(Path::new(&path), &out);
    }

    fn object_keeps_source_extension(&self, target: &Target, source: &gt::SourceInfo) -> bool {
        let properties = self.source_props(target, &source.path);
        if properties.get("KEEP_EXTENSION").is_some_and(is_on) {
            return true;
        }
        if properties.get("PCH_EXTENSION").is_some() {
            return false;
        }
        !self
            .ctx
            .var(
                target.makefile,
                &format!("CMAKE_{}_OUTPUT_EXTENSION_REPLACE", source.language),
            )
            .is_some_and(|value| is_on(&value))
    }

    fn write_local_makefile(&self, dir: usize) {
        let regenerate = self.regenerate();
        let regenerate_dep: Vec<String> = if regenerate {
            vec!["cmake_check_build_system".into()]
        } else {
            Vec::new()
        };
        let mut out = build_file_header("");
        apply_verbose_default(self.ctx, &mut out);
        out = out
            .replace("# Delete rule output on recipe failure.\n.DELETE_ON_ERROR:\n\n", "# Default target executed when no arguments are given to make.\ndefault_target: all\n.PHONY : default_target\n\n# Allow only one \"make -f Makefile2\" at a time, but pass parallelism.\n.NOTPARALLEL:\n\n")
            .replace("@CMAKE_COMMAND@", &shell_path(self.ctx, &self.ctx.interp.cmake_command))
            .replace("@CMAKE_SOURCE_DIR@", &shell_path(self.ctx, &self.ctx.interp.home_source))
            .replace("@CMAKE_BINARY_DIR@", &shell_path(self.ctx, &self.ctx.interp.home_binary));
        apply_instrumentation_variable(self.ctx, &mut out);
        out.push_str("#=============================================================================\n# Targets provided globally by CMake.\n\n");
        let cmake = shell_path(self.ctx, &self.ctx.interp.cmake_command);
        let ctest = format!(
            "{}/ctest",
            self.ctx
                .interp
                .cmake_command
                .rsplit_once('/')
                .map_or("", |(d, _)| d)
        );
        let cpack = format!(
            "{}/cpack",
            self.ctx
                .interp
                .cmake_command
                .rsplit_once('/')
                .map_or("", |(d, _)| d)
        );
        let special =
            |out: &mut String, name: &str, echo: &str, commands: &[String], deps: &[String]| {
                write_make_rule(
                    out,
                    Some(&format!("Special rule for the target {name}")),
                    name,
                    deps,
                    &std::iter::once(format!("@{}", self.echo(echo, "--cyan", None)))
                        .chain(commands.iter().cloned())
                        .collect::<Vec<_>>(),
                    true,
                );
                let fastdeps = vec![name.to_owned()];
                write_make_rule(
                    out,
                    Some(&format!("Special rule for the target {name}")),
                    &format!("{name}/fast"),
                    &fastdeps,
                    &[],
                    true,
                );
            };
        let root_binary = shell_path(self.ctx, &self.ctx.interp.home_binary);
        let package_cd = if self.ctx.interp.makefiles[dir].binary_dir == self.ctx.interp.home_binary
        {
            String::new()
        } else {
            format!("cd {root_binary} && ")
        };
        let (package, package_source) = package_configs(self.ctx);
        if package {
            special(
                &mut out,
                "package",
                "Run CPack packaging tool...",
                &[format!(
                    "{package_cd}{} --config ./CPackConfig.cmake",
                    shell_path(self.ctx, &cpack)
                )],
                &["preinstall".into()],
            );
        }
        if package_source {
            special(
                &mut out,
                "package_source",
                "Run CPack packaging tool for source...",
                &[format!(
                    "{package_cd}{} --config ./CPackSourceConfig.cmake {root_binary}/CPackSourceConfig.cmake",
                    shell_path(self.ctx, &cpack)
                )],
                &[],
            );
        }
        let testing = self
            .ctx
            .var(0, "CMAKE_TESTING_ENABLED")
            .is_some_and(|v| crate::util::is_on(&v));
        if testing {
            let test_depends = if self
                .ctx
                .var(0, "CMAKE_SKIP_TEST_ALL_DEPENDENCY")
                .is_some_and(|value| crate::util::is_off(&value))
            {
                vec!["all".into()]
            } else {
                Vec::new()
            };
            special(
                &mut out,
                "test",
                "Running tests...",
                &[format!("{} $(ARGS)", shell_path(self.ctx, &ctest))],
                &test_depends,
            );
        }
        special(
            &mut out,
            "edit_cache",
            "No interactive CMake dialog available...",
            &[format!(
                "{cmake} -E echo No\\ interactive\\ CMake\\ dialog\\ available."
            )],
            &[],
        );
        special(
            &mut out,
            "rebuild_cache",
            "Running CMake to regenerate build system...",
            &[format!(
                "{cmake} --regenerate-during-build -S$(CMAKE_SOURCE_DIR) -B$(CMAKE_BINARY_DIR)"
            )],
            &[],
        );
        let install_enabled = self
            .ctx
            .interp
            .ext
            .misc
            .contains_key("install-target-enabled")
            && !self
                .ctx
                .var(0, "CMAKE_SKIP_INSTALL_RULES")
                .is_some_and(|v| crate::util::is_on(&v));
        if install_enabled {
            // cmGlobalGenerator::AddGlobalTarget_Install: the sorted
            // InstallComponents set registered by the install() commands.
            let components: std::collections::BTreeSet<String> = self
                .ctx
                .interp
                .ext
                .misc
                .get("install-components")
                .map(|value| crate::util::expand_list(value, false).into_iter().collect())
                .unwrap_or_default();
            let message = if components.is_empty() {
                "Only default component available".to_owned()
            } else {
                format!(
                    "Available install components are: {}",
                    components
                        .iter()
                        .map(|component| format!("\"{component}\""))
                        .collect::<Vec<_>>()
                        .join(" ")
                )
            };
            write_make_rule(
                &mut out,
                Some("Special rule for the target list_install_components"),
                "list_install_components",
                &[],
                &[format!("@{}", self.echo(&message, "--cyan", None))],
                true,
            );
            write_make_rule(
                &mut out,
                Some("Special rule for the target list_install_components"),
                "list_install_components/fast",
                &["list_install_components".into()],
                &[],
                true,
            );
            let strip = install_strip_enabled(self.ctx);
            let install_cmake = if self.ctx.var(0, "CMake_BINARY_DIR").is_some()
                && !self
                    .ctx
                    .var(0, "CMAKE_CROSSCOMPILING")
                    .is_some_and(|v| crate::util::is_on(&v))
            {
                let built = format!("{}/bin/cmake", self.ctx.interp.home_binary);
                shell_path(self.ctx, &maybe_relative_cur(self.ctx, dir, &built))
            } else {
                cmake.clone()
            };
            let mut install_rules = vec![
                ("install", "Install the project...", ""),
                (
                    "install/local",
                    "Installing only the local directory...",
                    "-DCMAKE_INSTALL_LOCAL_ONLY=1 ",
                ),
            ];
            if strip {
                install_rules.push((
                    "install/strip",
                    "Installing the project stripped...",
                    "-DCMAKE_INSTALL_DO_STRIP=1 ",
                ));
            }
            for (name, echo, args) in install_rules {
                for fast in [false, true] {
                    let rule = if fast {
                        format!("{name}/fast")
                    } else {
                        name.into()
                    };
                    write_make_rule(
                        &mut out,
                        Some(&format!("Special rule for the target {name}")),
                        &rule,
                        &[if fast {
                            "preinstall/fast".into()
                        } else {
                            "preinstall".into()
                        }],
                        &[
                            format!("@{}", self.echo(echo, "--cyan", None)),
                            format!("{install_cmake} {args}-P cmake_install.cmake"),
                        ],
                        true,
                    );
                }
            }
        }
        let progress_dir = shell_path(
            self.ctx,
            &format!("{}/CMakeFiles", self.ctx.interp.home_binary),
        );
        let cur_binary = self.ctx.interp.makefiles[dir].binary_dir.clone();
        let dir_rel = home_relative_output_path(self.ctx, dir)
            .trim_end_matches('/')
            .to_owned();
        let recurse = |rule: &str| {
            if dir == 0 {
                rule.to_owned()
            } else {
                format!("{dir_rel}/{rule}")
            }
        };
        let cdroot = if dir == 0 {
            String::new()
        } else {
            format!(
                "cd {} && ",
                shell_path(self.ctx, &self.ctx.interp.home_binary)
            )
        };
        write_make_rule(
            &mut out,
            Some("The main all target"),
            "all",
            &regenerate_dep,
            &[
                format!(
                    "{cdroot}$(CMAKE_COMMAND) -E cmake_progress_start {progress_dir} {}",
                    quote_output_path(&format!("{cur_binary}//CMakeFiles/progress.marks"))
                ),
                format!(
                    "{cdroot}$(MAKE) $(MAKESILENT) -f CMakeFiles/Makefile2 {}",
                    shell_path(self.ctx, &recurse("all"))
                ),
                format!("$(CMAKE_COMMAND) -E cmake_progress_start {progress_dir} 0"),
            ],
            true,
        );
        let codegen_enabled = self
            .ctx
            .interp
            .ext
            .misc
            .contains_key("allow-global-target-codegen");
        if codegen_enabled {
            write_make_rule(
                &mut out,
                Some("The main codegen target"),
                "codegen",
                &regenerate_dep,
                &[
                    format!(
                        "{cdroot}$(CMAKE_COMMAND) -E cmake_progress_start {progress_dir} {}",
                        quote_output_path(&format!("{cur_binary}//CMakeFiles/progress.marks"))
                    ),
                    format!(
                        "{cdroot}$(MAKE) $(MAKESILENT) -f CMakeFiles/Makefile2 {}",
                        shell_path(self.ctx, &recurse("codegen"))
                    ),
                    format!("$(CMAKE_COMMAND) -E cmake_progress_start {progress_dir} 0"),
                ],
                true,
            );
        }
        write_make_rule(
            &mut out,
            Some("The main clean target"),
            "clean",
            &[],
            &[format!(
                "{cdroot}$(MAKE) $(MAKESILENT) -f CMakeFiles/Makefile2 {}",
                shell_path(self.ctx, &recurse("clean"))
            )],
            true,
        );
        write_make_rule(
            &mut out,
            Some("The main clean target"),
            "clean/fast",
            &["clean".into()],
            &[],
            true,
        );
        write_make_rule(
            &mut out,
            Some("Prepare targets for installation."),
            "preinstall",
            &["all".into()],
            &[format!(
                "{cdroot}$(MAKE) $(MAKESILENT) -f CMakeFiles/Makefile2 {}",
                shell_path(self.ctx, &recurse("preinstall"))
            )],
            true,
        );
        write_make_rule(
            &mut out,
            Some("Prepare targets for installation."),
            "preinstall/fast",
            &[],
            &[format!(
                "{cdroot}$(MAKE) $(MAKESILENT) -f CMakeFiles/Makefile2 {}",
                shell_path(self.ctx, &recurse("preinstall"))
            )],
            true,
        );
        let depend_commands = check_build_system_commands(self.ctx, "CMakeFiles/Makefile.cmake", 1)
            .into_iter()
            .map(|command| format!("{cdroot}{command}"))
            .collect::<Vec<_>>();
        if regenerate {
            write_make_rule(
                &mut out,
                Some("clear depends"),
                "depend",
                &[],
                &depend_commands,
                true,
            );
        }
        let mut objects: BTreeMap<String, (bool, Vec<(String, String)>)> = BTreeMap::new();
        for mf in self.ctx.interp.makefiles.iter() {
            for key in &mf.targets {
                if let Some(t) = self.ctx.interp.ext.targets.get(key) {
                    if !is_in_build_system(t) {
                        continue;
                    }
                    let r = format!(
                        "{}{}",
                        home_relative_output_path(self.ctx, t.makefile),
                        maybe_relative_cur(
                            self.ctx,
                            t.makefile,
                            &gt::support_directory(self.ctx, t)
                        )
                    );
                    if matches!(
                        t.kind,
                        TargetKind::Executable
                            | TargetKind::StaticLibrary
                            | TargetKind::SharedLibrary
                            | TargetKind::ModuleLibrary
                            | TargetKind::ObjectLibrary
                            | TargetKind::InterfaceLibrary
                            | TargetKind::Utility
                    ) && (dir == 0
                        // cmLocalUnixMakefileGenerator3::WriteLocalMakefileTargets
                        // skips INTERFACE libraries (the root Makefile's
                        // WriteConvenienceRules keeps every in-build-system one).
                        || (t.makefile == dir && t.kind != TargetKind::InterfaceLibrary))
                    {
                        if dir == 0 {
                            let _ = writeln!(
                                out,
                                "#=============================================================================\n# Target rules for targets named {}\n",
                                t.name
                            );
                        }
                        if dir == 0 {
                            write_make_rule(
                                &mut out,
                                Some("Build rule for target."),
                                &t.name,
                                &regenerate_dep,
                                &[format!(
                                    "$(MAKE) $(MAKESILENT) -f CMakeFiles/Makefile2 {}",
                                    t.name
                                )],
                                true,
                            );
                        } else {
                            write_make_rule(
                                &mut out,
                                Some("Convenience name for target."),
                                &format!("{r}/rule"),
                                &[],
                                &[format!(
                                    "{cdroot}$(MAKE) $(MAKESILENT) -f CMakeFiles/Makefile2 {}",
                                    shell_path(self.ctx, &format!("{r}/rule"))
                                )],
                                true,
                            );
                            write_make_rule(
                                &mut out,
                                Some("Convenience name for target."),
                                &t.name,
                                &[format!("{r}/rule")],
                                &[],
                                true,
                            );
                        }
                        write_make_rule(
                            &mut out,
                            Some("fast build rule for target."),
                            &format!("{}/fast", t.name),
                            &[],
                            &[format!(
                                "{cdroot}$(MAKE) $(MAKESILENT) -f {} {}",
                                shell_path(self.ctx, &format!("{r}/build.make")),
                                shell_path(self.ctx, &format!("{r}/build"))
                            )],
                            true,
                        );
                    }
                    if t.makefile != dir {
                        continue;
                    }
                    for src in gt::sources(self.ctx, t, &self.ctx.config)
                        .iter()
                        .filter(|s| s.kind == SourceKind::ObjectSource)
                    {
                        let mut object_name = if util::is_full_path(&src.object_name) {
                            Path::new(&src.object_name)
                                .file_name()
                                .and_then(|name| name.to_str())
                                .unwrap_or(&src.object_name)
                                .to_owned()
                        } else {
                            src.object_name.clone()
                        };
                        // cmLocalUnixMakefileGenerator3::GetLocalObjectFiles asks
                        // for object names without the target's custom CUDA output
                        // extension.  Actual compilation modes use .ptx/.fatbin/etc.,
                        // but top-level convenience rules use the normal object name.
                        for (property, custom_extension) in [
                            ("CUDA_PTX_COMPILATION", ".ptx"),
                            ("CUDA_CUBIN_COMPILATION", ".cubin"),
                            ("CUDA_FATBIN_COMPILATION", ".fatbin"),
                            ("CUDA_OPTIX_COMPILATION", ".optixir"),
                        ] {
                            if t.props.get(property).is_some_and(is_on)
                                && object_name.ends_with(custom_extension)
                            {
                                object_name.truncate(object_name.len() - custom_extension.len());
                                let normal_extension = self
                                    .ctx
                                    .interp
                                    .langs
                                    .output_extension
                                    .get(&src.language)
                                    .map(String::as_str)
                                    .unwrap_or(".o");
                                let replace = self
                                    .ctx
                                    .var(
                                        t.makefile,
                                        &format!("CMAKE_{}_OUTPUT_EXTENSION_REPLACE", src.language),
                                    )
                                    .is_some_and(|value| is_on(&value));
                                if !replace {
                                    if let Some(source_extension) = Path::new(&src.path)
                                        .file_name()
                                        .and_then(|name| name.to_str())
                                        .and_then(|name| {
                                            name.rsplit_once('.').map(|(_, extension)| extension)
                                        })
                                    {
                                        object_name.push('.');
                                        object_name.push_str(source_extension);
                                    }
                                }
                                object_name.push_str(normal_extension);
                                break;
                            }
                        }
                        let has_source_extension = self.object_keeps_source_extension(t, src);
                        let entry = objects
                            .entry(object_name)
                            .or_insert_with(|| (has_source_extension, Vec::new()));
                        entry.0 = has_source_extension;
                        entry.1.push((r.clone(), src.language.clone()));
                    }
                }
            }
        }
        let do_preprocess_rules = !self
            .ctx
            .var(dir, "CMAKE_SKIP_PREPROCESSED_SOURCE_RULES")
            .is_some_and(|value| is_on(&value));
        let do_assembly_rules = !self
            .ctx
            .var(dir, "CMAKE_SKIP_ASSEMBLY_SOURCE_RULES")
            .is_some_and(|value| is_on(&value));
        fn split_extension(path: &str) -> Option<(&str, &str)> {
            let dot = path.rfind('.')?;
            Some((&path[..dot], &path[dot..]))
        }
        fn object_alias(output: &str) -> Option<String> {
            let (base, object_extension) = split_extension(output)?;
            let (without_source, _) = split_extension(base)?;
            Some(format!("{without_source}{object_extension}"))
        }
        let write_object_convenience = |out: &mut String,
                                        comment: &str,
                                        output: &str,
                                        entries: &[(String, String)],
                                        has_source_extension: bool|
         -> String {
            let help_target = if has_source_extension {
                if let Some(alias) = object_alias(output) {
                    write_make_rule(out, None, &alias, &[output.to_owned()], &[], true);
                    alias
                } else {
                    output.to_owned()
                }
            } else {
                output.to_owned()
            };
            let commands: Vec<String> = entries
                .iter()
                .map(|(target_dir, _)| {
                    format!(
                        "{cdroot}$(MAKE) $(MAKESILENT) -f {} {}",
                        shell_path(self.ctx, &format!("{target_dir}/build.make")),
                        shell_path(self.ctx, &format!("{target_dir}/{output}"))
                    )
                })
                .collect();
            write_make_rule(out, Some(comment), output, &[], &commands, true);
            help_target
        };
        let mut help_objects = Vec::new();
        for (object_name, (has_source_extension, entries)) in objects {
            help_objects.push(write_object_convenience(
                &mut out,
                "target to build an object file",
                &object_name,
                &entries,
                has_source_extension,
            ));
            let lang_has_rules = entries.iter().any(|(_, language)| {
                matches!(
                    language.as_str(),
                    "C" | "CXX" | "CUDA" | "Fortran" | "HIP" | "ISPC"
                )
            });
            let base = strip_last_extension(&object_name);
            if lang_has_rules && do_preprocess_rules {
                help_objects.push(write_object_convenience(
                    &mut out,
                    "target to preprocess a source file",
                    &format!("{base}.i"),
                    &entries,
                    has_source_extension,
                ));
            }
            if lang_has_rules && do_assembly_rules {
                help_objects.push(write_object_convenience(
                    &mut out,
                    "target to generate assembly for a file",
                    &format!("{base}.s"),
                    &entries,
                    has_source_extension,
                ));
            }
        }
        out.push_str("# Help Target\nhelp:\n\t@echo \"The following are some of the valid targets for this Makefile:\"\n\t@echo \"... all (the default if no target is provided)\"\n\t@echo \"... clean\"\n");
        if regenerate {
            out.push_str("\t@echo \"... depend\"\n");
        }
        if codegen_enabled {
            out.push_str("\t@echo \"... codegen\"\n");
        }
        out.push_str("\t@echo \"... edit_cache\"\n");
        if install_enabled {
            out.push_str("\t@echo \"... install\"\n\t@echo \"... install/local\"\n");
            if install_strip_enabled(self.ctx) {
                out.push_str("\t@echo \"... install/strip\"\n");
            }
            out.push_str("\t@echo \"... list_install_components\"\n");
        }
        if package {
            out.push_str("\t@echo \"... package\"\n");
        }
        if package_source {
            out.push_str("\t@echo \"... package_source\"\n");
        }
        out.push_str("\t@echo \"... rebuild_cache\"\n");
        if testing {
            out.push_str("\t@echo \"... test\"\n");
        }
        let mut utility_names = std::collections::BTreeSet::new();
        let mut global_names = std::collections::BTreeSet::new();
        let mut project_names = std::collections::BTreeSet::new();
        for t in self
            .ctx
            .interp
            .makefiles
            .iter()
            .flat_map(|mf| mf.targets.iter())
            .filter_map(|k| self.ctx.interp.ext.targets.get(k))
            .filter(|t| dir == 0 || t.makefile == dir)
        {
            match t.kind {
                TargetKind::Executable
                | TargetKind::StaticLibrary
                | TargetKind::SharedLibrary
                | TargetKind::ModuleLibrary
                | TargetKind::ObjectLibrary
                    if is_in_build_system(t) =>
                {
                    project_names.insert(t.name.clone());
                }
                TargetKind::InterfaceLibrary if is_in_build_system(t) => {
                    project_names.insert(t.name.clone());
                }
                TargetKind::Utility => {
                    utility_names.insert(t.name.clone());
                }
                TargetKind::GlobalTarget => {
                    global_names.insert(t.name.clone());
                }
                _ => {}
            }
        }
        for n in global_names
            .into_iter()
            .chain(utility_names)
            .chain(project_names)
        {
            let _ = writeln!(out, "\t@echo \"... {n}\"");
        }
        for o in help_objects {
            let _ = writeln!(out, "\t@echo \"... {o}\"");
        }
        let check_commands = check_build_system_commands(self.ctx, "CMakeFiles/Makefile.cmake", 0)
            .iter()
            .map(|command| format!("\t{cdroot}{command}\n"))
            .collect::<String>();
        out.push_str(".PHONY : help\n\n\n\n#=============================================================================\n# Special targets to cleanup operation of make.\n\n");
        if regenerate {
            out.push_str(&format!("# Special rule to run CMake to check the build system integrity.\n# No rule that depends on this can have commands that come from listfiles\n# because they might be regenerated.\ncmake_check_build_system:\n{check_commands}.PHONY : cmake_check_build_system\n\n"));
        }
        let path = format!("{}/Makefile", self.ctx.interp.makefiles[dir].binary_dir);
        if dir == 0 {
            let _ = crate::fsutil::write_generated(Path::new(&path), &out);
        } else {
            let _ = crate::fsutil::write_if_different(Path::new(&path), &out);
        }
    }

    fn write_top_makefile(&self) {
        // The root Makefile is the local makefile of the top directory.  It is
        // written by write_local_makefile(0); replace it with the full root
        // version once directory-level traversal is implemented.
    }
}

/// `cmGeneratorTarget::NeedRelinkBeforeInstall` for Unix Makefiles.
fn need_relink_before_install(ctx: &GenCtx<'_>, target: &Target, language: &str) -> bool {
    if !matches!(
        target.kind,
        TargetKind::Executable | TargetKind::SharedLibrary | TargetKind::ModuleLibrary
    ) || !target.have_install_rule
        || ctx
            .var(target.makefile, "CMAKE_SKIP_RPATH")
            .is_some_and(|value| is_on(&value))
        || target
            .props
            .get("BUILD_WITH_INSTALL_RPATH")
            .is_some_and(|value| is_on(&ctx.genex(value, Some(target), None)))
    {
        return false;
    }
    if language.is_empty()
        || ctx
            .var(
                target.makefile,
                &format!("CMAKE_SHARED_LIBRARY_RUNTIME_{language}_FLAG"),
            )
            .is_none_or(|value| value.is_empty())
    {
        return false;
    }
    let query = super::link::Query::new(ctx, &ctx.config, language);
    if super::link::information::is_chrpath_used(&query, target) {
        return false;
    }

    let install_rpath = target.props.get("INSTALL_RPATH").is_some_and(|value| {
        !ctx.var(target.makefile, "CMAKE_SKIP_INSTALL_RPATH")
            .is_some_and(|skip| is_on(&skip))
            && !ctx.genex(value, Some(target), None).is_empty()
    });
    let build_rpath = !target
        .props
        .get("SKIP_BUILD_RPATH")
        .is_some_and(|value| is_on(&ctx.genex(value, Some(target), None)))
        && (target.props.contains("BUILD_RPATH")
            || target
                .props
                .get("LINK_LIBRARIES")
                .is_some_and(|value| !ctx.genex(value, Some(target), None).is_empty()));
    build_rpath || install_rpath
}

fn makefile_link_libraries(line: &super::link::LinkLine) -> String {
    fn append_values(values: &[(String, impl Clone)]) -> String {
        let mut result = String::new();
        for (value, _) in values {
            if result.is_empty() {
                result.push(' ');
            }
            result.push_str(value);
        }
        result
    }
    let mut result = line.framework_path.clone();
    result.push_str(&append_values(&line.path_fragments));
    result.push_str(&append_values(&line.lib_fragments));
    result
}

fn escape_cmake(text: &str) -> String {
    text.replace('\\', "\\\\").replace('"', "\\\"")
}

/// cmGlobalGenerator::AddGlobalTarget_InstallStrip: `install/strip` exists
/// only when the top directory has `CMAKE_STRIP` set (cmMakefile::IsSet).
fn install_strip_enabled(ctx: &GenCtx<'_>) -> bool {
    ctx.var(0, "CMAKE_STRIP")
        .is_some_and(|v| !v.is_empty() && !crate::util::is_notfound(&v))
}

fn is_in_build_system(target: &Target) -> bool {
    super::ninja::is_in_build_system(target)
}

/// Global targets created by `cmGlobalUnixMakefileGenerator3` via
/// `cmGlobalGenerator::CreateDefaultGlobalTargets`, exposed for extra
/// generators.
pub fn global_targets(ctx: &GenCtx<'_>) -> Vec<super::ninja::GlobalTarget> {
    let interp = ctx.interp;
    let is_reserved = |names: &[&str]| {
        names.iter().any(|name| {
            crate::state::find_global_target_key(interp, name).is_some()
                || interp
                    .ext
                    .alias_backtraces
                    .iter()
                    .any(|(_, alias, _)| alias == name)
        })
    };
    let mut names: Vec<(&str, bool)> = Vec::new();
    let (package, package_source) = package_configs(ctx);
    if package && !is_reserved(&["package", "PACKAGE"]) {
        names.push(("package", true));
    }
    if package_source && !is_reserved(&["package_source"]) {
        names.push(("package_source", true));
    }
    if ctx
        .var(0, "CMAKE_TESTING_ENABLED")
        .is_some_and(|v| is_on(&v))
        && !is_reserved(&["test", "RUN_TESTS"])
    {
        names.push(("test", true));
    }
    names.push(("edit_cache", false));
    names.push(("rebuild_cache", false));
    let install_enabled = interp.ext.misc.contains_key("install-target-enabled")
        && !ctx
            .var(0, "CMAKE_SKIP_INSTALL_RULES")
            .is_some_and(|v| is_on(&v));
    if install_enabled {
        names.push(("list_install_components", true));
        names.push(("install", true));
        names.push(("install/local", true));
        if install_strip_enabled(ctx) {
            names.push(("install/strip", true));
        }
    }
    let mut globals = Vec::new();
    for makefile in 0..interp.makefiles.len() {
        for &(name, per_config) in &names {
            let exists = interp.makefiles[makefile].targets.iter().any(|key| {
                interp
                    .ext
                    .targets
                    .get(key)
                    .is_some_and(|target| target.name == name)
            });
            if exists {
                continue;
            }
            globals.push(super::ninja::GlobalTarget {
                name: name.to_owned(),
                message: String::new(),
                command_lines: Vec::new(),
                depends: Vec::new(),
                uses_terminal: false,
                makefile,
                working_dir: None,
                per_config,
            });
        }
    }
    globals
}
