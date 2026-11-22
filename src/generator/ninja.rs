//! The Ninja generator (port of cmGlobalNinjaGenerator,
//! cmLocalNinjaGenerator, cmNinjaTargetGenerator,
//! cmNinjaNormalTargetGenerator and cmNinjaUtilityTargetGenerator) for the
//! single-configuration "Ninja" generator on Unix.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fmt::Write as _;

use super::ctx::GenCtx;
use super::target::{self as gt, ArtifactType, SourceKind};
use super::{custom, flags, link};
use crate::outconv;
use crate::state::{CustomCommand, CustomCommandKind, Target, TargetKind};
use crate::util;

pub const SHELL_NOOP: &str = ":";

/// A custom command attached to source `index` of directory `dir`.
pub type CcRef = (usize, usize);
const REQUIRED_NINJA_VERSION_CONSOLE_POOL: &str = "1.5";

#[derive(Default, Clone, Debug)]
pub struct Rule {
    pub name: String,
    pub command: String,
    pub description: String,
    pub comment: String,
    pub depfile: String,
    pub deptype: String,
    pub rspfile: String,
    pub rspcontent: String,
    pub restat: String,
    pub generator: bool,
}

#[derive(Default, Clone, Debug)]
pub struct Build {
    pub comment: String,
    pub rule: String,
    pub outputs: Vec<String>,
    pub implicit_outs: Vec<String>,
    pub workdir_outs: Vec<String>,
    pub explicit_deps: Vec<String>,
    pub implicit_deps: Vec<String>,
    pub order_only_deps: Vec<String>,
    /// Ordered (CMake uses std::map: sorted by name).
    pub variables: BTreeMap<String, String>,
    pub rsp_file: String,
}

impl Build {
    pub fn new(rule: &str) -> Build {
        Build {
            rule: rule.to_owned(),
            ..Build::default()
        }
    }
}

/// `cmGlobalNinjaGenerator::EncodeLiteral`.
pub fn encode_literal(text: &str) -> String {
    text.replace('$', "$$").replace('\n', "$\n")
}

/// `cmGlobalNinjaGenerator::EncodePath`.
pub fn encode_path(path: &str) -> String {
    encode_literal(path).replace(' ', "$ ").replace(':', "$:")
}

/// `cmGlobalNinjaGenerator::EncodeRuleName`.
pub fn encode_rule_name(name: &str) -> String {
    let mut out = String::new();
    for byte in name.bytes() {
        if byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-' {
            out.push(byte as char);
        } else {
            let _ = write!(out, ".{byte:02x}");
        }
    }
    out
}

pub fn write_divider(out: &mut String) {
    out.push_str(
        "# =============================================================================\n",
    );
}

pub fn write_comment(out: &mut String, comment: &str) {
    if comment.is_empty() {
        return;
    }
    out.push_str("\n#############################################\n");
    for line in comment.split('\n') {
        let _ = writeln!(out, "# {line}");
    }
    out.push('\n');
}

fn trim_whitespace(text: &str) -> &str {
    text.trim_matches(|ch: char| {
        ch == ' ' || ch == '\t' || ch == '\n' || ch == '\r' || ch == '\x0b' || ch == '\x0c'
    })
}

pub fn write_variable(out: &mut String, name: &str, value: &str, comment: &str, indent: usize) {
    let value = if name == "CODE_CHECK" || name == "LAUNCHER" {
        value
    } else {
        trim_whitespace(value)
    };
    if value.is_empty() {
        return;
    }
    write_comment(out, comment);
    for _ in 0..indent {
        out.push_str("  ");
    }
    let _ = writeln!(out, "{name} = {value}");
}

pub fn write_rule(out: &mut String, rule: &Rule) {
    write_comment(out, &rule.comment);
    let _ = writeln!(out, "rule {}", rule.name);
    let mut kv = |key: &str, value: &str| {
        if !value.is_empty() {
            let _ = writeln!(out, "  {key} = {value}");
        }
    };
    kv("depfile", &rule.depfile);
    kv("deps", &rule.deptype);
    kv("command", &rule.command);
    kv("description", &rule.description);
    if !rule.rspfile.is_empty() {
        kv("rspfile", &rule.rspfile);
        kv("rspfile_content", &rule.rspcontent);
    }
    kv("restat", &rule.restat);
    if rule.generator {
        kv("generator", "1");
    }
    out.push('\n');
}

/// `cmGlobalNinjaGenerator::WriteBuild`.
pub fn write_build(out: &mut String, build: &Build, command_line_limit: i64) -> bool {
    write_comment(out, &build.comment);
    let mut build_str = String::from("build");
    for output in &build.outputs {
        build_str.push(' ');
        build_str.push_str(&encode_path(output));
    }
    if !build.implicit_outs.is_empty() {
        build_str.push_str(" |");
        for output in &build.implicit_outs {
            build_str.push(' ');
            build_str.push_str(&encode_path(output));
        }
    }
    if !build.workdir_outs.is_empty() {
        if build.implicit_outs.is_empty() {
            build_str.push_str(" |");
        }
        for output in &build.workdir_outs {
            build_str.push_str(" ${cmake_ninja_workdir}");
            build_str.push_str(&encode_path(output));
        }
    }
    build_str.push_str(": ");
    build_str.push_str(&build.rule);
    let mut arguments = String::new();
    for dep in &build.explicit_deps {
        arguments.push(' ');
        arguments.push_str(&encode_path(dep));
    }
    if !build.implicit_deps.is_empty() {
        arguments.push_str(" |");
        for dep in &build.implicit_deps {
            arguments.push(' ');
            arguments.push_str(&encode_path(dep));
        }
    }
    if !build.order_only_deps.is_empty() {
        arguments.push_str(" ||");
        for dep in &build.order_only_deps {
            arguments.push(' ');
            arguments.push_str(&encode_path(dep));
        }
    }
    arguments.push('\n');
    let mut assignments = String::new();
    for (name, value) in &build.variables {
        write_variable(&mut assignments, name, value, "", 1);
    }
    let mut use_rsp = false;
    if command_line_limit < 0
        || (command_line_limit > 0
            && (arguments.len() + build_str.len() + assignments.len() + 1000) as i64
                > command_line_limit)
    {
        write_variable(&mut assignments, "RSP_FILE", &build.rsp_file, "", 1);
        use_rsp = true;
    }
    out.push_str(&build_str);
    out.push_str(&arguments);
    out.push_str(&assignments);
    out.push('\n');
    use_rsp
}

/// `cmSystemTools::CalculateCommandLineLengthLimit` on Linux:
/// min(MAX_ARG_STRLEN = PAGE_SIZE * 32, ARG_MAX - 1000).
pub fn command_line_length_limit() -> i64 {
    // SAFETY: sysconf is always safe to call.
    let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) } as i64;
    let mut limit = page * 32;
    // SAFETY: as above.
    let arg_max = unsafe { libc::sysconf(libc::_SC_ARG_MAX) } as i64;
    if arg_max != -1 {
        let arg_max = if arg_max < 1000 { 0 } else { arg_max - 1000 };
        limit = limit.min(arg_max);
    }
    limit
}

/// What a dependency on a target means (`cmNinjaTargetDepends`).
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum DependOn {
    TargetArtifact,
    TargetOrdering,
}

/// A target as seen by the generator: a real target or a generated global
/// utility target (test, install, rebuild_cache, ...).
#[derive(Clone, Debug)]
pub struct GlobalTarget {
    pub name: String,
    pub message: String,
    pub command_lines: Vec<Vec<String>>,
    pub depends: Vec<String>,
    pub uses_terminal: bool,
    pub makefile: usize,
    /// `GlobalTargetInfo::WorkingDir` (default: the directory's binary dir).
    pub working_dir: Option<String>,
}

pub struct NinjaGenerator<'a> {
    pub ctx: &'a GenCtx<'a>,
    pub build: String,
    pub rules: String,
    rule_names: HashSet<String>,
    pub custom_command_outputs: BTreeSet<String>,
    /// alias -> Some(target name) or None when ambiguous.
    target_aliases: BTreeMap<String, Option<String>>,
    global_targets: Vec<GlobalTarget>,
    /// Object files per target for later reuse (`Configs[config].Objects`).
    objects: HashMap<String, Vec<String>>,
    /// `cmGlobalNinjaGenerator::Configs[config].CustomCommands` (seen).
    seen_custom_commands: HashSet<CcRef>,
    /// `cmLocalNinjaGenerator::CustomCommands` of the current directory, in
    /// first-use order, with the targets using each (`CustomCommandTargets`).
    local_custom_commands: Vec<(CcRef, Vec<&'a Target>)>,
    /// `TargetDependsClosures` cache.
    depends_closures: HashMap<String, BTreeSet<String>>,
    /// `CompileCommandsStream` entries (compile_commands.json).
    compile_commands: Vec<String>,
    pub cmake_cmd: String,
    pub ninja_cmd: String,
    home_binary: String,
    home_source: String,
    config: String,
    install_target_enabled: bool,
}

