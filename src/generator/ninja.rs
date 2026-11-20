//! The Ninja generator (port of cmGlobalNinjaGenerator,
//! cmLocalNinjaGenerator, cmNinjaTargetGenerator,
//! cmNinjaNormalTargetGenerator and cmNinjaUtilityTargetGenerator) for the
//! single-configuration "Ninja" generator on Unix.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fmt::Write as _;

use super::ctx::GenCtx;
use super::target::{self as gt, ArtifactType, SourceKind};
use super::{flags, link};
use crate::outconv;
use crate::state::{Target, TargetKind};
use crate::util;

pub const SHELL_NOOP: &str = ":";
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
    out.push_str("# =============================================================================\n");
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
    text.trim_matches(|ch: char| ch == ' ' || ch == '\t' || ch == '\n' || ch == '\r' || ch == '\x0b' || ch == '\x0c')
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
            && (arguments.len() + build_str.len() + assignments.len() + 1000) as i64 > command_line_limit)
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

/// `cmSystemTools::CalculateCommandLineLengthLimit` on POSIX.
pub fn command_line_length_limit() -> i64 {
    // SAFETY: sysconf is always safe to call.
    let arg_max = unsafe { libc::sysconf(libc::_SC_ARG_MAX) } as i64;
    let mut limit = if arg_max > 0 { arg_max } else { 0 };
    // Subtract some for the environment and "safety" margin like CMake:
    // sz = sysconf(_SC_ARG_MAX); sz -= 1000 (environment estimate).
    if limit > 0 {
        let env_size: i64 = std::env::vars_os()
            .map(|(key, value)| (key.len() + value.len() + 2) as i64)
            .sum();
        limit -= env_size;
        limit -= 1000;
    }
    // Clamp as CMake does (128 KiB per argument limit on Linux).
    limit.clamp(0, 131_072)
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
    seen_custom_commands: HashSet<usize>,
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
        let make_program = interp.cache.value("CMAKE_MAKE_PROGRAM").unwrap_or("ninja").to_owned();
        let ninja = outconv::escape_for_shell(&make_program, outconv::NINJA_UNIX | outconv::FLAG_ALLOW_MAKE_VARIABLES);
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
            cmake_cmd: cmake,
            ninja_cmd: ninja,
            home_binary: interp.home_binary.clone(),
            home_source: interp.home_source.clone(),
            config: ctx.config.clone(),
            install_target_enabled: interp.ext.misc.get("install-target-enabled").is_some(),
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
        outconv::escape_for_shell(text, outconv::NINJA_UNIX | outconv::FLAG_ALLOW_MAKE_VARIABLES)
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

    fn target_kind_name(kind: TargetKind) -> &'static str {
        kind.type_name()
    }

    /// `cmGlobalNinjaGenerator::AppendTargetOutputs`.
    pub fn append_target_outputs(&self, target: &Target, out: &mut Vec<String>, depend: DependOn) {
        let config = self.config.clone();
        match target.kind {
            TargetKind::SharedLibrary | TargetKind::StaticLibrary | TargetKind::ModuleLibrary => {
                if depend == DependOn::TargetOrdering {
                    out.push(format!("cmake_object_order_depends_target_{}", target.name));
                    return;
                }
                out.push(self.ninja_path(&gt::full_path(self.ctx, target, &config, ArtifactType::Runtime, false)));
            }
            TargetKind::Executable => {
                if depend == DependOn::TargetOrdering {
                    out.push(format!("cmake_object_order_depends_target_{}", target.name));
                    return;
                }
                out.push(self.ninja_path(&gt::full_path(self.ctx, target, &config, ArtifactType::Runtime, false)));
            }
            TargetKind::ObjectLibrary => {
                if depend == DependOn::TargetOrdering {
                    out.push(format!("cmake_object_order_depends_target_{}", target.name));
                    return;
                }
                let dir = self.target_dir_prefix(target.makefile);
                out.push(format!("{dir}{}", target.name));
            }
            TargetKind::Utility | TargetKind::GlobalTarget => {
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
            if dep.kind == TargetKind::InterfaceLibrary {
                continue;
            }
            self.append_target_outputs(dep, &mut outs, depend);
        }
        outs.sort();
        outs.dedup();
        out.extend(outs);
    }

    pub fn add_target_alias(&mut self, alias: &str, target: &Target) {
        let mut outputs = Vec::new();
        self.append_target_outputs(target, &mut outputs, DependOn::TargetArtifact);
        // Don't write an alias that is the same as the output.
        if outputs.len() == 1 && outputs[0] == alias {
            return;
        }
        if alias == "all" || alias == "clean" || alias == "help" {
            return;
        }
        match self.target_aliases.get(alias) {
            Some(existing) if existing.as_deref() != Some(target.name.as_str()) => {
                self.target_aliases.insert(alias.to_owned(), None);
            }
            Some(_) => {}
            None => {
                self.target_aliases.insert(alias.to_owned(), Some(target.name.clone()));
            }
        }
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
        crate::fsutil::write_if_different(std::path::Path::new(&rules_path), rules_file.as_bytes())?;
        crate::fsutil::write_if_different(std::path::Path::new(&build_path), build_file.as_bytes())?;
        Ok(())
    }
}