impl<'a> NinjaGenerator<'a> {
    pub fn new(ctx: &'a GenCtx<'a>) -> NinjaGenerator<'a> {
        let interp = ctx.interp;
        let cmake = outconv::escape_for_shell(
            &interp.cmake_command,
            outconv::NINJA_UNIX | outconv::FLAG_ALLOW_MAKE_VARIABLES,
        );
        let make_program = interp
            .cache
            .value("CMAKE_MAKE_PROGRAM")
            .unwrap_or("ninja")
            .to_owned();
        let ninja = outconv::escape_for_shell(
            &make_program,
            outconv::NINJA_UNIX | outconv::FLAG_ALLOW_MAKE_VARIABLES,
        );
        NinjaGenerator {
            ctx,
            build: String::new(),
            rules: String::new(),
            rule_names: HashSet::new(),
            custom_command_outputs: BTreeSet::new(),
            target_aliases: BTreeMap::new(),
            global_targets: Vec::new(),
            objects: HashMap::new(),
            seen_custom_commands: HashSet::new(),
            local_custom_commands: Vec::new(),
            depends_closures: HashMap::new(),
            compile_commands: Vec::new(),
            cmake_cmd: cmake,
            ninja_cmd: ninja,
            home_binary: interp.home_binary.clone(),
            home_source: interp.home_source.clone(),
            config: ctx.config.clone(),
            install_target_enabled: interp.ext.misc.contains_key("install-target-enabled"),
        }
    }

    // ------------------------------------------------------------------
    // Path helpers

    /// `cmOutputConverter::MaybeRelativeToTopBinDir` +
    /// `cmGlobalNinjaGenerator::ConvertToNinjaPath`.
    pub fn ninja_path(&self, path: &str) -> String {
        let bin = &self.home_binary;
        if path == bin {
            return ".".to_owned();
        }
        if util::is_subdirectory(path, bin) {
            return path[bin.len() + 1..].to_owned();
        }
        path.to_owned()
    }

    /// `cmGlobalNinjaGenerator::ConvertToNinjaAbsPath`.
    pub fn ninja_abs_path(&self, path: &str) -> String {
        util::collapse_full_path(path, &self.home_binary)
    }

    /// `cmOutputConverter::ConvertToOutputFormat(path, SHELL)`.
    pub fn shell(&self, text: &str) -> String {
        outconv::escape_for_shell(
            text,
            outconv::NINJA_UNIX | outconv::FLAG_ALLOW_MAKE_VARIABLES,
        )
    }

    pub fn add_rule(&mut self, rule: &Rule) {
        if self.rule_names.insert(rule.name.clone()) {
            write_rule(&mut self.rules, rule);
        }
    }

    pub fn has_rule(&self, name: &str) -> bool {
        self.rule_names.contains(name)
    }

    fn write_build(&mut self, build: &Build) {
        write_build(&mut self.build, build, 0);
    }

    /// `cmLocalNinjaGenerator::BuildCommandLine` (Unix).
    pub fn build_command_line(&self, lines: &[String]) -> String {
        if lines.is_empty() {
            return SHELL_NOOP.to_owned();
        }
        lines.join(" && ")
    }

    // ------------------------------------------------------------------
    // Target outputs/dependencies

    /// `cmGlobalNinjaGenerator::AppendTargetOutputs`.
    pub fn append_target_outputs(&self, target: &Target, out: &mut Vec<String>, depend: DependOn) {
        let config = self.config.clone();
        match target.kind {
            TargetKind::SharedLibrary | TargetKind::StaticLibrary | TargetKind::ModuleLibrary => {
                if depend == DependOn::TargetOrdering {
                    out.push(format!("cmake_object_order_depends_target_{}", target.name));
                    return;
                }
                out.push(self.ninja_path(&gt::full_path(
                    self.ctx,
                    target,
                    &config,
                    ArtifactType::Runtime,
                    false,
                )));
            }
            TargetKind::Executable => {
                out.push(self.ninja_path(&gt::full_path(
                    self.ctx,
                    target,
                    &config,
                    ArtifactType::Runtime,
                    false,
                )));
            }
            TargetKind::ObjectLibrary => {
                if depend == DependOn::TargetOrdering {
                    out.push(format!("cmake_object_order_depends_target_{}", target.name));
                    return;
                }
                let dir = self.target_dir_prefix(target.makefile);
                out.push(format!("{dir}{}", target.name));
            }
            TargetKind::Utility | TargetKind::GlobalTarget | TargetKind::InterfaceLibrary => {
                let dir = self.target_dir_prefix(target.makefile);
                out.push(format!("{dir}{}", target.name));
            }
            _ => {}
        }
    }

    /// The `<dir>/` prefix used for utility/global targets in subdirectories.
    fn target_dir_prefix(&self, makefile: usize) -> String {
        let binary = &self.ctx.interp.makefiles[makefile].binary_dir;
        let relative = self.ninja_path(binary);
        if relative == "." || relative.is_empty() {
            String::new()
        } else {
            format!("{relative}/")
        }
    }

    /// `cmGlobalNinjaGenerator::AppendTargetDepends`.
    pub fn append_target_depends(&self, target: &Target, out: &mut Vec<String>, depend: DependOn) {
        if target.kind == TargetKind::GlobalTarget {
            return;
        }
        let mut outs = Vec::new();
        for dependency in link::target_dependencies(self.ctx, target, &self.config) {
            let Some(dep) = self.ctx.target(&dependency) else {
                continue;
            };
            if !is_in_build_system(dep) {
                continue;
            }
            self.append_target_outputs(dep, &mut outs, depend);
        }
        // cmGlobalNinjaGenerator::AppendTargetDepends sorts but keeps
        // duplicates.
        outs.sort();
        out.extend(outs);
    }

    /// `cmGlobalNinjaGenerator::AddTargetAlias`: the target's own outputs
    /// are marked ambiguous so no other target aliases them, then the alias
    /// is inserted if absent (the first target to claim a name keeps it).
    pub fn add_target_alias(&mut self, alias: &str, target: &Target) {
        let mut outputs = Vec::new();
        self.append_target_outputs(target, &mut outputs, DependOn::TargetArtifact);
        for output in outputs {
            self.target_aliases.insert(output, None);
        }
        self.target_aliases
            .entry(alias.to_owned())
            .or_insert_with(|| Some(target.name.clone()));
    }

    // ------------------------------------------------------------------
    // Top-level generation

    /// `cmGlobalNinjaGenerator::Generate` (single config).
    pub fn generate(&mut self) -> std::io::Result<()> {
        let interp = self.ctx.interp;
        // Default global targets per directory.
        self.create_global_targets();
        // Local generators (directories) in creation order.
        for index in 0..interp.makefiles.len() {
            self.generate_local(index);
        }
        self.write_target_aliases();
        self.write_folder_targets();
        self.write_builtin_targets();
        let build_path = format!("{}/build.ninja", self.home_binary);
        let rules_path = format!("{}/CMakeFiles/rules.ninja", self.home_binary);
        let mut rules_file = String::from(
            "# CMAKE generated file: DO NOT EDIT!\n# Generated by \"Ninja\" Generator, CMake Version 4.1\n\n# This file contains all the rules used to get the outputs files\n# built from the input files.\n# It is included in the main 'build.ninja'.\n\n",
        );
        rules_file.push_str(&self.rules);
        let mut build_file = String::from(
            "# CMAKE generated file: DO NOT EDIT!\n# Generated by \"Ninja\" Generator, CMake Version 4.1\n\n# This file contains all the build statements describing the\n# compilation DAG.\n\n",
        );
        build_file.push_str(&self.build);
        // CloseCompileCommandsStream.
        if !self.compile_commands.is_empty() {
            let content = format!("[\n{}\n]\n", self.compile_commands.join(",\n"));
            crate::fsutil::write_if_different(
                std::path::Path::new(&format!("{}/compile_commands.json", self.home_binary)),
                content.as_bytes(),
            )?;
        }
        // cmGlobalGenerator::WriteSummary: every build-system target's
        // support directory.
        let mut summary = String::new();
        for index in 0..interp.makefiles.len() {
            for target in self.ctx.targets_in_order() {
                if target.makefile == index
                    && is_in_build_system(target)
                    && target.kind != TargetKind::GlobalTarget
                {
                    let directory = gt::support_directory(self.ctx, target);
                    self.write_labels(target, &directory)?;
                    summary.push_str(&directory);
                    summary.push('\n');
                }
            }
            // The directory's copies of the global targets follow.
            let binary = &interp.makefiles[index].binary_dir;
            for global in self
                .global_targets
                .iter()
                .filter(|global| global.makefile == index)
            {
                summary.push_str(&format!("{binary}/CMakeFiles/{}.dir\n", global.name));
            }
        }
        crate::fsutil::write_if_different(
            std::path::Path::new(&format!(
                "{}/CMakeFiles/TargetDirectories.txt",
                self.home_binary
            )),
            summary.as_bytes(),
        )?;
        // cmSystemTools::GetErrorOccurredFlag: the streams are marked failed,
        // so neither file is replaced.
        if self.ctx.interp.error || !self.ctx.errors.borrow().is_empty() {
            return Ok(());
        }
        crate::fsutil::write_if_different(
            std::path::Path::new(&rules_path),
            rules_file.as_bytes(),
        )?;
        crate::fsutil::write_if_different(
            std::path::Path::new(&build_path),
            build_file.as_bytes(),
        )?;
        Ok(())
    }
}

impl<'a> NinjaGenerator<'a> {
    fn project_name(&self, makefile: usize) -> String {
        self.ctx.interp.makefiles[makefile]
            .props
            .get("PROJECT_NAME")
            .unwrap_or("Project")
            .to_owned()
    }

    fn write_project_header(&self, out: &mut String, makefile: usize) {
        write_divider(out);
        let _ = writeln!(out, "# Project: {}", self.project_name(makefile));
        let _ = writeln!(out, "# Configurations: {}", self.config);
        write_divider(out);
    }

    /// `cmLocalNinjaGenerator::Generate`.
    fn generate_local(&mut self, makefile: usize) {
        let interp = self.ctx.interp;
        // WriteProcessedMakefile
        let mut header = String::new();
        write_divider(&mut header);
        let _ = writeln!(
            header,
            "# Write statements declared in CMakeLists.txt:\n# {}",
            self.ctx.var_safe(makefile, "CMAKE_CURRENT_LIST_FILE")
        );
        if makefile == 0 {
            header.push_str("# Which is the root file.\n");
        }
        write_divider(&mut header);
        header.push('\n');
        self.build.push_str(&header);
        if makefile == 0 {
            // WriteBuildFileTop
            let mut top = String::new();
            self.write_project_header(&mut top, 0);
            write_comment(&mut top, "Minimal version of Ninja required by this file");
            let _ = write!(
                top,
                "ninja_required_version = {REQUIRED_NINJA_VERSION_CONSOLE_POOL}\n\n"
            );
            write_variable(
                &mut top,
                "CONFIGURATION",
                &self.config,
                "Set configuration variable for custom commands.",
                0,
            );
            write_divider(&mut top);
            top.push_str("# Include auxiliary files.\n\n");
            write_comment(&mut top, "Include rules file.");
            top.push_str("include CMakeFiles/rules.ninja\n\n");
            write_divider(&mut top);
            write_comment(
                &mut top,
                "Logical path to working directory; prefix for absolute paths.",
            );
            let _ = writeln!(
                top,
                "cmake_ninja_workdir = {}",
                encode_path(&format!("{}/", self.home_binary))
            );
            self.build.push_str(&top);
            let mut rules_top = String::new();
            self.write_project_header(&mut rules_top, 0);
            // WritePools
            write_divider(&mut rules_top);
            let pools = interp
                .global_props
                .get("JOB_POOLS")
                .map(str::to_owned)
                .or_else(|| self.ctx.var(0, "CMAKE_JOB_POOLS"));
            if let Some(pools) = pools {
                write_comment(&mut rules_top, "Pools defined by global property JOB_POOLS");
                for pool in util::expand_list(&pools, false) {
                    if let Some((name, depth)) = pool.split_once('=')
                        && let Ok(depth) = depth.trim().parse::<u32>()
                    {
                        let _ = write!(rules_top, "pool {name}\n  depth = {depth}\n\n");
                    } else {
                        crate::configure::system_error(
                            None,
                            &format!("Invalid pool defined by property 'JOB_POOLS': {pool}"),
                        );
                    }
                }
            }
            self.rules.push_str(&rules_top);
        }
        // Targets of this directory in creation order, then global targets.
        let targets: Vec<&Target> = self
            .ctx
            .targets_in_order()
            .into_iter()
            .filter(|target| {
                target.makefile == makefile && !target.imported && target.alias_of.is_none()
            })
            .collect();
        for target in targets {
            match target.kind {
                TargetKind::Executable
                | TargetKind::StaticLibrary
                | TargetKind::SharedLibrary
                | TargetKind::ModuleLibrary
                | TargetKind::ObjectLibrary => self.generate_normal_target(target),
                // cmNinjaTargetGenerator::New: INTERFACE libraries in the
                // build system use the utility generator.
                // INTEGRATION: C++20 module sources use the normal generator.
                TargetKind::Utility => self.generate_utility_target(target),
                TargetKind::InterfaceLibrary if is_in_build_system(target) => {
                    self.generate_utility_target(target)
                }
                _ => {}
            }
        }
        let globals: Vec<GlobalTarget> = self
            .global_targets
            .iter()
            .filter(|global| global.makefile == makefile)
            .cloned()
            .collect();
        for global in globals {
            self.generate_global_target(&global);
        }
        self.write_custom_command_build_statements(makefile);
    }

    // ------------------------------------------------------------------
    // Global targets

    /// `cmGlobalGenerator::CreateDefaultGlobalTargets` for each directory.
    fn create_global_targets(&mut self) {
        let interp = self.ctx.interp;
        let ctest = interp
            .cache
            .value("CMAKE_CTEST_COMMAND")
            .unwrap_or("ctest")
            .to_owned();
        let cmake = interp.cmake_command.clone();
        type TestInfo = (String, String, Vec<Vec<String>>, Vec<String>, bool);
        let mut infos: Vec<TestInfo> = Vec::new();
        let mut working_dirs: HashMap<String, String> = HashMap::new();
        // AddGlobalTarget_Package / AddGlobalTarget_PackageSource.
        let cpack = format!("{}/cpack", util::filename_path(&interp.cmake_command));
        let root_binary = interp.makefiles[0].binary_dir.clone();
        // CheckReservedTargetName: stops at the first reserved name in use.
        let ctx = self.ctx;
        let reserved_free = |names: &[&str], reason: &str| {
            for name in names {
                if let Some(key) = crate::state::find_global_target_key(interp, name) {
                    ctx.error(
                        interp.ext.targets[&key].backtrace.clone(),
                        format!("The target name \"{name}\" is reserved {reason}."),
                    );
                    return false;
                }
            }
            true
        };
        if std::path::Path::new(&format!("{root_binary}/CPackConfig.cmake")).exists()
            && reserved_free(&["package", "PACKAGE"], "when CPack packaging is enabled")
        {
            let mut depends = Vec::new();
            if self
                .ctx
                .var(0, "CMAKE_SKIP_PACKAGE_ALL_DEPENDENCY")
                .is_none_or(|value| util::is_off(&value))
            {
                depends.push("all".to_owned());
            }
            infos.push((
                "package".into(),
                "Run CPack packaging tool...".into(),
                vec![vec![
                    cpack.clone(),
                    "--config".into(),
                    "./CPackConfig.cmake".into(),
                ]],
                depends,
                true,
            ));
            working_dirs.insert("package".into(), root_binary.clone());
        }
        let source_config = format!("{root_binary}/CPackSourceConfig.cmake");
        if std::path::Path::new(&source_config).exists()
            && reserved_free(
                &["package_source"],
                "when CPack source packaging is enabled",
            )
        {
            infos.push((
                "package_source".into(),
                "Run CPack packaging tool for source...".into(),
                vec![vec![
                    cpack,
                    "--config".into(),
                    "./CPackSourceConfig.cmake".into(),
                    source_config,
                ]],
                Vec::new(),
                true,
            ));
            working_dirs.insert("package_source".into(), root_binary.clone());
        }
        if self
            .ctx
            .var(0, "CMAKE_TESTING_ENABLED")
            .is_some_and(|value| util::is_on(&value))
        {
            let mut line = vec![ctest];
            if let Some(args) = self.ctx.var(0, "CMAKE_CTEST_ARGUMENTS") {
                line.extend(util::expand_list(&args, false));
            }
            line.push("$(ARGS)".to_owned());
            let mut depends = Vec::new();
            if self
                .ctx
                .var(0, "CMAKE_SKIP_TEST_ALL_DEPENDENCY")
                .is_some_and(|value| util::is_off(&value))
            {
                depends.push("all".to_owned());
            }
            infos.push((
                "test".into(),
                "Running tests...".into(),
                vec![line],
                depends,
                true,
            ));
        }
        infos.push((
            "edit_cache".into(),
            "No interactive CMake dialog available...".into(),
            vec![vec![
                cmake.clone(),
                "-E".into(),
                "echo".into(),
                "No interactive CMake dialog available.".into(),
            ]],
            Vec::new(),
            false,
        ));
        infos.push((
            "rebuild_cache".into(),
            "Running CMake to regenerate build system...".into(),
            vec![vec![
                cmake.clone(),
                "--regenerate-during-build".into(),
                format!("-S{}", self.home_source),
                format!("-B{}", self.home_binary),
            ]],
            Vec::new(),
            true,
        ));
        let skip_install = self
            .ctx
            .var(0, "CMAKE_SKIP_INSTALL_RULES")
            .is_some_and(|value| util::is_on(&value));
        if self.install_target_enabled && !skip_install {
            let components: BTreeSet<String> = interp
                .ext
                .misc
                .get("install-components")
                .map(|value| util::expand_list(value, false).into_iter().collect())
                .unwrap_or_default();
            let message = if components.is_empty() {
                "Only default component available".to_owned()
            } else {
                format!(
                    "Available install components are: {}",
                    components
                        .iter()
                        .map(|c| format!("\"{c}\""))
                        .collect::<Vec<_>>()
                        .join(" ")
                )
            };
            infos.push((
                "list_install_components".into(),
                message,
                Vec::new(),
                Vec::new(),
                false,
            ));
            let mut depends = Vec::new();
            if self
                .ctx
                .var(0, "CMAKE_SKIP_INSTALL_ALL_DEPENDENCY")
                .is_none_or(|value| util::is_off(&value))
            {
                depends.push("all".to_owned());
            }
            // We are building CMake itself: the original executable cannot
            // install over itself, so the in-tree `cmake` target is used.
            let install_cmake = if self.ctx.var(0, "CMake_BINARY_DIR").is_some()
                && !self
                    .ctx
                    .var(0, "CMAKE_CROSSCOMPILING")
                    .is_some_and(|value| util::is_on(&value))
            {
                "cmake".to_owned()
            } else {
                cmake.clone()
            };
            let base = vec![
                install_cmake,
                "-P".to_owned(),
                "cmake_install.cmake".to_owned(),
            ];
            infos.push((
                "install".into(),
                "Install the project...".into(),
                vec![base.clone()],
                depends.clone(),
                true,
            ));
            let mut local = base.clone();
            local.insert(1, "-DCMAKE_INSTALL_LOCAL_ONLY=1".into());
            let parallel = interp
                .global_props
                .get("INSTALL_PARALLEL")
                .is_some_and(util::is_on);
            infos.push((
                "install/local".into(),
                "Installing only the local directory...".into(),
                vec![local],
                depends.clone(),
                !parallel,
            ));
            if self
                .ctx
                .var(0, "CMAKE_STRIP")
                .is_some_and(|value| !value.is_empty() && !util::is_notfound(&value))
            {
                let mut strip = base;
                strip.insert(1, "-DCMAKE_INSTALL_DO_STRIP=1".into());
                infos.push((
                    "install/strip".into(),
                    "Installing the project stripped...".into(),
                    vec![strip],
                    depends,
                    true,
                ));
            }
        } else if self.install_target_enabled && skip_install {
            // Warning issued by the C++ in AddGlobalTarget_Install.
        }
        for makefile in 0..interp.makefiles.len() {
            for (name, message, lines, depends, terminal) in &infos {
                self.global_targets.push(GlobalTarget {
                    name: name.clone(),
                    message: message.clone(),
                    command_lines: lines.clone(),
                    depends: depends.clone(),
                    uses_terminal: *terminal,
                    makefile,
                    working_dir: working_dirs.get(name).cloned(),
                });
            }
        }
    }

    /// Utility generation for a global target (cmNinjaUtilityTargetGenerator
    /// for GLOBAL_TARGET).
    fn generate_global_target(&mut self, global: &GlobalTarget) {
        let prefix = self.target_dir_prefix(global.makefile);
        let binary_dir = global.working_dir.clone().unwrap_or_else(|| {
            self.ctx.interp.makefiles[global.makefile]
                .binary_dir
                .clone()
        });
        let output_name = format!("{prefix}{}", global.name);
        let util_name = format!("{prefix}CMakeFiles/{}.util", global.name);
        let mut lines: Vec<String> = Vec::new();
        let real_lines: Vec<&Vec<String>> = global
            .command_lines
            .iter()
            .filter(|line| !line.is_empty())
            .collect();
        if !real_lines.is_empty() {
            lines.push(format!("cd {}", self.shell(&binary_dir)));
            for line in real_lines {
                // cmCustomCommandGenerator: an executable target named by
                // argv[0] runs from its build location.
                let argv0 = match gt::target_in_directory(self.ctx, global.makefile, &line[0])
                    .filter(|target| target.kind == TargetKind::Executable)
                {
                    Some(target) => {
                        gt::full_path(self.ctx, target, &self.config, ArtifactType::Runtime, false)
                    }
                    None => line[0].clone(),
                };
                let mut command = self.shell(&argv0);
                for arg in &line[1..] {
                    // Make-style variable references like $(ARGS) are dropped
                    // by the Ninja generator.
                    if arg == "$(ARGS)" {
                        continue;
                    }
                    command.push(' ');
                    // Global targets use old-style escaping
                    // (cmCustomCommandGenerator escapeForShellOldStyle).
                    command.push_str(&arg.replace(' ', "\\ "));
                }
                lines.push(command);
            }
        }
        // AppendTargetDepends for GLOBAL_TARGET: utilities are CMake-provided
        // targets of the same directory.
        let directory = self.ctx.interp.makefiles[global.makefile]
            .binary_dir
            .clone();
        let mut depends: Vec<String> = global
            .depends
            .iter()
            .map(|dep| self.ninja_path(&format!("{directory}/{dep}")))
            .collect();
        depends.sort();
        if lines.is_empty() {
            let mut build = Build::new("phony");
            build.comment = format!("Utility command for {}", global.name);
            build.outputs.push(output_name.clone());
            build.explicit_deps = depends;
            self.write_build(&build);
            return;
        }
        let command = self.build_command_line(&lines);
        self.add_custom_command_rule();
        let mut build = Build::new("CUSTOM_COMMAND");
        build.comment = format!("Utility command for {}", global.name);
        build.outputs.push(util_name.clone());
        build.explicit_deps = depends;
        build.variables.insert("COMMAND".into(), command);
        build
            .variables
            .insert("DESC".into(), encode_literal(&global.message));
        build.variables.insert("restat".into(), "1".into());
        if global.uses_terminal {
            build.variables.insert("pool".into(), "console".into());
        }
        self.write_build(&build);
        let mut alias = Build::new("phony");
        alias.outputs.push(output_name);
        alias.explicit_deps.push(util_name);
        self.write_build(&alias);
    }

    pub fn add_custom_command_rule(&mut self) {
        self.add_rule(&Rule {
            name: "CUSTOM_COMMAND".into(),
            command: "$COMMAND".into(),
            description: "$DESC".into(),
            comment: "Rule for running custom commands.".into(),
            ..Rule::default()
        });
    }

    // ------------------------------------------------------------------
    // Aliases, folders, builtins

    fn write_target_aliases(&mut self) {
        let mut out = String::new();
        write_divider(&mut out);
        out.push_str("# Target aliases.\n\n");
        let aliases: Vec<(String, Option<String>)> = self
            .target_aliases
            .iter()
            .map(|(a, t)| (a.clone(), t.clone()))
            .collect();
        for (alias, target) in aliases {
            let Some(target) = target else {
                continue;
            };
            if self.custom_command_outputs.contains(&alias) {
                continue;
            }
            let Some(target) = self.ctx.target(&target) else {
                continue;
            };
            let mut build = Build::new("phony");
            build.outputs.push(alias);
            self.append_target_outputs(target, &mut build.explicit_deps, DependOn::TargetArtifact);
            write_build(&mut out, &build, 0);
        }
        self.build.push_str(&out);
    }