impl<'a> NinjaGenerator<'a> {
    fn project_name(&self, makefile: usize) -> String {
        self.ctx
            .interp
            .makefiles[makefile]
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
            let _ = write!(top, "ninja_required_version = {REQUIRED_NINJA_VERSION_CONSOLE_POOL}\n\n");
            write_variable(&mut top, "CONFIGURATION", &self.config, "Set configuration variable for custom commands.", 0);
            write_divider(&mut top);
            top.push_str("# Include auxiliary files.\n\n");
            write_comment(&mut top, "Include rules file.");
            top.push_str("include CMakeFiles/rules.ninja\n\n");
            write_divider(&mut top);
            write_comment(&mut top, "Logical path to working directory; prefix for absolute paths.");
            let _ = writeln!(top, "cmake_ninja_workdir = {}", encode_path(&format!("{}/", self.home_binary)));
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
                        crate::configure::system_error(None, &format!("Invalid pool defined by property 'JOB_POOLS': {pool}"));
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
            .filter(|target| target.makefile == makefile && !target.imported && target.alias_of.is_none())
            .collect();
        for target in targets {
            match target.kind {
                TargetKind::Executable
                | TargetKind::StaticLibrary
                | TargetKind::SharedLibrary
                | TargetKind::ModuleLibrary
                | TargetKind::ObjectLibrary => self.generate_normal_target(target),
                TargetKind::Utility => self.generate_utility_target(target),
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
        let ctest = interp.cache.value("CMAKE_CTEST_COMMAND").unwrap_or("ctest").to_owned();
        let cmake = interp.cmake_command.clone();
        let mut infos: Vec<(String, String, Vec<Vec<String>>, Vec<String>, bool)> = Vec::new();
        if self.ctx.var(0, "CMAKE_TESTING_ENABLED").is_some_and(|value| util::is_on(&value)) {
            let mut line = vec![ctest];
            if let Some(args) = self.ctx.var(0, "CMAKE_CTEST_ARGUMENTS") {
                line.extend(util::expand_list(&args, false));
            }
            line.push("$(ARGS)".to_owned());
            let mut depends = Vec::new();
            if self.ctx.var(0, "CMAKE_SKIP_TEST_ALL_DEPENDENCY").is_some_and(|value| util::is_off(&value)) {
                depends.push("all".to_owned());
            }
            infos.push(("test".into(), "Running tests...".into(), vec![line], depends, true));
        }
        infos.push((
            "edit_cache".into(),
            "No interactive CMake dialog available...".into(),
            vec![vec![cmake.clone(), "-E".into(), "echo".into(), "No interactive CMake dialog available.".into()]],
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
        let skip_install = self.ctx.var(0, "CMAKE_SKIP_INSTALL_RULES").is_some_and(|value| util::is_on(&value));
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
                    components.iter().map(|c| format!("\"{c}\"")).collect::<Vec<_>>().join(" ")
                )
            };
            infos.push(("list_install_components".into(), message, Vec::new(), Vec::new(), false));
            let mut depends = Vec::new();
            if self.ctx.var(0, "CMAKE_SKIP_INSTALL_ALL_DEPENDENCY").is_none_or(|value| util::is_off(&value)) {
                depends.push("all".to_owned());
            }
            let base = vec![cmake.clone(), "-P".to_owned(), "cmake_install.cmake".to_owned()];
            infos.push(("install".into(), "Install the project...".into(), vec![base.clone()], depends.clone(), true));
            let mut local = base.clone();
            local.insert(1, "-DCMAKE_INSTALL_LOCAL_ONLY=1".into());
            let parallel = interp.global_props.get("INSTALL_PARALLEL").is_some_and(util::is_on);
            infos.push((
                "install/local".into(),
                "Installing only the local directory...".into(),
                vec![local],
                depends.clone(),
                !parallel,
            ));
            if self.ctx.var(0, "CMAKE_STRIP").is_some_and(|value| !value.is_empty() && !util::is_notfound(&value)) {
                let mut strip = base;
                strip.insert(1, "-DCMAKE_INSTALL_DO_STRIP=1".into());
                infos.push(("install/strip".into(), "Installing the project stripped...".into(), vec![strip], depends, true));
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
                });
            }
        }
    }

    /// Utility generation for a global target (cmNinjaUtilityTargetGenerator
    /// for GLOBAL_TARGET).
    fn generate_global_target(&mut self, global: &GlobalTarget) {
        let prefix = self.target_dir_prefix(global.makefile);
        let binary_dir = self.ctx.interp.makefiles[global.makefile].binary_dir.clone();
        let output_name = format!("{prefix}{}", global.name);
        let util_name = format!("{prefix}CMakeFiles/{}.util", global.name);
        let mut lines: Vec<String> = Vec::new();
        let real_lines: Vec<&Vec<String>> = global.command_lines.iter().filter(|line| !line.is_empty()).collect();
        if !real_lines.is_empty() {
            lines.push(format!("cd {}", self.shell(&binary_dir)));
            for line in real_lines {
                let mut command = self.shell(&line[0]);
                for arg in &line[1..] {
                    // Make-style variable references like $(ARGS) are dropped
                    // by the Ninja generator.
                    if arg == "$(ARGS)" {
                        continue;
                    }
                    command.push(' ');
                    command.push_str(&outconv::escape_for_shell(arg, outconv::NINJA_UNIX));
                }
                lines.push(command);
            }
        }
        let mut depends: Vec<String> = global
            .depends
            .iter()
            .map(|dep| if dep == "all" { "all".to_owned() } else { dep.clone() })
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
        build.variables.insert("DESC".into(), encode_literal(&global.message));
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
        let aliases: Vec<(String, Option<String>)> =
            self.target_aliases.iter().map(|(a, t)| (a.clone(), t.clone())).collect();
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
    fn write_folder_targets(&mut self) {
        let interp = self.ctx.interp;
        let mut out = String::new();
        write_divider(&mut out);
        out.push_str("# Folder targets.\n\n");
        // For each directory, its "all" depends on its own targets that are
        // not EXCLUDE_FROM_ALL, plus children's "all" unless excluded.
        let count = interp.makefiles.len();
        let mut folder_outputs: Vec<Vec<String>> = vec![Vec::new(); count];
        for index in 0..count {
            for target in self.ctx.targets_in_order() {
                if target.makefile != index || target.imported || target.alias_of.is_some() {
                    continue;
                }
                if matches!(target.kind, TargetKind::InterfaceLibrary | TargetKind::GlobalTarget) {
                    continue;
                }
                if is_excluded_from_all(target) {
                    continue;
                }
                let mut outputs = Vec::new();
                self.append_target_outputs(target, &mut outputs, DependOn::TargetArtifact);
                folder_outputs[index].extend(outputs);
            }
        }
        // Write in directory order (DFS like the C++ which iterates
        // LocalGenerators and adds children's all).
        for index in 0..count {
            let makefile = &interp.makefiles[index];
            let mut build = Build::new("phony");
            build.comment = format!("Folder: {}", makefile.binary_dir);
            let prefix = self.target_dir_prefix(index);
            build.outputs.push(format!("{prefix}all"));
            build.explicit_deps.extend(folder_outputs[index].iter().cloned());
            for &child in &makefile.children {
                if interp.makefiles[child].exclude_from_all && !makefile.exclude_from_all {
                    continue;
                }
                let child_prefix = self.target_dir_prefix(child);
                build.explicit_deps.push(format!("{child_prefix}all"));
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
        self.build.push_str(&out);
    }

    fn write_rebuild_manifest(&mut self, out: &mut String) {
        if self.ctx.var(0, "CMAKE_SUPPRESS_REGENERATION").is_some_and(|value| util::is_on(&value)) {
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
        for extra in self.ctx.interp.ext.misc.get("cmake-depend-files").map(|v| util::expand_list(v, false)).unwrap_or_default() {
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
        let mut outputs = vec!["build.ninja".to_owned()];
        for makefile in &self.ctx.interp.makefiles {
            outputs.push(format!("{}/cmake_install.cmake", makefile.binary_dir));
            if self.ctx.interp.ext.misc.contains_key("testing-enabled") || true {
                outputs.push(format!("{}/CTestTestfile.cmake", makefile.binary_dir));
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

/// `cmGeneratorTarget::GetPropertyAsBool("EXCLUDE_FROM_ALL")`.
pub fn is_excluded_from_all(target: &Target) -> bool {
    target.props.get("EXCLUDE_FROM_ALL").is_some_and(util::is_on)
}

impl<'a> NinjaGenerator<'a> {
    fn object_dir(&self, target: &Target) -> String {
        gt::object_directory(self.ctx, target, &self.config).trim_end_matches('/').to_owned()
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
        let depfile_format = self.ctx.var_safe(makefile, &format!("CMAKE_{language}_DEPFILE_FORMAT"));
        if depfile_format == "msvc" {
            rule.deptype = "msvc".into();
        } else {
            rule.deptype = "gcc".into();
            rule.depfile = "$DEP_FILE".into();
        }
        let depfile_flags = self.ctx.var_safe(makefile, &format!("CMAKE_DEPFILE_FLAGS_{language}"));
        if !depfile_flags.is_empty() {
            let expanded = flags::expand_rule(
                self.ctx,
                target,
                &depfile_flags,
                &[("DEP_FILE", rule.depfile.clone()), ("DEP_TARGET", "$out".into())],
            );
            flags_text.push(' ');
            flags_text.push_str(&expanded);
        }
        let compile = self.ctx.var_safe(makefile, &format!("CMAKE_{language}_COMPILE_OBJECT"));
        let mut commands = util::expand_list(&compile, false);
        if let Some(first) = commands.first_mut() {
            first.insert_str(0, "${LAUNCHER}${CODE_CHECK}");
        }
        let extra = self.ctx.var_safe(makefile, &format!("CMAKE_{language}_DEPENDS_EXTRA_COMMANDS"));
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
            .map(|command| flags::expand_rule(self.ctx, target, &format!("{launcher}{command}"), &vars))
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
                &format!("CMake can not determine linker language for target: {}", target.name),
            );
            return;
        }
        let sources = gt::sources(self.ctx, target, &config);
        // WriteLanguagesRules: compile rules for every object language.
        let mut languages: Vec<String> = Vec::new();
        for source in &sources {
            if source.kind == SourceKind::ObjectSource && !languages.contains(&source.language) {
                languages.push(source.language.clone());
            }
        }
        languages.sort();
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
        let object_dir = self.object_dir(target);
        let _ = std::fs::create_dir_all(&object_dir);
        let order_target = format!("cmake_object_order_depends_target_{}", target.name);
        {
            let mut build = Build::new("phony");
            build.comment = format!("Order-only phony target for {}", target.name);
            build.outputs.push(order_target.clone());
            let mut order = Vec::new();
            self.append_target_depends(target, &mut order, DependOn::TargetOrdering);
            for source in &sources {
                if source.kind == SourceKind::CustomCommandOutput || (source.generated && source.kind != SourceKind::ObjectSource) {
                    order.push(self.ninja_path(&source.path));
                }
            }
            order.sort();
            order.dedup();
            if order.is_empty() {
                order.push(".".into());
            }
            build.order_only_deps = order;
            write_build(&mut out, &build, 0);
        }
        let mut objects: Vec<String> = Vec::new();
        for source in &sources {
            match source.kind {
                SourceKind::ObjectSource => {}
                SourceKind::ExternalObject => {
                    objects.push(self.ninja_path(&source.path));
                    continue;
                }
                _ => continue,
            }
            let object = self.ninja_path(&format!("{object_dir}/{}", source.object_name));
            let object_file_dir = util::filename_path(&object);
            let vars = flags::compile_vars(self.ctx, target, &config, source);
            let mut build = Build::new(&self.compiler_rule_name(&source.language, target));
            build.outputs.push(object.clone());
            build.explicit_deps.push(self.ninja_path(&source.path));
            build.order_only_deps.push(order_target.clone());
            build.variables.insert("FLAGS".into(), vars.flags);
            build.variables.insert("DEFINES".into(), vars.defines);
            build.variables.insert("INCLUDES".into(), vars.includes);
            build.variables.insert("CONFIG".into(), config.clone());
            let depfile_format = self.ctx.var_safe(target.makefile, &format!("CMAKE_{}_DEPFILE_FORMAT", source.language));
            if depfile_format != "msvc" {
                build.variables.insert("DEP_FILE".into(), format!("{object}.d"));
            }
            build.variables.insert(
                "LAUNCHER".into(),
                flags::compiler_launcher(self.ctx, target, &source.language),
            );
            let object_dir_rel = self.ninja_path(&object_dir);
            build.variables.insert("OBJECT_DIR".into(), object_dir_rel);
            build.variables.insert("OBJECT_FILE_DIR".into(), object_file_dir);
            self.set_pdb_variables(target, &mut build.variables, true);
            write_build(&mut out, &build, 0);
            objects.push(object);
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

    fn set_pdb_variables(&self, target: &Target, vars: &mut BTreeMap<String, String>, compile: bool) {
        // MSVC-style PDB variables are written for every target type that
        // has a PDB name (cmNinjaTargetGenerator::SetMsvcTargetPdbVariable
        // writes them for non-MSVC too when the rule has <TARGET_PDB>).
        let config = self.config.clone();
        let names = gt::names(self.ctx, target, &config);
        if names.pdb.is_empty() {
            return;
        }
        let object_dir = self.ninja_path(&self.object_dir(target));
        let compile_pdb = if target.kind == TargetKind::StaticLibrary {
            format!("{object_dir}/{}.pdb", names.base.trim_start_matches("lib"))
        } else {
            format!("{object_dir}/")
        };
        let _ = compile;
        let dir = gt::directory(self.ctx, target, &config, ArtifactType::Runtime);
        let pdb = self.ninja_path(&format!("{dir}/{}", names.pdb));
        vars.insert("TARGET_PDB".into(), pdb);
        vars.insert("TARGET_COMPILE_PDB".into(), compile_pdb);
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
                rule.deptype = self.ctx.var_safe(makefile, &format!("CMAKE_{language}_LINKER_DEPFILE_FORMAT"));
                rule.depfile = "$DEP_FILE".into();
            }
            let commands: Vec<String> = if target.kind == TargetKind::StaticLibrary {
                // cmNinjaNormalTargetGenerator::ComputeLinkCmd for static libs:
                // remove the old archive first, then create/append/finish.
                let mut list = vec![format!("{} -E rm -f <TARGET>", self.cmake_cmd)];
                let create = self.ctx.var_safe(makefile, &format!("CMAKE_{language}_ARCHIVE_CREATE"));
                list.extend(util::expand_list(&create, false));
                let finish = self.ctx.var_safe(makefile, &format!("CMAKE_{language}_ARCHIVE_FINISH"));
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
                vars.push(("LANGUAGE_COMPILE_FLAGS", "$LANGUAGE_COMPILE_FLAGS $ARCH_FLAGS".into()));
            }
            let mut expanded: Vec<String> = commands
                .iter()
                .map(|command| flags::expand_rule(self.ctx, target, &format!("{launcher}{command}"), &vars))
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
            let (rule_name, command, description, comment) = if target.kind == TargetKind::Executable {
                (
                    "CMAKE_SYMLINK_EXECUTABLE",
                    format!("{} -E cmake_symlink_executable $in $out && $POST_BUILD", self.cmake_cmd),
                    "Creating executable symlink $out",
                    "Rule for creating executable symlink.",
                )
            } else {
                (
                    "CMAKE_SYMLINK_LIBRARY",
                    format!("{} -E cmake_symlink_library $in $SONAME $out && $POST_BUILD", self.cmake_cmd),
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
        let object_dir = self.ninja_path(&self.object_dir(target));
        if target.kind != TargetKind::StaticLibrary {
            let line = link::link_line(self.ctx, target, &config, language);
            build.variables.insert("LINK_LIBRARIES".into(), line.libraries);
            build.variables.insert("LINK_PATH".into(), line.link_path);
            build.implicit_deps = line.depends.iter().map(|dep| self.ninja_path(dep)).collect();
        }
        if target.kind == TargetKind::Executable {
            build.variables.insert("FLAGS".into(), link_vars.flags);
        } else {
            build.variables.insert("LANGUAGE_COMPILE_FLAGS".into(), link_vars.language_compile_flags);
            build.variables.insert("ARCH_FLAGS".into(), link_vars.arch_flags);
        }
        // cmNinjaTargetGenerator::AddDepfileBinding.
        if let Some(depfile) = &link_depfile {
            let depfile_shell = self.shell(depfile);
            if depfile_shell != *depfile {
                build.variables.insert("depfile".into(), depfile.clone());
            }
            build.variables.insert("DEP_FILE".into(), depfile_shell);
        }
        build.variables.insert("LINK_FLAGS".into(), link_vars.link_flags);
        build.variables.insert("CONFIG".into(), config.clone());
        build.variables.insert("OBJECT_DIR".into(), object_dir.clone());
        build.variables.insert("TARGET_FILE".into(), target_real.clone());
        build.variables.insert("PRE_LINK".into(), self.build_command_line(&[]));
        build.variables.insert("POST_BUILD".into(), self.build_command_line(&[]));
        if matches!(target.kind, TargetKind::SharedLibrary | TargetKind::ModuleLibrary)
            && gt::has_soname(self.ctx, target, &config)
        {
            build
                .variables
                .insert("SONAME_FLAG".into(), self.ctx.var_safe(makefile, &format!("CMAKE_SHARED_LIBRARY_SONAME_{language}_FLAG")));
            build.variables.insert("SONAME".into(), names.shared_object.clone());
        }
        if matches!(target.kind, TargetKind::SharedLibrary | TargetKind::ModuleLibrary | TargetKind::Executable) {
            let implib_dir = gt::directory(self.ctx, target, &config, ArtifactType::Import);
            build.variables.insert(
                "TARGET_IMPLIB".into(),
                format!("{}/{}", implib_dir, names.import_library),
            );
        }
        self.set_pdb_variables(target, &mut build.variables, false);
        // Order-only deps: other targets' outputs.
        let mut order = Vec::new();
        self.append_target_depends(target, &mut order, DependOn::TargetArtifact);
        build.order_only_deps = order;
        let limit = if use_rsp { -1 } else { command_line_length_limit() };
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
                if !names.shared_object.is_empty() && names.shared_object != names.real {
                    symlink.outputs.push(so.clone());
                }
                if target_output != so {
                    symlink.outputs.push(target_output.clone());
                }
                symlink.variables.insert("SONAME".into(), so);
            }
            symlink.explicit_deps.push(target_real.clone());
            symlink.variables.insert("POST_BUILD".into(), ":".into());
            write_build(&mut out, &symlink, 0);
        }
        self.build.push_str(&out);
        self.add_target_alias(&target.name.clone(), target);
    }

    /// `cmNinjaUtilityTargetGenerator::Generate` for add_custom_target.
    fn generate_utility_target(&mut self, target: &'a Target) {
        // INTEGRATION: utility command lines come from the target model's
        // custom command representation (pending merge).
        let prefix = self.target_dir_prefix(target.makefile);
        let mut build = Build::new("phony");
        build.comment = format!("Utility command for {}", target.name);
        build.outputs.push(format!("{prefix}{}", target.name));
        let mut deps = Vec::new();
        self.append_target_depends(target, &mut deps, DependOn::TargetArtifact);
        build.explicit_deps = deps;
        self.write_build(&build);
        self.add_target_alias(&target.name.clone(), target);
    }

    fn write_custom_command_build_statements(&mut self, makefile: usize) {
        // INTEGRATION: custom commands are recorded by the target model.
        let _ = makefile;
        let _ = &self.seen_custom_commands;
    }
}

/// `cmGeneratorTarget::GetTargetVersion` (major, minor).
fn target_version(target: &Target) -> (String, String) {
    let version = target.props.get("VERSION").unwrap_or("");
    let mut parts = version.split('.');
    let major = parts.next().and_then(|part| part.parse::<i64>().ok()).unwrap_or(0);
    let minor = parts.next().and_then(|part| part.parse::<i64>().ok()).unwrap_or(0);
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
        .or_else(|| ctx.interp.makefiles[target.makefile].props.get(property).map(str::to_owned))
        .or_else(|| ctx.interp.global_props.get(property).map(str::to_owned));
    match value {
        Some(value) if !value.is_empty() => format!("{value} "),
        _ => String::new(),
    }
}