    /// `cmGlobalNinjaGenerator::WriteFolderTargets`: per-directory `all`
    /// phony targets.
    /// `cmGlobalNinjaGenerator::WriteFolderTargets` with
    /// `cmGlobalCommonGenerator::ComputeDirectoryTargets` (directories in
    /// sorted binary-path order, as std::map iterates).
    fn write_folder_targets(&mut self) {
        let interp = self.ctx.interp;
        let mut out = String::new();
        write_divider(&mut out);
        out.push_str("# Folder targets.\n\n");
        let count = interp.makefiles.len();
        // Per directory: (targets that are not excluded, in order).
        let mut dir_targets: Vec<Vec<&Target>> = vec![Vec::new(); count];
        for index in 0..count {
            for name in &interp.makefiles[index].targets {
                let Some(target) = interp.ext.targets.get(name) else {
                    continue;
                };
                if target.imported
                    || target.kind == TargetKind::GlobalTarget
                    || !is_in_build_system(target)
                {
                    continue;
                }
                match target.props.get("EXCLUDE_FROM_ALL") {
                    Some(value) => {
                        let value = self.ctx.genex(value, Some(target), None);
                        if !util::is_on(&value) {
                            // Explicitly un-excluded: every enclosing
                            // directory's rule depends on it too.
                            let mut parent = interp.makefiles[index].parent;
                            while let Some(dir) = parent {
                                dir_targets[dir].push(target);
                                parent = interp.makefiles[dir].parent;
                            }
                            dir_targets[index].push(target);
                        }
                    }
                    None => dir_targets[index].push(target),
                }
            }
        }
        let mut order: Vec<usize> = (0..count).collect();
        order.sort_by(|a, b| {
            interp.makefiles[*a]
                .binary_dir
                .cmp(&interp.makefiles[*b].binary_dir)
        });
        // Codegen targets (CMP0171).
        if interp.ext.misc.contains_key("allow-global-target-codegen") {
            for &index in &order {
                let makefile = &interp.makefiles[index];
                write_divider(&mut out);
                let mut build = Build::new("phony");
                build.comment = format!("Folder: {}", makefile.binary_dir);
                build
                    .outputs
                    .push(self.ninja_path(&format!("{}/codegen", makefile.binary_dir)));
                for target in &dir_targets[index] {
                    for source in gt::sources(self.ctx, target, &self.config).iter() {
                        if source.kind != SourceKind::CustomCommandOutput {
                            continue;
                        }
                        let Some(cc) = self.attached_command(target.makefile, &source.path) else {
                            continue;
                        };
                        let command = self.command(cc);
                        if command.codegen {
                            build.explicit_deps.extend(
                                command.outputs.iter().map(|output| self.ninja_path(output)),
                            );
                        }
                    }
                }
                for &child in &makefile.children {
                    let excluded = interp.makefiles[child]
                        .props
                        .get("EXCLUDE_FROM_ALL")
                        .is_some_and(util::is_on);
                    if !excluded {
                        build.explicit_deps.push(self.ninja_path(&format!(
                            "{}/codegen",
                            interp.makefiles[child].binary_dir
                        )));
                    }
                }
                write_build(&mut out, &build, 0);
            }
        }
        for index in order {
            let makefile = &interp.makefiles[index];
            write_divider(&mut out);
            let mut build = Build::new("phony");
            build.comment = format!("Folder: {}", makefile.binary_dir);
            build
                .outputs
                .push(self.ninja_path(&format!("{}/all", makefile.binary_dir)));
            for target in &dir_targets[index] {
                self.append_target_outputs(
                    target,
                    &mut build.explicit_deps,
                    DependOn::TargetArtifact,
                );
            }
            for &child in &makefile.children {
                let excluded = interp.makefiles[child]
                    .props
                    .get("EXCLUDE_FROM_ALL")
                    .is_some_and(util::is_on);
                if !excluded {
                    build.explicit_deps.push(
                        self.ninja_path(&format!("{}/all", interp.makefiles[child].binary_dir)),
                    );
                }
            }
            write_build(&mut out, &build, 0);
        }
        self.build.push_str(&out);
    }

    fn write_builtin_targets(&mut self) {
        let mut out = String::new();
        write_divider(&mut out);
        out.push_str("# Built-in targets\n\n");
        self.write_rebuild_manifest(&mut out);
        self.write_target_clean(&mut out);
        self.write_target_help(&mut out);
        write_comment(&mut out, "Make the all target the default.");
        out.push_str("default all\n");
        if self.install_target_enabled
            && self
                .ctx
                .interp
                .global_props
                .get("INSTALL_PARALLEL")
                .is_some_and(util::is_on)
            && !self
                .ctx
                .var(0, "CMAKE_SKIP_INSTALL_RULES")
                .is_some_and(|value| util::is_on(&value))
        {
            let mut build = Build::new("phony");
            build.comment = "Install every subdirectory in parallel".into();
            build.outputs.push("install/parallel".into());
            for makefile in &self.ctx.interp.makefiles {
                build
                    .explicit_deps
                    .push(self.ninja_path(&format!("{}/install/local", makefile.binary_dir)));
            }
            write_build(&mut out, &build, 0);
        }
        self.build.push_str(&out);
    }

    fn write_rebuild_manifest(&mut self, out: &mut String) {
        if self
            .ctx
            .var(0, "CMAKE_SUPPRESS_REGENERATION")
            .is_some_and(|value| util::is_on(&value))
        {
            return;
        }
        let rule = Rule {
            name: "RERUN_CMAKE".into(),
            command: format!(
                "{} --regenerate-during-build -S{} -B{}",
                self.cmake_cmd,
                self.shell(&self.home_source),
                self.shell(&self.home_binary)
            ),
            description: "Re-running CMake...".into(),
            comment: "Rule for re-running cmake.".into(),
            generator: true,
            ..Rule::default()
        };
        write_rule(&mut self.rules, &rule);
        self.rule_names.insert(rule.name.clone());
        let mut rebuild = Build::new("RERUN_CMAKE");
        rebuild.comment = "Re-run CMake if any of its inputs changed.".into();
        rebuild.outputs.push("build.ninja".into());
        for makefile in &self.ctx.interp.makefiles {
            rebuild
                .outputs
                .push(format!("{}/cmake_install.cmake", makefile.binary_dir));
        }
        for makefile in &self.ctx.interp.makefiles {
            rebuild
                .outputs
                .push(format!("{}/CTestTestfile.cmake", makefile.binary_dir));
        }
        rebuild.outputs = self.order_rebuild_outputs(rebuild.outputs);
        let mut deps: Vec<String> = Vec::new();
        for makefile in &self.ctx.interp.makefiles {
            for file in &makefile.list_files {
                deps.push(self.ninja_path(file));
            }
        }
        for extra in self
            .ctx
            .interp
            .ext
            .misc
            .get("cmake-depend-files")
            .map(|v| util::expand_list(v, false))
            .unwrap_or_default()
        {
            deps.push(self.ninja_path(&extra));
        }
        deps.push("CMakeCache.txt".into());
        rebuild.variables.insert("pool".into(), "console".into());
        deps.sort();
        deps.dedup();
        rebuild.implicit_deps = deps.clone();
        write_build(out, &rebuild, 0);
        let mut phony = Build::new("phony");
        phony.comment = "A missing CMake input file is not an error.".into();
        phony.outputs = deps
            .into_iter()
            .filter(|dep| !self.custom_command_outputs.contains(dep))
            .collect();
        write_build(out, &phony, 0);
    }

    /// `cmGlobalNinjaGenerator::AddRebuildManifestOutputs`: build.ninja then
    /// each directory's cmake_install.cmake and CTestTestfile.cmake, in the
    /// order the C++ adds them (per local generator: install, then test).
    fn order_rebuild_outputs(&self, _outputs: Vec<String>) -> Vec<String> {
        // AddRebuildManifestOutputs: build.ninja, then
        // cmGlobalGenerator::AddCMakeFilesToRebuild (install scripts of all
        // directories, then test files of directories with testing on).
        let mut outputs = vec!["build.ninja".to_owned()];
        let skip_install = self
            .ctx
            .var(0, "CMAKE_SKIP_INSTALL_RULES")
            .is_some_and(|value| util::is_on(&value));
        let count = self.ctx.interp.makefiles.len();
        if !skip_install {
            for index in 0..count {
                outputs.push(format!(
                    "{}/cmake_install.cmake",
                    self.ctx.interp.makefiles[index].binary_dir
                ));
            }
        }
        for index in 0..count {
            if self
                .ctx
                .var(index, "CMAKE_TESTING_ENABLED")
                .is_some_and(|value| util::is_on(&value))
            {
                outputs.push(format!(
                    "{}/CTestTestfile.cmake",
                    self.ctx.interp.makefiles[index].binary_dir
                ));
            }
        }
        outputs
    }

    fn write_target_clean(&mut self, out: &mut String) {
        let rule = Rule {
            name: "CLEAN".into(),
            command: format!("{} $FILE_ARG -t clean $TARGETS", self.ninja_cmd),
            description: "Cleaning all built files...".into(),
            comment: "Rule for cleaning all built files.".into(),
            ..Rule::default()
        };
        write_rule(&mut self.rules, &rule);
        let mut build = Build::new("CLEAN");
        build.comment = "Clean all the built files.".into();
        build.outputs.push("clean".into());
        write_build(out, &build, 0);
    }

    fn write_target_help(&mut self, out: &mut String) {
        let rule = Rule {
            name: "HELP".into(),
            command: format!("{} -t targets", self.ninja_cmd),
            description: "All primary targets available:".into(),
            comment: "Rule for printing all primary targets available.".into(),
            ..Rule::default()
        };
        write_rule(&mut self.rules, &rule);
        let mut build = Build::new("HELP");
        build.comment = "Print all primary targets available.".into();
        build.outputs.push("help".into());
        write_build(out, &build, 0);
    }
}

/// `cmGeneratorTarget::IsInBuildSystem`.
/// `cmGeneratorTarget::IsInBuildSystem`.
pub fn is_in_build_system(target: &Target) -> bool {
    !target.imported
        && match target.kind {
            // An INTERFACE library is in the build system if it has SOURCES
            // or header/C++ module file sets.
            TargetKind::InterfaceLibrary => ["SOURCES", "HEADER_SETS", "CXX_MODULE_SETS"]
                .iter()
                .any(|prop| {
                    target
                        .props
                        .get(prop)
                        .is_some_and(|value| !value.is_empty())
                }),
            TargetKind::UnknownLibrary => false,
            _ => true,
        }
}

impl<'a> NinjaGenerator<'a> {
    fn object_dir(&self, target: &Target) -> String {
        gt::object_directory(self.ctx, target, &self.config)
            .trim_end_matches('/')
            .to_owned()
    }

    /// `cmNinjaTargetGenerator::LanguageCompilerRule`.
    fn compiler_rule_name(&self, language: &str, target: &Target) -> String {
        format!(
            "{}_COMPILER__{}_unscanned_{}",
            language,
            encode_rule_name(&target.name),
            self.config
        )
    }

    /// `cmNinjaNormalTargetGenerator::LanguageLinkerRule`.
    fn linker_rule_name(&self, language: &str, target: &Target) -> String {
        format!(
            "{}_{}_LINKER__{}_{}",
            language,
            target.kind.type_name(),
            encode_rule_name(&target.name),
            self.config
        )
    }

    /// Writes the compile rule for one language (cmNinjaTargetGenerator::WriteCompileRule,
    /// without dependency scanning).
    fn write_compile_rule(&mut self, language: &str, target: &Target) {
        let name = self.compiler_rule_name(language, target);
        if self.has_rule(&name) {
            return;
        }
        let makefile = target.makefile;
        let mut rule = Rule {
            name,
            ..Rule::default()
        };
        let mut flags_text = "$FLAGS".to_owned();
        let depfile_format = self
            .ctx
            .var_safe(makefile, &format!("CMAKE_{language}_DEPFILE_FORMAT"));
        if depfile_format == "msvc" {
            rule.deptype = "msvc".into();
        } else {
            rule.deptype = "gcc".into();
            rule.depfile = "$DEP_FILE".into();
        }
        let depfile_flags = self
            .ctx
            .var_safe(makefile, &format!("CMAKE_DEPFILE_FLAGS_{language}"));
        if !depfile_flags.is_empty() {
            let expanded = flags::expand_rule(
                self.ctx,
                target,
                &depfile_flags,
                &[
                    ("DEP_FILE", rule.depfile.clone()),
                    ("DEP_TARGET", "$out".into()),
                ],
            );
            flags_text.push(' ');
            flags_text.push_str(&expanded);
        }
        let compile = self
            .ctx
            .var_safe(makefile, &format!("CMAKE_{language}_COMPILE_OBJECT"));
        let mut commands = util::expand_list(&compile, false);
        if let Some(first) = commands.first_mut() {
            first.insert_str(0, "${LAUNCHER}${CODE_CHECK}");
        }
        let extra = self.ctx.var_safe(
            makefile,
            &format!("CMAKE_{language}_DEPENDS_EXTRA_COMMANDS"),
        );
        commands.extend(util::expand_list(&extra, false));
        let launcher = rule_launcher(self.ctx, target, "RULE_LAUNCH_COMPILE");
        let vars: Vec<(&str, String)> = vec![
            ("TARGET_NAME", target.name.clone()),
            ("TARGET_TYPE", target.kind.type_name().to_owned()),
            ("LANGUAGE", language.to_owned()),
            ("SOURCE", "$in".into()),
            ("OBJECT", "$out".into()),
            ("DEFINES", "$DEFINES".into()),
            ("INCLUDES", "$INCLUDES".into()),
            ("TARGET_PDB", "$TARGET_PDB".into()),
            ("TARGET_COMPILE_PDB", "$TARGET_COMPILE_PDB".into()),
            ("OBJECT_DIR", "$OBJECT_DIR".into()),
            ("OBJECT_FILE_DIR", "$OBJECT_FILE_DIR".into()),
            ("CONFIG", "$CONFIG".into()),
            ("FLAGS", flags_text),
            ("DEP_FILE", rule.depfile.clone()),
            ("DEP_TARGET", "$out".into()),
        ];
        let expanded: Vec<String> = commands
            .iter()
            .map(|command| {
                flags::expand_rule(self.ctx, target, &format!("{launcher}{command}"), &vars)
            })
            .collect();
        rule.command = self.build_command_line(&expanded);
        rule.comment = format!("Rule for compiling {language} files.");
        rule.description = format!("Building {language} object $out");
        self.add_rule(&rule);
    }

    /// `cmNinjaNormalTargetGenerator::Generate`.
    fn generate_normal_target(&mut self, target: &'a Target) {
        let config = self.config.clone();
        let link_language = gt::linker_language(self.ctx, target, &config);
        if link_language.is_empty() {
            crate::configure::system_error(
                None,
                &format!(
                    "CMake can not determine linker language for target: {}",
                    target.name
                ),
            );
            return;
        }
        let sources = gt::sources(self.ctx, target, &config);
        // WriteLanguagesRules: compile rules for every object language.
        let mut languages: Vec<String> = Vec::new();
        for source in sources.iter() {
            if source.kind == SourceKind::ObjectSource && !languages.contains(&source.language) {
                languages.push(source.language.clone());
            }
        }
        languages.sort();
        // cmCommonTargetGenerator::HaveRequiredLanguages: the first object
        // language (in sorted order) that was never enabled is a fatal
        // error reported against the directory's list-file scope, which
        // cmMakefile::Configure leaves on the backtrace for the generate
        // step.
        if let Some(language) = languages
            .iter()
            .find(|language| !self.ctx.interp.langs.enabled.contains(*language))
        {
            let directory = &self.ctx.interp.makefiles[target.makefile];
            let file = directory
                .list_files
                .first()
                .cloned()
                .unwrap_or_else(|| format!("{}/CMakeLists.txt", directory.source_dir));
            self.ctx.error(
                vec![crate::messages::Frame {
                    file: std::rc::Rc::from(file.as_str()),
                    line: 0,
                    name: None,
                    deferred: false,
                }],
                format!(
                    "The language {language} was requested for compilation but was not enabled. To enable a language it needs to be specified in a 'project' or 'enable_language' command in the root CMakeLists.txt"
                ),
            );
        }
        for language in &languages {
            self.write_compile_rule(language, target);
        }
        // WriteObjectBuildStatements
        let mut out = String::new();
        write_divider(&mut out);
        let _ = write!(
            out,
            "# Object build statements for {} target {}\n\n",
            target.kind.type_name(),
            target.name
        );
        // GetCustomCommands: register each attached command with this target.
        let mut custom_outputs: Vec<String> = Vec::new();
        for source in sources.iter() {
            if source.kind != SourceKind::CustomCommandOutput {
                continue;
            }
            if let Some(cc) = self.attached_command(target.makefile, &source.path) {
                self.add_custom_command_target(cc, target);
                let evaluated = self.evaluate_command(self.command(cc));
                custom_outputs.extend(evaluated.outputs);
                custom_outputs.extend(evaluated.byproducts);
            }
        }
        let object_dir = self.object_dir(target);
        let _ = std::fs::create_dir_all(&object_dir);
        let order_target = format!("cmake_object_order_depends_target_{}", target.name);
        {
            let mut build = Build::new("phony");
            build.comment = format!("Order-only phony target for {}", target.name);
            build.outputs.push(order_target.clone());
            let mut order = Vec::new();
            self.append_target_depends(target, &mut order, DependOn::TargetOrdering);
            // Order-only dependencies on custom command outputs.
            // INTEGRATION: CMP0154 private generated sources of file sets.
            order.extend(custom_outputs.iter().map(|path| self.ninja_path(path)));
            order.sort();
            order.dedup();
            if order.is_empty() {
                order.push(".".into());
            }
            build.order_only_deps = order;
            write_build(&mut out, &build, 0);
        }
        // GetExternalObjects precede the compiled objects.
        let pch_extension = self.ctx.var(target.makefile, "CMAKE_PCH_EXTENSION");
        let mut objects: Vec<String> = sources
            .iter()
            .filter(|source| source.kind == SourceKind::ExternalObject)
            .map(|source| self.ninja_path(&source.path))
            .filter(|object| {
                pch_extension
                    .as_ref()
                    .is_none_or(|extension| !object.ends_with(extension))
            })
            .collect();
        for source in sources.iter() {
            if source.kind != SourceKind::ObjectSource {
                continue;
            }
            let object = self.ninja_path(&format!("{object_dir}/{}", source.object_name));
            let object_file_dir = util::filename_path(&object);
            // The cmSourceFile's own properties (COMPILE_FLAGS, OBJECT_DEPENDS...).
            let source_props = crate::commands::target::find_known_source(
                self.ctx.interp,
                target.makefile,
                &source.path,
            )
            .map(|index| {
                self.ctx.interp.ext.sources[&target.makefile][index]
                    .props
                    .clone()
            })
            .unwrap_or_default();
            let vars = flags::compile_vars_with_source_props(
                self.ctx,
                target,
                &config,
                source,
                &source_props,
            );
            let mut build = Build::new(&self.compiler_rule_name(&source.language, target));
            build.outputs.push(object.clone());
            // cmNinjaTargetGenerator::GetCompiledSourceNinjaPath: sources are
            // passed to the compiler by absolute path.
            build.explicit_deps.push(self.ninja_abs_path(&source.path));
            // Precompiled header dependencies.
            let pch_source = super::pch::pch_source(self.ctx, target, &config, &source.language);
            if !pch_source.is_empty() && source_props.get("SKIP_PRECOMPILE_HEADERS").is_none() {
                let header = super::pch::pch_header(self.ctx, target, &config, &source.language);
                build.implicit_deps.push(self.ninja_path(&header));
                if source.path != pch_source {
                    let file = super::pch::pch_file(self.ctx, target, &config, &source.language);
                    build.implicit_deps.push(self.ninja_path(&file));
                }
            }
            if let Some(depends) = source_props.get("OBJECT_DEPENDS") {
                for dep in util::expand_list(depends, false) {
                    let dep = if util::is_full_path(&dep) {
                        util::collapse_full_path(&dep, "")
                    } else {
                        dep
                    };
                    build.implicit_deps.push(self.ninja_path(&dep));
                }
            }
            build.order_only_deps.push(order_target.clone());
            build.variables.insert("FLAGS".into(), vars.flags.clone());
            build.variables.insert("DEFINES".into(), vars.defines);
            build.variables.insert("INCLUDES".into(), vars.includes);
            build.variables.insert("CONFIG".into(), config.clone());
            let depfile_format = self.ctx.var_safe(
                target.makefile,
                &format!("CMAKE_{}_DEPFILE_FORMAT", source.language),
            );
            if depfile_format != "msvc" {
                build
                    .variables
                    .insert("DEP_FILE".into(), format!("{object}.d"));
            }
            build.variables.insert(
                "LAUNCHER".into(),
                flags::compiler_launcher(self.ctx, target, &source.language),
            );
            let object_dir_rel = self.ninja_path(&object_dir);
            build.variables.insert("OBJECT_DIR".into(), object_dir_rel);
            build
                .variables
                .insert("OBJECT_FILE_DIR".into(), object_file_dir);
            self.set_msvc_target_pdb_variable(target, &mut build.variables);
            let mut export_vars = build.variables.clone();
            export_vars.insert("FLAGS".into(), vars.flags_untrimmed.clone());
            self.export_object_compile_command(target, source, &object, &export_vars);
            write_build(&mut out, &build, 0);
            // PCH objects are not linked.
            let is_pch = self
                .ctx
                .var(target.makefile, "CMAKE_PCH_EXTENSION")
                .is_some_and(|extension| object.ends_with(&extension));
            if !is_pch {
                objects.push(object);
            }
        }
        out.push('\n');
        self.build.push_str(&out);
        self.objects.insert(target.name.clone(), objects.clone());
        if target.kind == TargetKind::ObjectLibrary {
            let mut build = Build::new("phony");
            build.comment = format!("Object library {}", target.name);
            let prefix = self.target_dir_prefix(target.makefile);
            build.outputs.push(format!("{prefix}{}", target.name));
            build.explicit_deps = objects;
            self.write_build(&build);
            self.add_target_alias(&target.name.clone(), target);
            return;
        }
        self.write_link_statement(target, &link_language, &objects);
    }

    /// `cmNinjaTargetGenerator::SetMsvcTargetPdbVariable`. The gate is the
    /// mere definition of `MSVC_<LANG>_ARCHITECTURE_ID`: compiler
    /// identification raises it (possibly empty) into the enabling scope.
    fn set_msvc_target_pdb_variable(
        &self,
        target: &Target,
        vars: &mut BTreeMap<String, String>,
    ) -> bool {
        let makefile = target.makefile;
        let defined = [
            "MSVC_C_ARCHITECTURE_ID",
            "MSVC_CXX_ARCHITECTURE_ID",
            "MSVC_CUDA_ARCHITECTURE_ID",
        ]
        .iter()
        .any(|name| self.ctx.var(makefile, name).is_some());
        if !defined {
            return false;
        }
        let config = self.config.clone();
        let compile_pdb = gt::target_compile_pdb(self.ctx, target, &config);
        let pdb = if matches!(
            target.kind,
            TargetKind::Executable
                | TargetKind::StaticLibrary
                | TargetKind::SharedLibrary
                | TargetKind::ModuleLibrary
        ) {
            let names = gt::names(self.ctx, target, &config);
            format!(
                "{}/{}",
                gt::pdb_directory(self.ctx, target, &config),
                names.pdb
            )
        } else {
            String::new()
        };
        vars.insert("TARGET_PDB".into(), self.shell(&self.ninja_path(&pdb)));
        vars.insert(
            "TARGET_COMPILE_PDB".into(),
            self.shell(&self.ninja_path(&compile_pdb)),
        );
        true
    }

    /// `cmNinjaNormalTargetGenerator::WriteLinkRule` +
    /// `WriteLinkStatement` (no device link, no Apple).
    fn write_link_statement(&mut self, target: &'a Target, language: &str, objects: &[String]) {
        let config = self.config.clone();
        let makefile = target.makefile;
        let names = gt::names(self.ctx, target, &config);
        let directory = gt::directory(self.ctx, target, &config, ArtifactType::Runtime);
        let target_output = self.ninja_path(&format!("{directory}/{}", names.output));
        let target_real = self.ninja_path(&format!("{directory}/{}", names.real));
        let rule_name = self.linker_rule_name(language, target);
        let link_command_var = match target.kind {
            TargetKind::Executable => format!("CMAKE_{language}_LINK_EXECUTABLE"),
            TargetKind::StaticLibrary => format!("CMAKE_{language}_ARCHIVE_CREATE"),
            TargetKind::SharedLibrary => format!("CMAKE_{language}_CREATE_SHARED_LIBRARY"),
            TargetKind::ModuleLibrary => format!("CMAKE_{language}_CREATE_SHARED_MODULE"),
            _ => String::new(),
        };
        // cmGeneratorTarget::HasLinkDependencyFile.
        let link_depfile = flags::link_dependency_file(self.ctx, target, language);
        let use_rsp = false;
        if !self.has_rule(&rule_name) {
            let mut rule = Rule {
                name: rule_name.clone(),
                ..Rule::default()
            };
            if link_depfile.is_some() {
                rule.deptype = self
                    .ctx
                    .var_safe(makefile, &format!("CMAKE_{language}_LINKER_DEPFILE_FORMAT"));
                rule.depfile = "$DEP_FILE".into();
            }
            let commands: Vec<String> = if target.kind == TargetKind::StaticLibrary {
                // cmNinjaNormalTargetGenerator::ComputeLinkCmd for static libs:
                // remove the old archive first, then create/append/finish.
                let mut list = vec![format!("{} -E rm -f <TARGET>", self.cmake_cmd)];
                let create = self
                    .ctx
                    .var_safe(makefile, &format!("CMAKE_{language}_ARCHIVE_CREATE"));
                list.extend(util::expand_list(&create, false));
                let finish = self
                    .ctx
                    .var_safe(makefile, &format!("CMAKE_{language}_ARCHIVE_FINISH"));
                list.extend(util::expand_list(&finish, false));
                list
            } else {
                util::expand_list(&self.ctx.var_safe(makefile, &link_command_var), false)
            };
            let launcher = rule_launcher(self.ctx, target, "RULE_LAUNCH_LINK");
            let mut vars: Vec<(&str, String)> = vec![
                ("TARGET_NAME", target.name.clone()),
                ("TARGET_TYPE", target.kind.type_name().to_owned()),
                ("LANGUAGE", language.to_owned()),
                ("OBJECTS", "$in".into()),
                ("LINK_LIBRARIES", "$LINK_PATH $LINK_LIBRARIES".into()),
                ("OBJECT_DIR", "$OBJECT_DIR".into()),
                ("TARGET", "$TARGET_FILE".into()),
                ("SONAME_FLAG", "$SONAME_FLAG".into()),
                ("TARGET_SONAME", "$SONAME".into()),
                ("TARGET_INSTALLNAME_DIR", "$INSTALLNAME_DIR".into()),
                ("TARGET_PDB", "$TARGET_PDB".into()),
                ("FLAGS", "$FLAGS".into()),
                ("LINK_FLAGS", "$LINK_FLAGS".into()),
                ("MANIFESTS", "$MANIFESTS".into()),
                ("CONFIG", "$CONFIG".into()),
                ("TARGET_VERSION_MAJOR", target_version(target).0),
                ("TARGET_VERSION_MINOR", target_version(target).1),
            ];
            if target.kind != TargetKind::Executable {
                vars.push((
                    "LANGUAGE_COMPILE_FLAGS",
                    "$LANGUAGE_COMPILE_FLAGS $ARCH_FLAGS".into(),
                ));
            }
            let mut expanded: Vec<String> = commands
                .iter()
                .map(|command| {
                    flags::expand_rule(self.ctx, target, &format!("{launcher}{command}"), &vars)
                })
                .filter(|command| command.trim() != ":")
                .collect();
            expanded.insert(0, "$PRE_LINK".into());
            expanded.push("$POST_BUILD".into());
            rule.command = self.build_command_line(&expanded);
            let visible = visible_type_name(target.kind);
            rule.comment = format!("Rule for linking {language} {visible}.");
            rule.description = format!("Linking {language} {visible} $TARGET_FILE");
            rule.restat = "$RESTAT".into();
            self.add_rule(&rule);
        }
        if names.output != names.real {
            let (rule_name, command, description, comment) =
                if target.kind == TargetKind::Executable {
                    (
                        "CMAKE_SYMLINK_EXECUTABLE",
                        format!(
                            "{} -E cmake_symlink_executable $in $out && $POST_BUILD",
                            self.cmake_cmd
                        ),
                        "Creating executable symlink $out",
                        "Rule for creating executable symlink.",
                    )
                } else {
                    (
                        "CMAKE_SYMLINK_LIBRARY",
                        format!(
                            "{} -E cmake_symlink_library $in $SONAME $out && $POST_BUILD",
                            self.cmake_cmd
                        ),
                        "Creating library symlink $out",
                        "Rule for creating library symlink.",
                    )
                };
            self.add_rule(&Rule {
                name: rule_name.into(),
                command,
                description: description.into(),
                comment: comment.into(),
                ..Rule::default()
            });
        }
        // Link statement.
        let mut out = String::new();
        write_divider(&mut out);
        let _ = write!(
            out,
            "# Link build statements for {} target {}\n\n",
            target.kind.type_name(),
            target.name
        );
        let mut build = Build::new(&rule_name);
        let visible = visible_type_name(target.kind);
        build.comment = format!("Link the {visible} {}", target_real);
        build.outputs.push(target_real.clone());
        build.explicit_deps = objects.to_vec();
        let link_vars = flags::link_flag_vars(self.ctx, target, &config, language);
        let mut shared_symlinks: Vec<String> = Vec::new();
        let object_dir = self.ninja_path(&self.object_dir(target));
        if target.kind != TargetKind::StaticLibrary {
            let line = link::link_line(self.ctx, target, &config, language);
            build
                .variables
                .insert("LINK_LIBRARIES".into(), line.libraries);
            build.variables.insert("LINK_PATH".into(), line.link_path);
            build.implicit_deps = line
                .depends
                .iter()
                .map(|dep| self.ninja_path(dep))
                .collect();
            shared_symlinks = line
                .shared_symlinks
                .iter()
                .map(|dep| self.ninja_path(dep))
                .collect();
        }
        if target.kind == TargetKind::Executable {
            build.variables.insert("FLAGS".into(), link_vars.flags);
        } else {
            build.variables.insert(
                "LANGUAGE_COMPILE_FLAGS".into(),
                link_vars.language_compile_flags,
            );
            build
                .variables
                .insert("ARCH_FLAGS".into(), link_vars.arch_flags);
        }
        // cmNinjaTargetGenerator::AddDepfileBinding.
        if let Some(depfile) = &link_depfile {
            let depfile_shell = self.shell(depfile);
            if depfile_shell != *depfile {
                build.variables.insert("depfile".into(), depfile.clone());
            }
            build.variables.insert("DEP_FILE".into(), depfile_shell);
        }
        build
            .variables
            .insert("LINK_FLAGS".into(), link_vars.link_flags);
        build.variables.insert("CONFIG".into(), config.clone());
        build
            .variables
            .insert("OBJECT_DIR".into(), object_dir.clone());
        build
            .variables
            .insert("TARGET_FILE".into(), target_real.clone());
        // Build-event commands: PRE_BUILD and PRE_LINK run before the link,
        // POST_BUILD after it.
        let mut byproducts = CcOutputs::default();
        let mut pre_link: Vec<String> = Vec::new();
        let mut post_build: Vec<String> = Vec::new();
        for stage in [
            CustomCommandKind::PreBuild,
            CustomCommandKind::PreLink,
            CustomCommandKind::PostBuild,
        ] {
            for command in target
                .custom_commands
                .iter()
                .filter(|command| command.kind == stage)
            {
                let evaluated = self.evaluate_command(command);
                let lines = self.custom_command_lines(&evaluated, makefile);
                if stage == CustomCommandKind::PostBuild {
                    post_build.extend(lines);
                } else {
                    pre_link.extend(lines);
                }
                self.add_cc_outputs(&mut byproducts, &evaluated.byproducts);
            }
        }
        // After PRE_LINK commands, go back to CMAKE_BINARY_DIR for the link.
        if !pre_link.is_empty() {
            pre_link.push(format!("cd {}", self.shell(&self.home_binary)));
        }
        build.variables.insert(
            "PRE_LINK".into(),
            self.build_command_line_step(&pre_link, "pre-link", Some(target)),
        );
        let post_build_line = self.build_command_line_step(&post_build, "post-build", Some(target));
        let symlink_needed = names.output != names.real;
        build.variables.insert(
            "POST_BUILD".into(),
            if symlink_needed {
                SHELL_NOOP.to_owned()
            } else {
                post_build_line.clone()
            },
        );
        if matches!(
            target.kind,
            TargetKind::SharedLibrary | TargetKind::ModuleLibrary
        ) && gt::has_soname(self.ctx, target, &config)
        {
            build.variables.insert(
                "SONAME_FLAG".into(),
                self.ctx.var_safe(
                    makefile,
                    &format!("CMAKE_SHARED_LIBRARY_SONAME_{language}_FLAG"),
                ),
            );
            build
                .variables
                .insert("SONAME".into(), names.shared_object.clone());
        }
        if !names.import_library.is_empty() {
            let implib = gt::full_path(self.ctx, target, &config, ArtifactType::Import, false);
            build.variables.insert(
                "TARGET_IMPLIB".into(),
                self.shell(&self.ninja_path(&implib)),
            );
        }
        if !self.set_msvc_target_pdb_variable(target, &mut build.variables) {
            // It is common to place debug symbols at a specific place, so a
            // plain target name is made available to the rule.
            let parts = gt::full_name_components(self.ctx, target, &config, ArtifactType::Runtime);
            let suffix = self
                .ctx
                .var(makefile, "CMAKE_DEBUG_SYMBOL_SUFFIX")
                .unwrap_or_else(|| ".dbg".to_owned());
            build.variables.insert(
                "TARGET_PDB".into(),
                format!("{}{}{suffix}", parts.base, parts.suffix),
            );
        }
        // Order-only deps: other targets' outputs.
        let mut order = Vec::new();
        self.append_target_depends(target, &mut order, DependOn::TargetArtifact);
        // Order-only dependencies on versioning symlinks of linked shared
        // libraries (runtime binaries only).
        for lib in shared_symlinks {
            if !build.implicit_deps.contains(&lib) {
                order.push(lib);
            }
        }
        build.order_only_deps = order;
        // Ninja should restat after linking if and only if there are byproducts.
        if !byproducts.explicit.is_empty() {
            build.variables.insert("RESTAT".into(), "1".into());
        }
        build.outputs.extend(byproducts.explicit);
        build.workdir_outs = byproducts.workdir;
        let limit = if use_rsp {
            -1
        } else {
            command_line_length_limit()
        };
        let _ = write_build(&mut out, &build, limit);
        // Symlinks.
        if names.output != names.real {
            let mut symlink = Build::new(if target.kind == TargetKind::Executable {
                "CMAKE_SYMLINK_EXECUTABLE"
            } else {
                "CMAKE_SYMLINK_LIBRARY"
            });
            if target.kind == TargetKind::Executable {
                symlink.comment = format!("Create executable symlink {target_output}");
                symlink.outputs.push(target_output.clone());
            } else {
                symlink.comment = format!("Create library symlink {target_output}");
                let so = self.ninja_path(&format!("{directory}/{}", names.shared_object));
                if target_real == so || target_output == so {
                    symlink.variables.insert("SONAME".into(), self.shell(&so));
                } else {
                    symlink.outputs.push(so.clone());
                }
                symlink.outputs.push(target_output.clone());
            }
            symlink.explicit_deps.push(target_real.clone());
            symlink
                .variables
                .insert("POST_BUILD".into(), post_build_line);
            write_build(&mut out, &symlink, 0);
        }
        self.build.push_str(&out);
        // Add aliases for the file name and the target name.
        self.add_target_alias(&names.output.clone(), target);
        self.add_target_alias(&target.name.clone(), target);
    }

    /// `cmNinjaTargetGenerator::ExportObjectCompileCommand` +
    /// `cmGlobalNinjaGenerator::AddCXXCompileCommand`.
    fn export_object_compile_command(
        &mut self,
        target: &Target,
        source: &gt::SourceInfo,
        object: &str,
        vars: &BTreeMap<String, String>,
    ) {
        if !target
            .props
            .get("EXPORT_COMPILE_COMMANDS")
            .is_some_and(util::is_on)
        {
            return;
        }
        let language = &source.language;
        let var = |name: &str| vars.get(name).cloned().unwrap_or_default();
        let rule_vars: Vec<(&str, String)> = vec![
            ("LANGUAGE", language.clone()),
            ("SOURCE", self.shell(&source.path)),
            ("OBJECT", self.shell(object)),
            ("OBJECT_DIR", var("OBJECT_DIR")),
            ("OBJECT_FILE_DIR", var("OBJECT_FILE_DIR")),
            ("FLAGS", var("FLAGS")),
            ("DEFINES", var("DEFINES")),
            ("INCLUDES", var("INCLUDES")),
            ("TARGET_COMPILE_PDB", var("TARGET_COMPILE_PDB")),
            ("TARGET_PDB", var("TARGET_PDB")),
        ];
        let template = self
            .ctx
            .var_safe(target.makefile, &format!("CMAKE_{language}_COMPILE_OBJECT"));
        // No launcher for CMAKE_EXPORT_COMPILE_COMMANDS.
        let commands: Vec<String> = util::expand_list(&template, false)
            .iter()
            .map(|command| flags::expand_rule(self.ctx, target, command, &rule_vars))
            .collect();
        let command = self.build_command_line(&commands);
        let home = &self.home_binary;
        let escape = |text: &str| {
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
        };
        self.compile_commands.push(format!(
            "{{\n  \"directory\": \"{}\",\n  \"command\": \"{}\",\n  \"file\": \"{}\",\n  \"output\": \"{}\"\n}}",
            escape(home),
            escape(&command),
            escape(&util::collapse_full_path(&source.path, home)),
            escape(&util::collapse_full_path(object, home)),
        ));
    }

    /// `cmGlobalGenerator::WriteSummary(target)`: Labels.txt/Labels.json in
    /// the target's support directory when any labels apply.
    fn write_labels(&self, target: &Target, directory: &str) -> std::io::Result<()> {
        use crate::commands::string::json::{Json, write_json_stream};
        let interp = self.ctx.interp;
        let file = format!("{directory}/Labels.txt");
        let json_file = format!("{directory}/Labels.json");
        let target_labels = target.props.get("LABELS");
        let directory_labels = interp.makefiles[target.makefile].props.get("LABELS");
        let cmake_directory_labels = self.ctx.var(target.makefile, "CMAKE_DIRECTORY_LABELS");
        if target_labels.is_none() && directory_labels.is_none() && cmake_directory_labels.is_none()
        {
            let _ = std::fs::remove_file(&file);
            let _ = std::fs::remove_file(&json_file);
            return Ok(());
        }
        let text = |value: &str| Json::Str(value.as_bytes().to_vec());
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
        let mut sources = Vec::new();
        let mut seen = HashSet::new();
        for source in gt::sources(self.ctx, target, &self.config).iter() {
            if !seen.insert(source.path.clone()) {
                continue;
            }
            out.push_str(&source.path);
            out.push('\n');
            let mut entry = std::collections::BTreeMap::new();
            entry.insert(b"file".to_vec(), text(&source.path));
            let labels =
                crate::commands::target::find_known_source(interp, target.makefile, &source.path)
                    .and_then(|index| {
                        interp.ext.sources[&target.makefile][index]
                            .props
                            .get("LABELS")
                            .map(str::to_owned)
                    });
            if let Some(labels) = labels {
                let mut list = Vec::new();
                for label in util::expand_list(&labels, false) {
                    out.push_str(&format!(" {label}\n"));
                    list.push(text(&label));
                }
                entry.insert(b"labels".to_vec(), Json::Array(list));
            }
            sources.push(Json::Object(entry));
        }
        let mut target_json = std::collections::BTreeMap::new();
        target_json.insert(b"labels".to_vec(), Json::Array(all_labels));
        target_json.insert(b"name".to_vec(), text(&target.name));
        let mut root = std::collections::BTreeMap::new();
        root.insert(b"sources".to_vec(), Json::Array(sources));
        root.insert(b"target".to_vec(), Json::Object(target_json));
        let _ = std::fs::create_dir_all(directory);
        crate::fsutil::write_if_different(std::path::Path::new(&file), out.as_bytes())?;
        crate::fsutil::write_if_different(
            std::path::Path::new(&json_file),
            write_json_stream(&Json::Object(root)).as_bytes(),
        )
    }

    /// `cmNinjaUtilityTargetGenerator::WriteUtilBuildStatements`.
    fn generate_utility_target(&mut self, target: &'a Target) {
        let config = self.config.clone();
        let makefile = target.makefile;
        let binary_dir = self.ctx.interp.makefiles[makefile].binary_dir.clone();
        let util_command_name =
            self.ninja_path(&format!("{binary_dir}/CMakeFiles/{}.util", target.name));
        let mut commands: Vec<String> = Vec::new();
        let mut deps: Vec<String> = Vec::new();
        let mut util_outputs = CcOutputs::default();
        util_outputs.explicit.push(util_command_name.clone());
        let mut command_desc = String::new();
        let mut uses_terminal = false;
        for stage in [CustomCommandKind::PreBuild, CustomCommandKind::PostBuild] {
            for command in target
                .custom_commands
                .iter()
                .filter(|command| command.kind == stage)
            {
                let evaluated = self.evaluate_command(command);
                deps.extend(evaluated.depends.iter().map(|dep| self.ninja_path(dep)));
                commands.extend(self.custom_command_lines(&evaluated, makefile));
                if let Some(comment) = &evaluated.comment {
                    if !command_desc.is_empty() {
                        command_desc.push_str("; ");
                    }
                    command_desc.push_str(comment);
                }
                self.add_cc_outputs(&mut util_outputs, &evaluated.byproducts);
                uses_terminal |= command.uses_terminal;
            }
        }
        for source in gt::sources(self.ctx, target, &config).iter() {
            let Some(cc) = self.attached_command(makefile, &source.path) else {
                continue;
            };
            self.add_custom_command_target(cc, target);
            // Depend on all custom command outputs.
            let evaluated = self.evaluate_command(self.command(cc));
            deps.extend(evaluated.outputs.iter().map(|path| self.ninja_path(path)));
            deps.extend(
                evaluated
                    .byproducts
                    .iter()
                    .map(|path| self.ninja_path(path)),
            );
        }
        let mut phony = Build::new("phony");
        self.append_target_outputs(target, &mut phony.outputs, DependOn::TargetArtifact);
        self.append_target_depends(target, &mut deps, DependOn::TargetArtifact);
        if commands.is_empty() {
            phony.comment = format!("Utility command for {}", target.name);
            phony.explicit_deps = deps;
            self.write_build(&phony);
        } else {
            let mut command = self.build_command_line_step(&commands, "utility", Some(target));
            let desc = if let Some(echo) = target.props.get("EchoString") {
                echo.to_owned()
            } else if !command_desc.is_empty() {
                command_desc
            } else {
                format!("Running utility command for {}", target.name)
            };
            command = command
                .replace("$(CMAKE_SOURCE_DIR)", &self.shell(&self.home_source))
                .replace("$(CMAKE_BINARY_DIR)", &self.shell(&self.home_binary))
                .replace("$(ARGS)", "");
            self.write_custom_command_build(
                &command,
                &desc,
                &format!("Utility command for {}", target.name),
                "",
                "",
                uses_terminal,
                true,
                util_outputs,
                deps,
                Vec::new(),
            );
            phony.explicit_deps.push(util_command_name);
            self.write_build(&phony);
        }
        self.add_target_alias(&target.name.clone(), target);
    }

    /// `cmLocalNinjaGenerator::WriteCustomCommandBuildStatements`.
    fn write_custom_command_build_statements(&mut self, _makefile: usize) {
        let commands = std::mem::take(&mut self.local_custom_commands);
        for (cc, targets) in commands {
            self.write_custom_command_build_statement(cc, &targets);
        }
    }

    /// The custom command attached to the source at `path` in `dir`.
    fn attached_command(&self, dir: usize, path: &str) -> Option<CcRef> {
        let interp = self.ctx.interp;
        let index = crate::commands::target::find_known_source(interp, dir, path)?;
        interp.ext.sources[&dir][index].custom_command.as_ref()?;
        Some((dir, index))
    }

    fn command(&self, cc: CcRef) -> &'a CustomCommand {
        self.ctx.interp.ext.sources[&cc.0][cc.1]
            .custom_command
            .as_ref()
            .expect("attached custom command")
    }

    /// A `cmCustomCommandGenerator` for the build configuration.
    fn evaluate_command(&self, command: &CustomCommand) -> custom::EvaluatedCommand {
        custom::evaluate_spec(self.ctx, &custom::spec_from_command(self.ctx, command))
    }

    /// `cmLocalNinjaGenerator::AddCustomCommandTarget`.
    fn add_custom_command_target(&mut self, cc: CcRef, target: &'a Target) {
        match self
            .local_custom_commands
            .iter_mut()
            .find(|(existing, _)| *existing == cc)
        {
            Some((_, targets)) => {
                if !targets
                    .iter()
                    .any(|existing| std::ptr::eq(*existing, target))
                {
                    targets.push(target);
                }
            }
            None => self.local_custom_commands.push((cc, vec![target])),
        }
    }

    /// `cmLocalNinjaGenerator::AppendCustomCommandLines`.
    fn custom_command_lines(
        &self,
        evaluated: &custom::EvaluatedCommand,
        makefile: usize,
    ) -> Vec<String> {
        let mut lines = Vec::new();
        if evaluated.command_lines.is_empty() {
            return lines;
        }
        let directory = &self.ctx.interp.makefiles[makefile];
        let working_directory = if evaluated.working_directory.is_empty() {
            directory.binary_dir.as_str()
        } else {
            evaluated.working_directory.as_str()
        };
        lines.push(format!("cd {}", self.shell(working_directory)));
        let launcher = self.custom_launcher(evaluated, makefile);
        for (index, argv) in evaluated.command_lines.iter().enumerate() {
            if argv[0].is_empty() {
                continue;
            }
            let mut line = format!("{launcher}{}", self.shell(&argv[0]));
            evaluated.append_arguments(index, &mut line);
            lines.push(line);
        }
        lines
    }

    /// `cmLocalNinjaGenerator::MakeCustomLauncher`.
    fn custom_launcher(&self, evaluated: &custom::EvaluatedCommand, makefile: usize) -> String {
        let interp = self.ctx.interp;
        let Some(launcher) = interp.makefiles[makefile]
            .props
            .get("RULE_LAUNCH_CUSTOM")
            .or_else(|| interp.global_props.get("RULE_LAUNCH_CUSTOM"))
            .filter(|value| !value.is_empty())
        else {
            return String::new();
        };
        let binary_dir = &interp.makefiles[makefile].binary_dir;
        let output = evaluated
            .outputs
            .iter()
            .map(|output| {
                let path = if evaluated.working_directory.is_empty() {
                    custom::maybe_relative_to(self.ctx, makefile, binary_dir, output)
                } else {
                    output.clone()
                };
                self.shell(&path)
            })
            .collect::<Vec<_>>()
            .join(",");
        // cmRulePlaceholderExpander with Output and Role.
        let mut launcher = launcher
            .replace("<OUTPUT>", &output)
            .replace("<ROLE>", "custom");
        if !launcher.is_empty() {
            launcher.push(' ');
        }
        launcher
    }

    /// `cmLocalNinjaGenerator::BuildCommandLine` (Unix), writing long custom
    /// steps to a script.
    fn build_command_line_step(
        &self,
        lines: &[String],
        custom_step: &str,
        target: Option<&Target>,
    ) -> String {
        if lines.is_empty() {
            return SHELL_NOOP.to_owned();
        }
        if !custom_step.is_empty() {
            let total: usize = lines.iter().map(|line| line.len() + 6).sum();
            if total as i64 > command_line_length_limit() / 2 {
                let mut script = match target {
                    Some(target) => gt::support_directory(self.ctx, target),
                    None => format!("{}/CMakeFiles", self.home_binary),
                };
                let _ = std::fs::create_dir_all(&script);
                script.push('/');
                script.push_str(custom_step);
                script.push_str(".sh");
                let mut content = String::from("set -e\n\n");
                for line in lines {
                    content.push_str(&line.replace("$$", "$"));
                    content.push('\n');
                }
                let _ = std::fs::write(&script, &content);
                let digest = <sha2::Sha256 as sha2::Digest>::digest(content.as_bytes());
                let hash: String = digest.iter().map(|byte| format!("{byte:02x}")).collect();
                return format!(
                    "/bin/sh {} {}",
                    self.shell(&self.ninja_path(&script)),
                    &hash[..16]
                );
            }
        }
        lines.join(" && ")
    }

    /// `cmGlobalNinjaGenerator::CCOutputs::Add`.
    fn add_cc_outputs(&mut self, outputs: &mut CcOutputs, paths: &[String]) {
        for path in paths {
            let out = self.ninja_path(path);
            if !util::is_full_path(&out) {
                outputs.workdir.push(out.clone());
                self.custom_command_outputs
                    .insert(self.ninja_abs_path(path));
            }
            self.custom_command_outputs.insert(out.clone());
            outputs.explicit.push(out);
        }
    }

    /// `cmGlobalNinjaGenerator::WriteCustomCommandBuild`.
    #[allow(clippy::too_many_arguments, reason = "mirrors the C++ signature")]
    fn write_custom_command_build(
        &mut self,
        command: &str,
        description: &str,
        comment: &str,
        depfile: &str,
        job_pool: &str,
        uses_terminal: bool,
        restat: bool,
        outputs: CcOutputs,
        explicit_deps: Vec<String>,
        order_only_deps: Vec<String>,
    ) {
        self.add_custom_command_rule();
        let mut ninja_depfile = String::new();
        let mut depfile_is_output = false;
        if !depfile.is_empty() {
            ninja_depfile = self.ninja_path(depfile);
            depfile_is_output = outputs.explicit.contains(&ninja_depfile);
        }
        let mut build = Build::new("CUSTOM_COMMAND");
        build.comment = comment.to_owned();
        build.outputs = outputs.explicit;
        build.workdir_outs = outputs.workdir;
        build.explicit_deps = explicit_deps;
        build.order_only_deps = order_only_deps;
        build.variables.insert("COMMAND".into(), command.to_owned());
        build
            .variables
            .insert("DESC".into(), encode_literal(description));
        if restat {
            build.variables.insert("restat".into(), "1".into());
        }
        if uses_terminal {
            build.variables.insert("pool".into(), "console".into());
        } else if !job_pool.is_empty() {
            build.variables.insert("pool".into(), job_pool.to_owned());
        }
        if !depfile.is_empty() {
            build.variables.insert("depfile".into(), ninja_depfile);
            if !depfile_is_output {
                build.variables.insert("deps".into(), "gcc".into());
            }
        }
        self.write_build(&build);
    }

    /// `cmGlobalNinjaGenerator::AppendTargetDependsClosure`.
    fn depends_closure(&mut self, target: &Target, omit_self: bool) -> BTreeSet<String> {
        let key = format!("{}:{}", target.makefile, target.name);
        let cached = match self.depends_closures.get(&key) {
            Some(cached) => cached.clone(),
            None => {
                let mut outs = BTreeSet::new();
                for dependency in link::target_dependencies(self.ctx, target, &self.config) {
                    let Some(dep) = self.ctx.target(&dependency) else {
                        continue;
                    };
                    if !is_in_build_system(dep) {
                        continue;
                    }
                    outs.extend(self.depends_closure(dep, false));
                }
                self.depends_closures.insert(key, outs.clone());
                outs
            }
        };
        let mut result = cached;
        if !omit_self {
            let mut own = Vec::new();
            self.append_target_outputs(target, &mut own, DependOn::TargetArtifact);
            result.extend(own);
        }
        result
    }

    /// `cmLocalNinjaGenerator::WriteCustomCommandBuildStatement`.
    fn write_custom_command_build_statement(&mut self, cc: CcRef, targets: &[&'a Target]) {
        if !self.seen_custom_commands.insert(cc) {
            return;
        }
        let command = self.command(cc);
        let makefile = cc.0;
        let evaluated = self.evaluate_command(command);
        if evaluated.outputs.is_empty() && evaluated.byproducts.is_empty() {
            // Generator expressions evaluate to no output for this config.
            return;
        }
        let mut order_only: BTreeSet<String> = BTreeSet::new();
        if !command.depends_explicit_only {
            // The intersection of the target-depends closures of all
            // targets using the command.
            let mut iter = targets.iter();
            if let Some(first) = iter.next() {
                order_only = self.depends_closure(first, true);
            }
            for target in iter {
                let deps = self.depends_closure(target, true);
                order_only.retain(|dep| deps.contains(dep));
            }
        }
        let interp = self.ctx.interp;
        let symbolic = evaluated.outputs.iter().any(|output| {
            crate::commands::target::find_source_index(interp, makefile, output).is_some_and(
                |index| {
                    interp.ext.sources[&makefile][index]
                        .props
                        .get("SYMBOLIC")
                        .is_some_and(util::is_on)
                },
            )
        });
        let mut outputs = CcOutputs::default();
        self.add_cc_outputs(&mut outputs, &evaluated.outputs);
        self.add_cc_outputs(&mut outputs, &evaluated.byproducts);
        let main_output = outputs.explicit[0].clone();
        let ninja_deps: Vec<String> = evaluated
            .depends
            .iter()
            .map(|dep| self.ninja_path(dep))
            .collect();
        let lines = self.custom_command_lines(&evaluated, makefile);
        let order_only: Vec<String> = order_only.into_iter().collect();
        if lines.is_empty() {
            let mut build = Build::new("phony");
            build.comment = format!("Phony custom command for {main_output}");
            build.outputs = outputs.explicit;
            build.workdir_outs = outputs.workdir;
            build.explicit_deps = ninja_deps;
            build.order_only_deps = order_only;
            self.write_build(&build);
            return;
        }
        let digest = <sha2::Sha256 as sha2::Digest>::digest(main_output.as_bytes());
        let hash: String = digest.iter().map(|byte| format!("{byte:02x}")).collect();
        let custom_step = format!("{}-{}", util::filename_name(&main_output), &hash[..7]);
        let mut depfile = evaluated.depfile.clone();
        if !depfile.is_empty() {
            let status = crate::policy::index_of("CMP0116")
                .and_then(|index| command.policies.get(index))
                .unwrap_or(crate::policy::PolicyStatus::Warn);
            match status {
                crate::policy::PolicyStatus::New => {
                    depfile = custom::internal_depfile_name(self.ctx, &evaluated.full_depfile);
                }
                crate::policy::PolicyStatus::Warn => {
                    if interp.makefiles[makefile].binary_dir != interp.home_binary
                        || interp.policy_optional_warning(
                            self.ctx
                                .var(makefile, "CMAKE_POLICY_WARNING_CMP0116")
                                .as_deref(),
                        )
                    {
                        self.ctx.warning(
                            command.backtrace.clone(),
                            crate::policy::policy_warning(
                                crate::policy::index_of("CMP0116").expect("CMP0116"),
                            ),
                        );
                    }
                }
                crate::policy::PolicyStatus::Old => {}
            }
        }
        let description = evaluated.description(self.ctx, makefile, "");
        let command_line = self.build_command_line_step(&lines, &custom_step, None);
        self.write_custom_command_build(
            &command_line,
            &description,
            &format!("Custom command for {main_output}"),
            &depfile,
            &command.job_pool,
            command.uses_terminal,
            !symbolic || !evaluated.byproducts.is_empty(),
            outputs,
            ninja_deps,
            order_only,
        );
    }
}

/// `cmGlobalNinjaGenerator::CCOutputs`.
#[derive(Default)]
struct CcOutputs {
    explicit: Vec<String>,
    workdir: Vec<String>,
}

/// `cmGeneratorTarget::GetTargetVersion` (major, minor).
fn target_version(target: &Target) -> (String, String) {
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

/// `cmNinjaNormalTargetGenerator::GetVisibleTypeName`.
fn visible_type_name(kind: TargetKind) -> &'static str {
    match kind {
        TargetKind::StaticLibrary => "static library",
        TargetKind::SharedLibrary => "shared library",
        TargetKind::ModuleLibrary => "shared module",
        TargetKind::Executable => "executable",
        _ => "",
    }
}

/// `cmLocalGenerator::GetRuleLauncher` (RULE_LAUNCH_* properties).
fn rule_launcher(ctx: &GenCtx, target: &Target, property: &str) -> String {
    let value = target
        .props
        .get(property)
        .map(str::to_owned)
        .or_else(|| {
            ctx.interp.makefiles[target.makefile]
                .props
                .get(property)
                .map(str::to_owned)
        })
        .or_else(|| ctx.interp.global_props.get(property).map(str::to_owned));
    match value {
        // cmGeneratorExpression::Evaluate(*value, lg, config, target).
        Some(value) if !value.is_empty() => {
            format!("{} ", ctx.genex(&value, Some(target), None))
        }
        _ => String::new(),
    }
}
