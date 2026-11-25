//! The Ninja generator (port of cmGlobalNinjaGenerator,
//! cmLocalNinjaGenerator, cmNinjaTargetGenerator,
//! cmNinjaNormalTargetGenerator and cmNinjaUtilityTargetGenerator) for the
//! single-configuration "Ninja" generator on Unix.

mod fortran;
pub(crate) mod modules;

use crate::messages::MessageType;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fmt::Write as _;
use std::rc::Rc;

use super::ctx::GenCtx;
use super::module_support::{self, CxxSupportLevel, ModuleSupport};
use super::osx_bundle::{self, SkipParts, SourceFileType};
use super::target::{self as gt, ArtifactType, SourceKind};
use super::{custom, flags, install_name, link};
use crate::outconv;
use crate::state::{CustomCommand, CustomCommandKind, Target, TargetKind};
use crate::util;

pub const SHELL_NOOP: &str = ":";

/// cmGlobalCommonGenerator::RemoveUnknownClangTidyExportFixesFiles.
fn remove_unknown_tidy_fixes(dir: &std::path::Path, known: &BTreeSet<String>) {
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

/// A custom command attached to source `index` of directory `dir`.
pub type CcRef = (usize, usize);
const REQUIRED_NINJA_VERSION_CONSOLE_POOL: &str = "1.5";
const REQUIRED_NINJA_VERSION_MANIFEST_RESTAT: &str = "1.8";

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
    let encoded = text.replace('$', "$$").replace('\n', "$\n");
    if MULTI_CONFIG.get() {
        // The CMAKE_CFG_INTDIR reference stays a Ninja variable.
        encoded.replace("$${CONFIGURATION}", "${CONFIGURATION}")
    } else {
        encoded
    }
}

thread_local! {
    /// `cmGlobalNinjaGenerator::IsMultiConfig` of the generator in use.
    pub static MULTI_CONFIG: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
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
    /// `GlobalTargetInfo::PerConfig` (`cmTarget::IsPerConfig`).
    pub per_config: bool,
}

/// A `cmGlobalNinjaGenerator::TargetAlias`: the target (by name) and the
/// configuration whose outputs the alias names; `None` when ambiguous.
type AliasEntry = Option<(String, String)>;

/// The ninja file a statement is written to under Ninja Multi-Config
/// (`cmGlobalNinjaMultiGenerator`'s impl/config/default/common streams).
/// For single-config Ninja every stream is `build.ninja`.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Stream {
    /// `CMakeFiles/common.ninja`.
    Common,
    /// `CMakeFiles/impl-<config>.ninja`.
    Impl(String),
    /// `build-<config>.ninja`.
    Config(String),
    /// `build.ninja`.
    Default,
}

/// `cmNinjaTargetGenerator::ScanningFiles`: one entry per scanned object or
/// BMI build, collated into the language's dyndep build statement.  The
/// `.ddi` scanning output is recorded only for the first `fileConfig`;
/// the module-map output is recorded for every `fileConfig`.
#[derive(Clone, Default)]
struct ScanningFiles {
    scanning_output: Option<String>,
    module_map_file: Option<String>,
}

impl ScanningFiles {
    /// `ScanningFiles::IsEmpty`.
    fn is_empty(&self) -> bool {
        self.scanning_output.is_none() && self.module_map_file.is_none()
    }
}

pub struct NinjaGenerator<'a> {
    pub ctx: std::rc::Rc<GenCtx<'a>>,
    pub build: String,
    pub rules: String,
    /// Multi-config file buffers keyed by stream; unused single-config.
    streams: std::collections::BTreeMap<Stream, String>,
    /// The stream `self.out()` currently routes to.
    stream: Stream,
    /// `cmGlobalNinjaGenerator::RuleCmdLength` of every written rule.
    rule_lengths: HashMap<String, i64>,
    pub custom_command_outputs: BTreeSet<String>,
    /// `TargetAliases`: alias -> (target name, config), None when ambiguous.
    target_aliases: BTreeMap<String, AliasEntry>,
    /// `Configs[config].TargetAliases` (multi-config `build-<Config>.ninja`).
    config_target_aliases: BTreeMap<String, BTreeMap<String, AliasEntry>>,
    /// `DefaultTargetAliases` (multi-config `build.ninja`).
    default_target_aliases: BTreeMap<String, AliasEntry>,
    /// `ByproductsForCleanTarget` (multi-config only consumer).
    byproducts_for_clean: Vec<String>,
    /// `Configs[config].ByproductsForCleanTarget`.
    config_byproducts_for_clean: BTreeMap<String, Vec<String>>,
    /// `Configs[config].AdditionalCleanFiles` (ADDITIONAL_CLEAN_FILES).
    additional_clean_files: BTreeMap<String, BTreeSet<String>>,
    global_targets: Vec<GlobalTarget>,
    /// Global dependency postorder used to break static cycles in module lookup.
    module_order: HashMap<String, usize>,
    /// `cmGlobalNinjaGenerator::Configs[config].CustomCommands` (seen).
    seen_custom_commands: HashSet<(CcRef, String)>,
    /// `cmLocalNinjaGenerator::CustomCommands` of the current directory, in
    /// first-use order, with the targets using each (`CustomCommandTargets`).
    local_custom_commands: Vec<(CcRef, Vec<&'a Target>)>,
    /// `TargetDependsClosures` cache.
    depends_closures: HashMap<String, Rc<BTreeSet<Rc<str>>>>,
    /// `CompileCommandsStream` entries (compile_commands.json).
    compile_commands: Vec<String>,
    tidy_fixes_dirs: BTreeSet<String>,
    tidy_fixes_files: BTreeSet<String>,
    /// `cmGlobalNinjaGenerator::AssumedSourceDependencies` (std::map:
    /// sorted by absolute source path).
    assumed_source_dependencies: BTreeMap<String, BTreeSet<String>>,
    /// `cmNinjaTargetGenerator::Configs[config].ScanningInfo` for the
    /// target currently being generated (std::map: sorted by language).
    /// Accumulates across the `fileConfig` loop of one `Generate(config)`.
    scanning_info: BTreeMap<String, Vec<ScanningFiles>>,
    /// `cmNinjaTargetGenerator::MacContentFolders` — per-target set of
    /// bundle content folders used by CreateFramework's symlinks.
    mac_content_folders: BTreeSet<String>,
    /// `cmNinjaTargetGenerator::Configs[config].ExtraFiles`: order-only
    /// files (bundle content copies) for the current target.
    extra_files: Vec<String>,
    pub cmake_cmd: String,
    pub ninja_cmd: String,
    home_binary: String,
    home_source: String,
    config: String,
    /// The `fileConfig` whose impl file the current statements go to
    /// (differs from `config` only for cross-config builds).
    file_config: String,
    install_target_enabled: bool,
    /// `cmGlobalNinjaGenerator::OutputPathPrefix` (with trailing slash).
    output_prefix: String,
}

impl<'a> NinjaGenerator<'a> {
    pub fn new(ctx: &'a GenCtx<'a>) -> NinjaGenerator<'a> {
        Self::new_rc(std::rc::Rc::new(ctx.clone()))
    }

    pub fn new_rc(ctx: std::rc::Rc<GenCtx<'a>>) -> NinjaGenerator<'a> {
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
            ctx: ctx.clone(),
            build: String::new(),
            rules: String::new(),
            streams: std::collections::BTreeMap::new(),
            stream: Stream::Common,
            rule_lengths: HashMap::new(),
            custom_command_outputs: BTreeSet::new(),
            target_aliases: BTreeMap::new(),
            config_target_aliases: BTreeMap::new(),
            default_target_aliases: BTreeMap::new(),
            byproducts_for_clean: Vec::new(),
            config_byproducts_for_clean: BTreeMap::new(),
            additional_clean_files: BTreeMap::new(),
            global_targets: Vec::new(),
            module_order: HashMap::new(),
            seen_custom_commands: HashSet::new(),
            local_custom_commands: Vec::new(),
            depends_closures: HashMap::new(),
            compile_commands: Vec::new(),
            tidy_fixes_dirs: BTreeSet::new(),
            tidy_fixes_files: BTreeSet::new(),
            assumed_source_dependencies: BTreeMap::new(),
            scanning_info: BTreeMap::new(),
            mac_content_folders: BTreeSet::new(),
            extra_files: Vec::new(),
            cmake_cmd: cmake,
            ninja_cmd: ninja,
            home_binary: interp.home_binary.clone(),
            home_source: interp.home_source.clone(),
            config: ctx.config.clone(),
            file_config: ctx.config.clone(),
            install_target_enabled: interp.ext.misc.contains_key("install-target-enabled"),
            output_prefix: {
                // cmGlobalNinjaGenerator::InitOutputPathPrefix
                let mut prefix = ctx
                    .var(0, "CMAKE_NINJA_OUTPUT_PATH_PREFIX")
                    .unwrap_or_default();
                if !prefix.is_empty() && !prefix.ends_with('/') {
                    prefix.push('/');
                }
                prefix
            },
        }
    }

    // ------------------------------------------------------------------
    // Path helpers

    /// `cmGlobalNinjaGenerator::NinjaOutputPath`: prepend
    /// `CMAKE_NINJA_OUTPUT_PATH_PREFIX` to relative output paths.
    pub fn output_path(&self, path: &str) -> String {
        if self.output_prefix.is_empty() || util::is_full_path(path) {
            return path.to_owned();
        }
        format!("{}{}", self.output_prefix, path)
    }

    /// `cmOutputConverter::MaybeRelativeToTopBinDir` +
    /// `cmGlobalNinjaGenerator::ConvertToNinjaPath`.
    pub fn ninja_path(&self, path: &str) -> String {
        let bin = &self.home_binary;
        if path == bin {
            return self.output_path(".");
        }
        if util::is_subdirectory(path, bin) {
            return self.output_path(&path[bin.len() + 1..]);
        }
        self.output_path(path)
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
        if !self.has_rule(&rule.name) {
            self.rule_lengths
                .insert(rule.name.clone(), rule.command.len() as i64);
            write_rule(&mut self.rules, rule);
        }
    }

    pub fn has_rule(&self, name: &str) -> bool {
        self.rule_lengths.contains_key(name)
    }

    /// `cmGlobalNinjaGenerator::GetRuleCmdLength`: 0 for an unwritten rule.
    fn rule_command_length(&self, name: &str) -> i64 {
        self.rule_lengths.get(name).copied().unwrap_or_default()
    }

    /// `cmNinjaTargetGenerator::ForceResponseFile`.
    fn force_response_file(&self, target: &Target) -> bool {
        let name = "CMAKE_NINJA_FORCE_RESPONSE_FILE";
        self.ctx.var(target.makefile, name).is_some() || std::env::var_os(name).is_some()
    }

    /// The flag introducing a compile rule's response file, or empty.
    fn response_file_flag(&self, target: &Target, language: &str) -> String {
        if language == "RC" || !self.force_response_file(target) {
            return String::new();
        }
        let flag = self.ctx.var_safe(
            target.makefile,
            &format!("CMAKE_{language}_RESPONSE_FILE_FLAG"),
        );
        if flag.is_empty() && language != "CUDA" {
            "@".to_owned()
        } else {
            flag
        }
    }

    /// `WriteBuild`'s command line limit for an object's compilation.
    fn compile_command_limit(&self, target: &Target, language: &str) -> i64 {
        let supports_response = language != "RC"
            && (language != "CUDA"
                || self
                    .ctx
                    .var(target.makefile, "CMAKE_CUDA_RESPONSE_FILE_FLAG")
                    .is_some());
        if supports_response && self.force_response_file(target) {
            -1
        } else {
            0
        }
    }

    /// The buffer `self.stream` routes to (`build.ninja` when not
    /// multi-config).
    fn out(&mut self) -> &mut String {
        let stream = self.stream.clone();
        self.out_at(&stream)
    }

    /// The buffer `stream` routes to (`build.ninja` when not multi-config).
    fn out_at(&mut self, stream: &Stream) -> &mut String {
        if !self.ctx.multi_config {
            return &mut self.build;
        }
        self.streams.entry(stream.clone()).or_default()
    }

    fn write_build(&mut self, build: &Build) {
        write_build(self.out(), build, 0);
    }

    fn write_build_to(&mut self, stream: &Stream, build: &Build) {
        write_build(self.out_at(stream), build, 0);
    }

    /// `cmNinjaTargetGenerator::AddDepfileBinding`: `DEP_FILE` holds the
    /// shell-escaped path for the command line while `depfile` holds the
    /// raw path for ninja's `depfile` attribute when they differ.
    fn add_depfile_binding(&self, vars: &mut BTreeMap<String, String>, depfile: String) {
        let depfile_shell = self.shell(&depfile);
        if depfile_shell != depfile {
            vars.insert("depfile".into(), depfile);
        }
        vars.insert("DEP_FILE".into(), depfile_shell);
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

    /// `cmGlobalNinjaGenerator::AppendTargetOutputs` for the current
    /// configuration.
    pub fn append_target_outputs(&self, target: &Target, out: &mut Vec<String>, depend: DependOn) {
        let config = self.config.clone();
        self.append_target_outputs_cfg(target, out, &config, depend);
    }

    /// `cmGlobalNinjaGenerator::AppendTargetOutputs(target, outputs, config,
    /// depends)`.
    pub fn append_target_outputs_cfg(
        &self,
        target: &Target,
        out: &mut Vec<String>,
        config: &str,
        depend: DependOn,
    ) {
        let view;
        let ctx: &GenCtx = if config == self.ctx.config {
            &self.ctx
        } else {
            view = self.ctx.for_config(config);
            &view
        };
        // For frameworks we want the real name, not the symlink name:
        // frameworks always appear versioned, and the build files manage
        // their symbolic links inside the bundle instead of letting
        // cmOSXBundleGenerator's links be build products.
        let realname = target.is_framework_on_apple();
        match target.kind {
            TargetKind::SharedLibrary
            | TargetKind::StaticLibrary
            | TargetKind::ModuleLibrary
            | TargetKind::Executable => {
                if depend == DependOn::TargetOrdering && target.kind != TargetKind::Executable {
                    out.push(ctx.order_depends_name(&target.name, config));
                    return;
                }
                if target.platform.apple && gt::has_import_library(ctx, target, config) {
                    out.push(self.ninja_path(&gt::full_path(
                        ctx,
                        target,
                        config,
                        ArtifactType::Import,
                        realname,
                    )));
                }
                out.push(self.ninja_path(&gt::full_path(
                    ctx,
                    target,
                    config,
                    ArtifactType::Runtime,
                    realname,
                )));
            }
            TargetKind::ObjectLibrary
            | TargetKind::Utility
            | TargetKind::GlobalTarget
            | TargetKind::InterfaceLibrary => {
                if depend == DependOn::TargetOrdering && target.kind == TargetKind::ObjectLibrary {
                    out.push(ctx.order_depends_name(&target.name, config));
                    return;
                }
                let dir = self.target_dir_prefix(target.makefile);
                let output = self.output_path(&format!("{dir}{}", target.name));
                if ctx.is_per_config(target) {
                    out.push(ctx.build_alias(&output, config));
                } else {
                    out.push(output);
                }
            }
            _ => {}
        }
    }

    /// The `<dir>/` prefix used for utility/global targets in subdirectories.
    /// Callers apply `output_path` to the formatted target name, so the
    /// directory part must not carry `CMAKE_NINJA_OUTPUT_PATH_PREFIX`.
    fn target_dir_prefix(&self, makefile: usize) -> String {
        let binary = &self.ctx.interp.makefiles[makefile].binary_dir;
        let bin = &self.home_binary;
        if binary == bin {
            return String::new();
        }
        let relative = if util::is_subdirectory(binary, bin) {
            &binary[bin.len() + 1..]
        } else {
            binary.as_str()
        };
        if relative.is_empty() {
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
        for dependency in link::target_depends(&self.ctx, target).iter() {
            let dep = &self.ctx.interp.ext.targets[&dependency.key];
            // A cross dependency is built in the file config.
            let config = if dependency.cross {
                self.file_config.clone()
            } else {
                self.config.clone()
            };
            self.append_target_outputs_cfg(dep, &mut outs, &config, depend);
        }
        // cmGlobalNinjaGenerator::AppendTargetDepends sorts but keeps
        // duplicates.
        outs.sort();
        out.extend(outs);
    }

    /// `cmGlobalNinjaGenerator::AddTargetAlias(alias, target, config)` for
    /// the current configuration.
    pub fn add_target_alias(&mut self, alias: &str, target: &Target) {
        let config = self.config.clone();
        self.add_target_alias_cfg(alias, target, &config);
    }

    /// `cmGlobalNinjaGenerator::AddTargetAlias`: the target's own outputs
    /// are marked ambiguous so no other target aliases them, then the alias
    /// is inserted if absent (the first target to claim a name keeps it).
    pub fn add_target_alias_cfg(&mut self, alias: &str, target: &Target, config: &str) {
        let output_path = self.output_path(alias);
        let build_alias = self.ctx.build_alias(&output_path, config);
        let mut outputs = Vec::new();
        if config != "all" {
            self.append_target_outputs_cfg(target, &mut outputs, config, DependOn::TargetArtifact);
        }
        let multi = self.ctx.multi_config;
        for output in outputs {
            self.target_aliases.insert(output.clone(), None);
            if multi {
                self.default_target_aliases.insert(output.clone(), None);
                for config2 in self.ctx.configs.clone() {
                    self.config_target_aliases
                        .entry(config2)
                        .or_default()
                        .insert(output.clone(), None);
                }
            }
        }
        let entry = Some((target.name.clone(), config.to_owned()));
        self.target_aliases
            .entry(build_alias)
            .or_insert_with(|| entry.clone());
        if multi {
            self.config_target_aliases
                .entry(config.to_owned())
                .or_default()
                .entry(output_path.clone())
                .or_insert_with(|| entry.clone());
            if self.ctx.default_configs.contains(config) {
                self.default_target_aliases
                    .entry(output_path)
                    .or_insert(entry);
            }
        }
    }

    // ------------------------------------------------------------------
    // Top-level generation

    /// `cmGlobalNinjaGenerator::Generate` (single config).
    /// `cmGlobalGenerator::CheckTargetProperties`: reports variables used
    /// in link libraries or include directories that resolve to
    /// `<var>-NOTFOUND`.
    fn check_target_properties(&mut self) {
        let interp = self.ctx.interp;
        let mut not_found: BTreeMap<String, String> = BTreeMap::new();
        for target in self.ctx.targets_in_order() {
            if target.kind == TargetKind::InterfaceLibrary {
                continue;
            }
            let dir = &interp.makefiles[target.makefile].source_dir;
            let mark = |uses: &mut Vec<(String, String)>, item: &str, usage: &str| {
                if item.len() > 9 && item.ends_with("-NOTFOUND") {
                    let mut var = item[..item.len() - 9].to_owned();
                    if interp
                        .cache
                        .entries
                        .get(&var)
                        .and_then(|entry| entry.properties.get("ADVANCED"))
                        .is_some_and(|value| util::is_on(value))
                    {
                        var.push_str(" (ADVANCED)");
                    }
                    uses.push((var, usage.to_owned()));
                }
            };
            let mut uses: Vec<(String, String)> = Vec::new();
            if let Some(libs) = target.props.get("LINK_LIBRARIES") {
                for lib in util::expand_list(libs, false) {
                    let name = target.name.clone();
                    mark(
                        &mut uses,
                        &lib,
                        &format!("    linked by target \"{name}\" in directory {dir}"),
                    );
                }
            }
            if let Some(directories) = target.props.get("INCLUDE_DIRECTORIES") {
                let stripped = crate::genex::preprocess(
                    directories,
                    crate::genex::PreprocessContext::Strip,
                    "",
                );
                for dir_item in util::expand_list(&stripped, false) {
                    mark(
                        &mut uses,
                        &dir_item,
                        &format!("   used as include directory in directory {dir}"),
                    );
                }
            }
            for (var, usage) in uses {
                not_found
                    .entry(var)
                    .or_default()
                    .push_str(&format!("\n{usage}"));
            }
        }
        if !not_found.is_empty() {
            let mut body = String::new();
            for (var, uses) in &not_found {
                body.push_str(var);
                body.push_str(uses);
                body.push('\n');
            }
            self.ctx.system_error(format!(
                "The following variables are used in this project, but they are set to NOTFOUND.\nPlease set them or make sure they are set and tested correctly in the CMake files:\n{body}"
            ));
        }
    }

    pub fn generate(&mut self) -> std::io::Result<()> {
        let interp = self.ctx.interp;
        self.check_target_properties();
        // Default global targets per directory.
        self.create_global_targets();
        // Local generators (directories) in creation order.
        for index in 0..interp.makefiles.len() {
            self.generate_local(index);
        }
        self.write_assumed_source_dependencies();
        self.stream = Stream::Common;
        self.write_target_aliases();
        self.write_folder_targets();
        self.write_builtin_targets();
        // cmGlobalNinjaGenerator::WriteDisclaimer.
        let disclaimer = format!(
            "# CMAKE generated file: DO NOT EDIT!\n# Generated by \"{}\" Generator, CMake Version 4.1\n\n",
            if self.ctx.multi_config {
                "Ninja Multi-Config"
            } else {
                "Ninja"
            }
        );
        let rules_path = format!("{}/CMakeFiles/rules.ninja", self.home_binary);
        let mut rules_file = format!(
            "{disclaimer}# This file contains all the rules used to get the outputs files\n# built from the input files.\n# It is included in the main 'build.ninja'.\n\n"
        );
        rules_file.push_str(&self.rules);
        // OpenBuildFileStreams: (path, content) of every build file.
        let mut build_files: Vec<(String, String)> = Vec::new();
        if self.ctx.multi_config {
            let mut streams = std::mem::take(&mut self.streams);
            let mut take = |stream: Stream| streams.remove(&stream).unwrap_or_default();
            build_files.push((
                format!("{}/CMakeFiles/common.ninja", self.home_binary),
                format!(
                    "{disclaimer}# This file contains build statements common to all configurations.\n\n{}",
                    take(Stream::Common)
                ),
            ));
            let default_config = self.ctx.default_file_config.clone();
            build_files.push((
                format!("{}/build.ninja", self.home_binary),
                format!(
                    "{disclaimer}# Build using rules for '{default_config}'.\n\ninclude {}\n\n{}",
                    self.output_path(&format!("CMakeFiles/impl-{default_config}.ninja")),
                    take(Stream::Default)
                ),
            ));
            for config in self.ctx.configs.clone() {
                build_files.push((
                    format!("{}/CMakeFiles/impl-{config}.ninja", self.home_binary),
                    format!(
                        "{disclaimer}# This file contains build statements specific to the \"{config}\"\n# configuration.\n\n{}",
                        take(Stream::Impl(config.clone()))
                    ),
                ));
                build_files.push((
                    format!("{}/build-{config}.ninja", self.home_binary),
                    format!(
                        "{disclaimer}# This file contains aliases specific to the \"{config}\"\n# configuration.\n\ninclude {}\n\n{}",
                        self.output_path(&format!("CMakeFiles/impl-{config}.ninja")),
                        take(Stream::Config(config.clone()))
                    ),
                ));
            }
        } else {
            build_files.push((
                format!("{}/build.ninja", self.home_binary),
                format!(
                    "{disclaimer}# This file contains all the build statements describing the\n# compilation DAG.\n\n{}",
                    self.build
                ),
            ));
        }
        // CloseCompileCommandsStream.
        if !self.compile_commands.is_empty() {
            let content = format!("[\n{}\n]\n", self.compile_commands.join(",\n"));
            crate::fsutil::write_generated(
                std::path::Path::new(&format!("{}/compile_commands.json", self.home_binary)),
                &content,
            )?;
        }
        // cmGlobalGenerator::WriteSummary: every build-system target's
        // support directory.
        let mut summary = String::new();
        for index in 0..interp.makefiles.len() {
            let (before, after) = self.ctx.targets_around_globals();
            let summarize = |targets: Vec<&Target>, summary: &mut String| {
                for target in targets {
                    if target.makefile == index
                        && is_in_build_system(target)
                        && target.kind != TargetKind::GlobalTarget
                    {
                        let directory = gt::support_directory(&self.ctx, target);
                        self.write_labels(target, &directory)?;
                        summary.push_str(&directory);
                        summary.push('\n');
                    }
                }
                Ok::<(), std::io::Error>(())
            };
            summarize(before, &mut summary)?;
            let binary = &interp.makefiles[index].binary_dir;
            for global in self
                .global_targets
                .iter()
                .filter(|global| global.makefile == index)
            {
                let directory = format!("{binary}/CMakeFiles/{}.dir", global.name);
                self.write_labels_files(&global.name, None, index, Vec::new(), &directory)?;
                summary.push_str(&directory);
                summary.push('\n');
            }
            summarize(after, &mut summary)?;
        }
        crate::fsutil::write_generated(
            std::path::Path::new(&format!(
                "{}/CMakeFiles/TargetDirectories.txt",
                self.home_binary
            )),
            &summary,
        )?;
        // cmSystemTools::GetErrorOccurredFlag: the streams are marked failed,
        // so neither file is replaced. Otherwise both are rewritten
        // (cmGeneratedFileStream without copy-if-different), so build.ninja
        // is always newer than the CMakeCache.txt it depends on.
        if self.ctx.interp.error || self.ctx.has_error() {
            return Ok(());
        }
        crate::fsutil::write_generated(std::path::Path::new(&rules_path), &rules_file)?;
        for (path, content) in &build_files {
            crate::fsutil::write_generated(std::path::Path::new(path), content)?;
        }
        for dir in &self.tidy_fixes_dirs {
            remove_unknown_tidy_fixes(std::path::Path::new(dir), &self.tidy_fixes_files);
        }
        Ok(())
    }

    /// The per-directory copies of the default global targets created by
    /// `create_global_targets`, for the extra generators.
    pub fn global_targets(&self) -> &[GlobalTarget] {
        &self.global_targets
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
        let _ = writeln!(out, "# Configurations: {}", self.ctx.configs.join(", "));
        write_divider(out);
    }

    /// Runs `body` with the generator switched to configuration `config`
    /// (`tg->Generate(config)` under Ninja Multi-Config), writing to that
    /// configuration's impl file.
    fn with_config(&mut self, config: &str, body: impl FnOnce(&mut Self)) {
        let saved_ctx = self.ctx.clone();
        let saved_config = std::mem::replace(&mut self.config, config.to_owned());
        let saved_stream = std::mem::replace(&mut self.stream, Stream::Impl(config.to_owned()));
        let saved_file_config = std::mem::replace(&mut self.file_config, config.to_owned());
        if self.ctx.config != config {
            self.ctx = std::rc::Rc::new(self.ctx.for_config(config));
        }
        body(self);
        self.ctx = saved_ctx;
        self.config = saved_config;
        self.stream = saved_stream;
        self.file_config = saved_file_config;
    }

    /// `cmGlobalNinjaGenerator::EnableCrossConfigBuild`.
    fn cross_enabled(&self) -> bool {
        self.ctx.multi_config && !self.ctx.cross_configs.is_empty()
    }

    /// The file configs whose `GetCrossConfigs(fileConfig)` contain
    /// `config`, in configuration order.
    fn file_configs_for(&self, config: &str) -> Vec<String> {
        if !self.ctx.multi_config {
            return vec![config.to_owned()];
        }
        self.ctx
            .configs
            .iter()
            .filter(|file_config| {
                file_config.as_str() == config || self.ctx.cross_configs.contains(config)
            })
            .cloned()
            .collect()
    }

    /// `cmNinjaTargetGenerator::AdditionalCleanFiles` (with a target) and
    /// `cmLocalNinjaGenerator::AdditionalCleanFiles` (directory property).
    fn add_additional_clean_files(&mut self, makefile: usize, target: Option<&Target>) {
        let value = match target {
            Some(target) => target
                .props
                .get("ADDITIONAL_CLEAN_FILES")
                .map(str::to_owned),
            None => self.ctx.interp.makefiles[makefile]
                .props
                .get("ADDITIONAL_CLEAN_FILES")
                .map(str::to_owned),
        };
        let Some(value) = value else {
            return;
        };
        let evaluated = match target {
            Some(target) => self.ctx.genex(&value, Some(target), None),
            None => self.ctx.genex_full(&value, makefile, None, None, true, &[]),
        };
        let binary_dir = self.ctx.interp.makefiles[makefile].binary_dir.clone();
        let config = self.config.clone();
        for file in util::expand_list(&evaluated, false) {
            // Support relative paths.
            let path = util::collapse_full_path(&file, &binary_dir);
            self.additional_clean_files
                .entry(config.clone())
                .or_default()
                .insert(path);
        }
    }

    /// `cmGlobalNinjaGenerator::IsSingleConfigUtility`.
    fn is_single_config_utility(&self, target: &Target) -> bool {
        target.kind == TargetKind::Utility
            && !self
                .ctx
                .interp
                .ext
                .per_config_utility_targets
                .contains(&target.name)
    }

    /// `cmLocalNinjaGenerator::WriteNinjaRequiredVersion`.
    fn write_ninja_required_version(&self, out: &mut String) {
        let required = if self.supports_manifest_restat()
            && self.ctx.interp.ext.glob_verify.is_some()
            && !self.regeneration_suppressed()
        {
            REQUIRED_NINJA_VERSION_MANIFEST_RESTAT
        } else {
            REQUIRED_NINJA_VERSION_CONSOLE_POOL
        };
        write_comment(out, "Minimal version of Ninja required by this file");
        let _ = write!(out, "ninja_required_version = {required}\n\n");
    }

    /// `cmLocalNinjaGenerator::Generate`.
    fn generate_local(&mut self, makefile: usize) {
        let interp = self.ctx.interp;
        let multi = self.ctx.multi_config;
        let configs = self.ctx.configs.clone();
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
        if multi {
            for config in &configs {
                self.out_at(&Stream::Impl(config.clone())).push_str(&header);
            }
        }
        self.out_at(&Stream::Common).push_str(&header);
        if makefile == 0 {
            // WriteBuildFileTop
            let mut top = String::new();
            self.write_project_header(&mut top, 0);
            if multi {
                self.out_at(&Stream::Common).push_str(&top);
                top.clear();
                for config in &configs {
                    let mut stream = String::new();
                    self.write_project_header(&mut stream, 0);
                    self.write_ninja_required_version(&mut stream);
                    write_variable(
                        &mut stream,
                        "CONFIGURATION",
                        config,
                        "Set configuration variable for custom commands.",
                        0,
                    );
                    // WriteNinjaFilesInclusionConfig
                    write_divider(&mut stream);
                    stream.push_str("# Include auxiliary files.\n\n");
                    write_comment(&mut stream, "Include common file.");
                    let common = self.output_path("CMakeFiles/common.ninja");
                    let _ = writeln!(stream, "include {}", encode_path(&common));
                    stream.push('\n');
                    self.out_at(&Stream::Impl(config.clone())).push_str(&stream);
                }
            } else {
                self.write_ninja_required_version(&mut top);
                write_variable(
                    &mut top,
                    "CONFIGURATION",
                    &self.config,
                    "Set configuration variable for custom commands.",
                    0,
                );
            }
            // WriteNinjaFilesInclusionCommon
            write_divider(&mut top);
            top.push_str("# Include auxiliary files.\n\n");
            write_comment(&mut top, "Include rules file.");
            let rules_include = self.output_path("CMakeFiles/rules.ninja");
            let _ = writeln!(top, "include {}", encode_path(&rules_include));
            top.push('\n');
            // WriteNinjaWorkDir
            write_divider(&mut top);
            write_comment(
                &mut top,
                "Logical path to working directory; prefix for absolute paths.",
            );
            // cmGlobalNinjaGenerator::StripNinjaOutputPathPrefixAsSuffix
            let mut workdir = format!("{}/", self.home_binary);
            if !self.output_prefix.is_empty() && workdir.ends_with(&self.output_prefix) {
                workdir.truncate(workdir.len() - self.output_prefix.len());
            }
            let _ = writeln!(top, "cmake_ninja_workdir = {}", encode_path(&workdir));
            self.out_at(&Stream::Common).push_str(&top);
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
        // Targets of this directory in creation order, with the global
        // targets before those the generator added.
        let (before, after) = self.ctx.targets_around_globals();
        let generate = |this: &mut Self, targets: Vec<&'a Target>| {
            for target in targets {
                if target.makefile != makefile || target.imported || target.alias_of.is_some() {
                    continue;
                }
                if multi {
                    for config in &configs {
                        this.with_config(config, |this| this.generate_target(target));
                    }
                } else {
                    this.generate_target(target);
                }
            }
        };
        generate(self, before);
        let globals: Vec<GlobalTarget> = self
            .global_targets
            .iter()
            .filter(|global| global.makefile == makefile)
            .cloned()
            .collect();
        for global in globals {
            if !multi {
                self.generate_global_target(&global);
                continue;
            }
            if !global.per_config {
                self.with_config("", |this| this.generate_global_target(&global));
                continue;
            }
            let prefix = self.target_dir_prefix(global.makefile);
            let output = self.output_path(&format!("{prefix}{}", global.name));
            for config in &configs {
                self.with_config(config, |this| this.generate_global_target(&global));
                let mut alias = Build::new("phony");
                alias.outputs.push(output.clone());
                alias
                    .explicit_deps
                    .push(self.ctx.build_alias(&output, config));
                self.write_build_to(&Stream::Config(config.clone()), &alias);
            }
            if !self.ctx.default_configs.is_empty() {
                let mut alias = Build::new("phony");
                alias.outputs.push(output.clone());
                for config in self.ctx.default_configs.clone() {
                    alias
                        .explicit_deps
                        .push(self.ctx.build_alias(&output, &config));
                }
                self.write_build_to(&Stream::Default, &alias);
            }
            let mut alias = Build::new("phony");
            alias.outputs.push(self.ctx.build_alias(&output, "all"));
            for config in &configs {
                alias
                    .explicit_deps
                    .push(self.ctx.build_alias(&output, config));
            }
            self.write_build_to(&Stream::Default, &alias);
        }
        generate(self, after);
        let commands = std::mem::take(&mut self.local_custom_commands);
        if multi {
            for config in &configs {
                self.with_config(config, |this| {
                    this.write_custom_command_build_statements(&commands);
                    this.add_additional_clean_files(makefile, None);
                });
            }
        } else {
            self.write_custom_command_build_statements(&commands);
            self.add_additional_clean_files(makefile, None);
        }
    }

    /// `cmNinjaTargetGenerator::New(target)->Generate(config)`.
    fn generate_target(&mut self, target: &'a Target) {
        match target.kind {
            TargetKind::Executable
            | TargetKind::StaticLibrary
            | TargetKind::SharedLibrary
            | TargetKind::ModuleLibrary
            | TargetKind::ObjectLibrary => self.generate_normal_target(target),
            // cmNinjaTargetGenerator::New: INTERFACE libraries in the
            // build system use the utility generator; synthetic C++ module
            // targets use the normal generator (BMI builds + module library
            // phony, no link statement).
            TargetKind::Utility => self.generate_utility_target(target),
            TargetKind::InterfaceLibrary if target.synthetic && is_in_build_system(target) => {
                self.generate_synthetic_target(target)
            }
            TargetKind::InterfaceLibrary if is_in_build_system(target) => {
                self.generate_utility_target(target)
            }
            _ => {}
        }
    }

    /// `cmGlobalNinjaGenerator::WriteAssumedSourceDependencies`: for
    /// GENERATED sources with no known producing rule, write an empty
    /// custom-command edge resting on the consuming targets' order-only
    /// dependencies.
    fn write_assumed_source_dependencies(&mut self) {
        let assumed = std::mem::take(&mut self.assumed_source_dependencies);
        for (source, deps) in assumed {
            let mut outputs = CcOutputs::default();
            outputs.explicit.push(source);
            self.write_custom_command_build(
                "",
                "",
                "Assume dependencies for generated source file.",
                "",
                "",
                false,
                true,
                "",
                outputs,
                Vec::new(),
                deps.into_iter().collect(),
            );
        }
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
        // GetCMakeCFGIntDir when it is not "." (multi-config only).
        let cfg_int_dir = self.ctx.multi_config.then_some("${CONFIGURATION}");
        let root_binary = interp.makefiles[0].binary_dir.clone();
        // CheckReservedTargetName: stops at the first reserved name in use.
        let ctx = &self.ctx;
        let reserved_free = |names: &[&str], reason: &str| {
            for name in names {
                if let Some(key) = crate::state::find_global_target_key(interp, name) {
                    ctx.error(
                        interp.ext.targets[&key].backtrace.clone(),
                        format!("The target name \"{name}\" is reserved {reason}."),
                    );
                    return false;
                }
                // Upstream records an `cmTarget` for each ALIAS, so
                // `FindTarget` also sees alias names.
                if let Some((_, _, backtrace)) = interp
                    .ext
                    .alias_backtraces
                    .iter()
                    .find(|(_, alias, _)| alias == name)
                {
                    ctx.error(
                        backtrace.clone(),
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
                depends.push(self.output_path("all"));
            }
            let mut line = vec![cpack.clone()];
            if let Some(cfg_int_dir) = cfg_int_dir {
                line.push("-C".into());
                line.push(cfg_int_dir.into());
            }
            line.push("--config".into());
            line.push("./CPackConfig.cmake".into());
            infos.push((
                "package".into(),
                "Run CPack packaging tool...".into(),
                vec![line],
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
            && reserved_free(&["test", "RUN_TESTS"], "when CTest testing is enabled")
        {
            let mut line = vec![ctest];
            if let Some(args) = self.ctx.var(0, "CMAKE_CTEST_ARGUMENTS") {
                line.extend(util::expand_list(&args, false));
            }
            match cfg_int_dir {
                Some(cfg_int_dir) => {
                    line.push("-C".to_owned());
                    line.push(cfg_int_dir.to_owned());
                }
                None => line.push("$(ARGS)".to_owned()),
            }
            let mut depends = Vec::new();
            if self
                .ctx
                .var(0, "CMAKE_SKIP_TEST_ALL_DEPENDENCY")
                .is_some_and(|value| util::is_off(&value))
            {
                depends.push(self.output_path("all"));
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
        if self.install_target_enabled && !skip_install && cfg_int_dir.is_none() {
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
        }
        if self.install_target_enabled && !skip_install {
            let mut depends = Vec::new();
            if self
                .ctx
                .var(0, "CMAKE_SKIP_INSTALL_ALL_DEPENDENCY")
                .is_none_or(|value| util::is_off(&value))
            {
                depends.push(self.output_path("all"));
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
            let mut base = vec![install_cmake];
            if let Some(cfg_int_dir) = cfg_int_dir {
                base.push(format!("-DBUILD_TYPE={cfg_int_dir}"));
            }
            base.push("-P".to_owned());
            base.push("cmake_install.cmake".to_owned());
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
                    // AddGlobalTarget_EditCache/RebuildCache: PerConfig::No.
                    per_config: name != "edit_cache" && name != "rebuild_cache",
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
        // WriteUtilBuildStatements(config, config): per-config global
        // targets live under CMakeFiles/<Config>/ with `name:Config` aliases.
        let config = self.config.clone();
        let config_dir = if global.per_config {
            self.ctx.config_directory(&config)
        } else {
            String::new()
        };
        let output_config = if global.per_config {
            config.clone()
        } else {
            String::new()
        };
        let output_name = self.ctx.build_alias(
            &self.output_path(&format!("{prefix}{}", global.name)),
            &output_config,
        );
        let util_name = self.output_path(&format!(
            "{prefix}CMakeFiles{config_dir}/{}.util",
            global.name
        ));
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
                let argv0 = match gt::target_in_directory(&self.ctx, global.makefile, &line[0])
                    .filter(|target| target.kind == TargetKind::Executable)
                {
                    Some(target) => gt::full_path(
                        &self.ctx,
                        target,
                        &self.config,
                        ArtifactType::Runtime,
                        false,
                    ),
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
            .map(|dep| {
                self.ctx
                    .build_alias(&self.ninja_path(&format!("{directory}/{dep}")), &config)
            })
            .collect();
        depends.sort();
        if lines.is_empty() {
            let mut build = Build::new("phony");
            build.comment = format!("Utility command for {}", global.name);
            build.outputs.push(output_name.clone());
            build.explicit_deps = depends;
            self.write_build_to(&Stream::Common, &build);
            return;
        }
        let mut command = self.build_command_line(&lines);
        if self.ctx.multi_config {
            // cmGlobalNinjaMultiGenerator::ExpandCFGIntDir.
            command = command.replace("${CONFIGURATION}", &config);
        }
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
        // WriteCustomCommandBuild with an empty ccConfig: the common file.
        self.write_build_to(&Stream::Common, &build);
        let mut alias = Build::new("phony");
        alias.outputs.push(output_name);
        alias.explicit_deps.push(util_name);
        self.write_build_to(&Stream::Common, &alias);
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

    /// `cmGlobalNinjaGenerator::WriteTargetAliases`.
    fn write_target_aliases(&mut self) {
        let mut out = String::new();
        write_divider(&mut out);
        out.push_str("# Target aliases.\n\n");
        self.out_at(&Stream::Common).push_str(&out);
        let multi = self.ctx.multi_config;
        let aliases: Vec<(String, AliasEntry)> = self
            .target_aliases
            .iter()
            .map(|(a, t)| (a.clone(), t.clone()))
            .collect();
        for (alias, entry) in aliases {
            let Some((target, config)) = entry else {
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
            if config == "all" {
                for cross in self.ctx.cross_configs.clone() {
                    self.append_target_outputs_cfg(
                        target,
                        &mut build.explicit_deps,
                        &cross,
                        DependOn::TargetArtifact,
                    );
                }
            } else {
                self.append_target_outputs_cfg(
                    target,
                    &mut build.explicit_deps,
                    &config,
                    DependOn::TargetArtifact,
                );
            }
            let stream = if !multi
                || (self.cross_enabled()
                    && (config == "all" || self.ctx.cross_configs.contains(&config)))
            {
                Stream::Common
            } else {
                Stream::Impl(config)
            };
            self.write_build_to(&stream, &build);
        }
        if !multi {
            return;
        }
        for config in self.ctx.configs.clone() {
            let aliases: Vec<(String, AliasEntry)> = self
                .config_target_aliases
                .get(&config)
                .map(|map| map.iter().map(|(a, t)| (a.clone(), t.clone())).collect())
                .unwrap_or_default();
            for (alias, entry) in aliases {
                let Some((target, _)) = entry else {
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
                self.append_target_outputs_cfg(
                    target,
                    &mut build.explicit_deps,
                    &config,
                    DependOn::TargetArtifact,
                );
                self.write_build_to(&Stream::Config(config.clone()), &build);
            }
        }
        if !self.ctx.default_configs.is_empty() {
            let aliases: Vec<(String, AliasEntry)> = self
                .default_target_aliases
                .iter()
                .map(|(a, t)| (a.clone(), t.clone()))
                .collect();
            for (alias, entry) in aliases {
                let Some((target, _)) = entry else {
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
                for config in self.ctx.default_configs.clone() {
                    self.append_target_outputs_cfg(
                        target,
                        &mut build.explicit_deps,
                        &config,
                        DependOn::TargetArtifact,
                    );
                }
                self.write_build_to(&Stream::Default, &build);
            }
        }
    }

    /// `cmGlobalNinjaGenerator::WriteFolderTargets`: per-directory `all`
    /// phony targets.
    /// `cmGlobalNinjaGenerator::WriteFolderTargets` with
    /// `cmGlobalCommonGenerator::ComputeDirectoryTargets` (directories in
    /// sorted binary-path order, as std::map iterates).
    fn write_folder_targets(&mut self) {
        let interp = self.ctx.interp;
        let multi = self.ctx.multi_config;
        let configs = self.ctx.configs.clone();
        let mut out = String::new();
        write_divider(&mut out);
        out.push_str("# Folder targets.\n\n");
        self.out_at(&Stream::Common).push_str(&out);
        let count = interp.makefiles.len();
        // cmGlobalCommonGenerator::ComputeDirectoryTargets: per directory,
        // its targets with the configurations excluding each from `all`.
        let mut dir_targets: Vec<Vec<(&Target, Vec<String>)>> = vec![Vec::new(); count];
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
                let mut excluded: Vec<String> = Vec::new();
                if let Some(value) = target.props.get("EXCLUDE_FROM_ALL") {
                    for config in &configs {
                        let value = if *config == self.ctx.config {
                            self.ctx.genex(value, Some(target), None)
                        } else {
                            self.ctx.for_config(config).genex(value, Some(target), None)
                        };
                        if util::is_on(&value) {
                            excluded.push(config.clone());
                        }
                    }
                    if excluded.is_empty() {
                        // Explicitly un-excluded: every enclosing
                        // directory's rule depends on it too.
                        let mut parent = interp.makefiles[index].parent;
                        while let Some(dir) = parent {
                            dir_targets[dir].push((target, Vec::new()));
                            parent = interp.makefiles[dir].parent;
                        }
                    }
                }
                dir_targets[index].push((target, excluded));
            }
        }
        // cmGlobalCommonGenerator::IsExcludedFromAllInConfig.
        let excluded_in = |excluded: &Vec<String>, config: &String| {
            if multi {
                excluded.contains(config)
            } else {
                !excluded.is_empty()
            }
        };
        let mut order: Vec<usize> = (0..count).collect();
        order.sort_by(|a, b| {
            interp.makefiles[*a]
                .binary_dir
                .cmp(&interp.makefiles[*b].binary_dir)
        });
        let included_children = |index: usize| -> Vec<usize> {
            interp.makefiles[index]
                .children
                .iter()
                .copied()
                .filter(|&child| {
                    !interp.makefiles[child]
                        .props
                        .get("EXCLUDE_FROM_ALL")
                        .is_some_and(util::is_on)
                })
                .collect()
        };
        // Codegen targets (CMP0171), then `all`.
        let codegen = interp.ext.misc.contains_key("allow-global-target-codegen");
        for kind in ["codegen", "all"] {
            if kind == "codegen" && !codegen {
                continue;
            }
            for &index in &order {
                let makefile = &interp.makefiles[index];
                let mut divider = String::new();
                write_divider(&mut divider);
                self.out_at(&Stream::Common).push_str(&divider);
                let dir_target = self.ninja_path(&format!("{}/{kind}", makefile.binary_dir));
                let comment = format!("Folder: {}", makefile.binary_dir);
                for config in &configs {
                    let mut build = Build::new("phony");
                    build.comment = comment.clone();
                    build
                        .outputs
                        .push(self.ctx.build_alias(&dir_target, config));
                    let view = self.ctx.for_config(config);
                    for (target, excluded) in &dir_targets[index] {
                        if excluded_in(excluded, config) {
                            continue;
                        }
                        if kind == "all" {
                            self.append_target_outputs_cfg(
                                target,
                                &mut build.explicit_deps,
                                config,
                                DependOn::TargetArtifact,
                            );
                            continue;
                        }
                        for source in gt::sources(&view, target, config).iter() {
                            if source.kind != SourceKind::CustomCommandOutput {
                                continue;
                            }
                            let Some(cc) = self.attached_command(target.makefile, &source.path)
                            else {
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
                    for child in included_children(index) {
                        build.explicit_deps.push(self.ctx.build_alias(
                            &self.ninja_path(&format!(
                                "{}/{kind}",
                                interp.makefiles[child].binary_dir
                            )),
                            config,
                        ));
                    }
                    let stream = if !multi
                        || (self.cross_enabled() && self.ctx.cross_configs.contains(config))
                    {
                        Stream::Common
                    } else {
                        Stream::Impl(config.clone())
                    };
                    self.write_build_to(&stream, &build);
                }
                if multi {
                    // Shortcut targets.
                    for config in &configs {
                        let mut build = Build::new("phony");
                        build.comment = comment.clone();
                        build.outputs.push(dir_target.clone());
                        build
                            .explicit_deps
                            .push(self.ctx.build_alias(&dir_target, config));
                        self.write_build_to(&Stream::Config(config.clone()), &build);
                    }
                    if !self.ctx.default_file_config.is_empty() {
                        let mut build = Build::new("phony");
                        build.comment = comment.clone();
                        build.outputs.push(dir_target.clone());
                        for config in &self.ctx.default_configs {
                            build
                                .explicit_deps
                                .push(self.ctx.build_alias(&dir_target, config));
                        }
                        self.write_build_to(&Stream::Default, &build);
                    }
                }
                if self.cross_enabled() {
                    // Target for all configs (`codegen` names its own).
                    let mut build = Build::new("phony");
                    build.comment = comment.clone();
                    build.outputs.push(
                        self.ctx.build_alias(
                            &dir_target,
                            if kind == "all" { "all" } else { "codegen" },
                        ),
                    );
                    for config in &self.ctx.cross_configs {
                        build
                            .explicit_deps
                            .push(self.ctx.build_alias(&dir_target, config));
                    }
                    self.write_build_to(&Stream::Common, &build);
                }
            }
        }
    }

    fn write_builtin_targets(&mut self) {
        let mut out = String::new();
        write_divider(&mut out);
        out.push_str("# Built-in targets\n\n");
        self.write_rebuild_manifest(&mut out);
        self.out_at(&Stream::Common).push_str(&out);
        out.clear();
        self.write_target_clean();
        self.write_target_help(&mut out);
        self.out_at(&Stream::Common).push_str(&out);
        out.clear();
        // cmGlobalNinjaGenerator::WriteTargetDefault: no default statement
        // when CMAKE_NINJA_OUTPUT_PATH_PREFIX is used.
        if self.output_prefix.is_empty() {
            let mut default = String::new();
            write_comment(&mut default, "Make the all target the default.");
            default.push_str("default all\n");
            for config in self.ctx.configs.clone() {
                self.out_at(&Stream::Config(config)).push_str(&default);
            }
            if self.ctx.multi_config && !self.ctx.default_file_config.is_empty() {
                self.out_at(&Stream::Default).push_str(&default);
            }
        }
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
            build.outputs.push(self.output_path("install/parallel"));
            for makefile in &self.ctx.interp.makefiles {
                build
                    .explicit_deps
                    .push(self.ninja_path(&format!("{}/install/local", makefile.binary_dir)));
            }
            write_build(&mut out, &build, 0);
        }
        self.out_at(&Stream::Common).push_str(&out);
    }

    fn regeneration_suppressed(&self) -> bool {
        self.ctx
            .var(0, "CMAKE_SUPPRESS_REGENERATION")
            .is_some_and(|value| util::is_on(&value))
    }

    /// `cmGlobalNinjaGenerator::SupportsManifestRestat`.
    fn supports_manifest_restat(&self) -> bool {
        self.ctx
            .interp
            .langs
            .ninja_version
            .as_deref()
            .is_none_or(|version| {
                !util::version_compare(
                    util::CompareOp::Less,
                    version,
                    REQUIRED_NINJA_VERSION_MANIFEST_RESTAT,
                )
            })
    }

    fn write_rebuild_manifest(&mut self, out: &mut String) {
        if self.regeneration_suppressed() {
            return;
        }
        let rule = Rule {
            name: "RERUN_CMAKE".into(),
            command: format!(
                "{} --regenerate-during-build{}{} -S{} -B{}",
                self.cmake_cmd,
                if self.ctx.interp.ignore_compile_warning_as_error {
                    " --compile-no-warning-as-error"
                } else {
                    ""
                },
                if self.ctx.interp.ignore_link_warning_as_error {
                    " --link-no-warning-as-error"
                } else {
                    ""
                },
                self.shell(&self.home_source),
                self.shell(&self.home_binary)
            ),
            description: "Re-running CMake...".into(),
            comment: "Rule for re-running cmake.".into(),
            generator: true,
            ..Rule::default()
        };
        self.add_rule(&rule);
        let mut rebuild = Build::new("RERUN_CMAKE");
        rebuild.comment = "Re-run CMake if any of its inputs changed.".into();
        rebuild.outputs.push(self.output_path("build.ninja"));
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
        if self.ctx.interp.instrument_build_hooks {
            let ctest = self
                .ctx
                .interp
                .cache
                .value("CMAKE_CTEST_COMMAND")
                .unwrap_or("ctest");
            self.add_rule(&Rule {
                name: "START_INSTRUMENT".into(),
                command: format!(
                    "exec {} --start-instrumentation {}",
                    self.shell(ctest),
                    self.shell(&self.home_binary)
                ),
                description: "Collecting build metrics".into(),
                comment: "Rule to initialize instrumentation daemon.".into(),
                restat: "1".into(),
                ..Rule::default()
            });
            let mut phony = Build::new("phony");
            phony.comment = "Phony target to keep START_INSTRUMENTATION out of date.".into();
            phony
                .outputs
                .push(self.output_path("CMakeFiles/instrument"));
            write_build(out, &phony, 0);
            let mut instrument = Build::new("START_INSTRUMENT");
            instrument.comment = "Start instrumentation daemon.".into();
            let output = self.output_path("start_instrument");
            instrument.outputs.push(output.clone());
            instrument.explicit_deps = phony.outputs;
            write_build(out, &instrument, 0);
            rebuild.explicit_deps.push(output);
        }
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
        // Inputs read by configure_file-style generation during generate
        // (Info.plist templates and friends).
        for extra in self.ctx.cmake_depend_files.borrow().iter() {
            deps.push(self.ninja_path(extra));
        }
        deps.push(self.output_path("CMakeCache.txt"));
        rebuild.variables.insert("pool".into(), "console".into());
        if let Some((script, stamp)) = self.ctx.interp.ext.glob_verify.clone() {
            self.write_verify_globs(out, &mut rebuild, &mut deps, &script, &stamp);
        }
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

    /// The `file(GLOB CONFIGURE_DEPENDS)` part of
    /// `cmGlobalNinjaGenerator::WriteTargetRebuildManifest`.
    fn write_verify_globs(
        &mut self,
        out: &mut String,
        rebuild: &mut Build,
        deps: &mut Vec<String>,
        script: &str,
        stamp: &str,
    ) {
        if !self.supports_manifest_restat() {
            let version = self
                .ctx
                .interp
                .langs
                .ninja_version
                .clone()
                .unwrap_or_default();
            self.ctx.message(
                MessageType::AuthorWarning,
                Vec::new(),
                format!(
                    "The detected version of Ninja:\n  {version}\nis less than the version of Ninja required by CMake for adding restat dependencies to the build.ninja manifest regeneration target:\n  1.8\nAny pre-check scripts, such as those generated for file(GLOB CONFIGURE_DEPENDS), will not be run by Ninja."
                ),
            );
            return;
        }
        let rule = Rule {
            name: "VERIFY_GLOBS".into(),
            command: format!("{} -P {}", self.cmake_cmd, self.shell(script)),
            description: "Re-checking globbed directories...".into(),
            comment: "Rule for re-checking globbed directories.".into(),
            generator: true,
            ..Rule::default()
        };
        self.add_rule(&rule);
        let mut phony = Build::new("phony");
        phony.comment = "Phony target to force glob verification run.".into();
        phony.outputs.push(format!("{script}_force"));
        write_build(out, &phony, 0);
        let stamp = self.output_path(stamp);
        let mut verify = Build::new("VERIFY_GLOBS");
        verify.comment = "Re-run CMake to check if globbed directories changed.".into();
        verify.outputs.push(stamp.clone());
        verify.implicit_deps = phony.outputs;
        verify.variables = rebuild.variables.clone();
        verify.variables.insert("restat".into(), "1".into());
        write_build(out, &verify, 0);
        deps.push(self.output_path(script));
        rebuild.explicit_deps.push(stamp);
    }

    /// `cmGlobalNinjaGenerator::AddRebuildManifestOutputs`: build.ninja then
    /// each directory's cmake_install.cmake and CTestTestfile.cmake, in the
    /// order the C++ adds them (per local generator: install, then test).
    fn order_rebuild_outputs(&self, _outputs: Vec<String>) -> Vec<String> {
        // AddRebuildManifestOutputs: build.ninja, then
        // cmGlobalGenerator::AddCMakeFilesToRebuild (install scripts of all
        // directories, then test files of directories with testing on).
        let mut outputs = Vec::new();
        if self.ctx.multi_config {
            // cmGlobalNinjaMultiGenerator::AddRebuildManifestOutputs.
            for config in &self.ctx.configs {
                outputs.push(self.output_path(&format!("CMakeFiles/impl-{config}.ninja")));
                outputs.push(self.output_path(&format!("build-{config}.ninja")));
            }
            if !self.ctx.default_file_config.is_empty() {
                outputs.push(self.output_path("build.ninja"));
            }
        } else {
            outputs.push(self.output_path("build.ninja"));
        }
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

    /// `cmGlobalNinjaGenerator::WriteTargetCleanAdditional`: the
    /// `CMakeFiles/clean_additional.cmake` script and its build statements
    /// in the common file. Returns whether there are files to clean.
    fn write_target_clean_additional(&mut self) -> bool {
        let script_rel = "CMakeFiles/clean_additional.cmake";
        let script_abs = format!("{}/{script_rel}", self.home_binary);
        let configs = self.ctx.configs.clone();
        if configs.iter().all(|config| {
            self.additional_clean_files
                .get(config)
                .is_none_or(BTreeSet::is_empty)
        }) {
            let _ = std::fs::remove_file(&script_abs);
            return false;
        }
        let mut script =
            String::from("# Additional clean files\ncmake_minimum_required(VERSION 3.16)\n");
        for config in &configs {
            let Some(files) = self.additional_clean_files.get(config) else {
                continue;
            };
            if files.is_empty() {
                continue;
            }
            let _ = write!(
                script,
                "\nif(\"${{CONFIG}}\" STREQUAL \"\" OR \"${{CONFIG}}\" STREQUAL \"{config}\")\n  file(REMOVE_RECURSE\n"
            );
            for file in files {
                let _ = writeln!(
                    script,
                    "  {}",
                    util::escape_for_cmake(&self.ninja_path(file))
                );
            }
            script.push_str("  )\nendif()\n");
        }
        if crate::fsutil::write_generated(std::path::Path::new(&script_abs), &script).is_err() {
            return false;
        }
        let rule = Rule {
            name: "CLEAN_ADDITIONAL".into(),
            command: format!(
                "{} -DCONFIG=$CONFIG -P {}",
                self.cmake_cmd,
                self.shell(&self.output_path(script_rel))
            ),
            description: "Cleaning additional files...".into(),
            comment: "Rule for cleaning additional files.".into(),
            ..Rule::default()
        };
        write_rule(&mut self.rules, &rule);
        let target = self.output_path("CMakeFiles/clean.additional");
        let mut build = Build::new("CLEAN_ADDITIONAL");
        build.comment = "Clean additional files.".into();
        for config in &configs {
            build.outputs = vec![self.ctx.build_alias(&target, config)];
            build.variables.insert("CONFIG".into(), config.clone());
            self.write_build_to(&Stream::Common, &build);
        }
        if self.ctx.multi_config {
            build.outputs = vec![target];
            build.variables.insert("CONFIG".into(), String::new());
            self.write_build_to(&Stream::Common, &build);
        }
        true
    }

    /// `cmGlobalNinjaGenerator::WriteTargetClean`.
    fn write_target_clean(&mut self) {
        let rule = Rule {
            name: "CLEAN".into(),
            command: format!("{} $FILE_ARG -t clean $TARGETS", self.ninja_cmd),
            description: "Cleaning all built files...".into(),
            comment: "Rule for cleaning all built files.".into(),
            ..Rule::default()
        };
        let multi = self.ctx.multi_config;
        let configs = self.ctx.configs.clone();
        // -- Additional clean target
        let additional = self.write_target_clean_additional();
        let additional_target = self.output_path("CMakeFiles/clean.additional");
        write_rule(&mut self.rules, &rule);
        let clean = self.output_path("clean");
        let byproducts = self.output_path("CMakeFiles/cmake_byproducts_for_clean_target");
        for config in &configs {
            let mut build = Build::new("CLEAN");
            build.comment = "Clean all the built files.".into();
            build.outputs.push(self.ctx.build_alias(&clean, config));
            if additional {
                build
                    .explicit_deps
                    .push(self.ctx.build_alias(&additional_target, config));
            }
            if multi {
                build.variables.insert(
                    "TARGETS".into(),
                    format!("{} {byproducts}", self.ctx.build_alias(&byproducts, config)),
                );
            }
            for file_config in &configs {
                if file_config != config && !self.cross_enabled() {
                    continue;
                }
                if multi {
                    build.variables.insert(
                        "FILE_ARG".into(),
                        format!(
                            "-f {}",
                            self.output_path(&format!("CMakeFiles/impl-{file_config}.ninja"))
                        ),
                    );
                }
                self.write_build_to(&Stream::Impl(file_config.clone()), &build);
            }
        }
        if self.cross_enabled() {
            let mut build = Build::new("CLEAN");
            build.comment = "Clean all the built files.".into();
            build.outputs.push(self.ctx.build_alias(&clean, "all"));
            if additional {
                for config in self.ctx.cross_configs.clone() {
                    build
                        .explicit_deps
                        .push(self.ctx.build_alias(&additional_target, &config));
                }
            }
            let mut targets: Vec<String> = self
                .ctx
                .cross_configs
                .iter()
                .map(|config| self.ctx.build_alias(&byproducts, config))
                .collect();
            targets.push("CMakeFiles/cmake_byproducts_for_clean_target".to_owned());
            build.variables.insert("TARGETS".into(), targets.join(" "));
            for file_config in &configs {
                build.variables.insert(
                    "FILE_ARG".into(),
                    format!(
                        "-f {}",
                        self.output_path(&format!("CMakeFiles/impl-{file_config}.ninja"))
                    ),
                );
                self.write_build_to(&Stream::Impl(file_config.clone()), &build);
            }
        }
        if multi {
            for config in &configs {
                let mut build = Build::new("phony");
                build.outputs.push(clean.clone());
                build
                    .explicit_deps
                    .push(self.ctx.build_alias(&clean, config));
                self.write_build_to(&Stream::Config(config.clone()), &build);
            }
            if !self.ctx.default_configs.is_empty() {
                let mut build = Build::new("phony");
                build.outputs.push(clean.clone());
                for config in self.ctx.default_configs.clone() {
                    build
                        .explicit_deps
                        .push(self.ctx.build_alias(&clean, &config));
                }
                self.write_build_to(&Stream::Default, &build);
            }
            // Write byproducts.
            let mut build = Build::new("phony");
            build.comment = "Clean byproducts.".into();
            build.outputs.push(byproducts.clone());
            build.explicit_deps = self.byproducts_for_clean.clone();
            self.write_build_to(&Stream::Common, &build);
            for config in &configs {
                build.outputs = vec![self.ctx.build_alias(&byproducts, config)];
                build.explicit_deps = self
                    .config_byproducts_for_clean
                    .get(config)
                    .cloned()
                    .unwrap_or_default();
                self.write_build_to(&Stream::Common, &build);
            }
        }
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
        build.outputs.push(self.output_path("help"));
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
                .any(|prop| target.props.contains(prop)),
            TargetKind::UnknownLibrary => false,
            _ => true,
        }
}

impl<'a> NinjaGenerator<'a> {
    /// `cmNinjaTargetGenerator::EnsureDirectoryExists`: relative paths are
    /// taken under the top binary directory minus the output path prefix.
    fn ensure_directory_exists(&self, path: &str) {
        if util::is_full_path(path) {
            let _ = std::fs::create_dir_all(path);
        } else {
            let mut full = format!("{}/", self.home_binary);
            if !self.output_prefix.is_empty() && full.ends_with(&self.output_prefix) {
                full.truncate(full.len() - self.output_prefix.len());
            }
            full.push_str(path);
            let _ = std::fs::create_dir_all(full);
        }
    }

    /// `cmNinjaTargetGenerator::EnsureParentDirectoryExists`.
    fn ensure_parent_directory_exists(&self, path: &str) {
        self.ensure_directory_exists(&util::filename_path(path));
    }

    /// `cmGlobalNinjaGenerator::SupportsCWDDepend`: the detected ninja
    /// binary understands order-only dependencies on the working
    /// directory (ninja 1.7+).
    fn supports_cwd_depend(&self) -> bool {
        match &self.ctx.interp.langs.ninja_version {
            Some(version) => !util::version_compare(util::CompareOp::Less, version, "1.7"),
            None => false,
        }
    }

    /// `cmGlobalNinjaGenerator::IsGCCOnWindows`
    /// (`DetectGCCOnWindows`): GCC-like compilers on a Windows host —
    /// detected per enabled language when CMake is built for Windows.
    fn is_gcc_on_windows(&self) -> bool {
        #[cfg(windows)]
        {
            let makefile = 0;
            self.ctx.interp.langs.enabled.iter().any(|language| {
                let compiler_id = self
                    .ctx
                    .var_safe(makefile, &format!("CMAKE_{language}_COMPILER_ID"));
                let simulate_id = self
                    .ctx
                    .var_safe(makefile, &format!("CMAKE_{language}_SIMULATE_ID"));
                let frontend_variant = self.ctx.var_safe(
                    makefile,
                    &format!("CMAKE_{language}_COMPILER_FRONTEND_VARIANT"),
                );
                (compiler_id == "Clang" && frontend_variant == "GNU")
                    || (simulate_id != "MSVC"
                        && (compiler_id == "GNU"
                            || compiler_id == "QCC"
                            || compiler_id.ends_with("Clang")))
            })
        }
        #[cfg(not(windows))]
        {
            false
        }
    }

    fn object_dir(&self, target: &Target) -> String {
        // cmNinjaTargetGenerator names objects by their normalized directory.
        gt::support_directory(&self.ctx, target) + &self.ctx.config_directory(&self.config)
    }

    /// `cmNinjaTargetGenerator::LanguageCompilerRule`.
    fn compiler_rule_name(&self, language: &str, target: &Target, scanned: bool) -> String {
        format!(
            "{}_COMPILER__{}_{}_{}",
            language,
            encode_rule_name(&target.name),
            if scanned { "scanned" } else { "unscanned" },
            self.config
        )
    }

    /// The concrete `<CUDA_COMPILE_MODE>` replacement of
    /// `cmNinjaTargetGenerator::WriteCompileRule` and
    /// `ExportObjectCompileCommand`.
    fn cuda_compile_mode(&self, target: &Target) -> String {
        let mut mode = String::new();
        if target
            .props
            .get("CUDA_SEPARABLE_COMPILATION")
            .is_some_and(util::is_on)
        {
            mode.push_str(&self.ctx.var_safe(target.makefile, "_CMAKE_CUDA_RDC_FLAG"));
            mode.push(' ');
        }
        let mut normal = true;
        for (property, variable) in [
            ("CUDA_PTX_COMPILATION", "_CMAKE_CUDA_PTX_FLAG"),
            ("CUDA_CUBIN_COMPILATION", "_CMAKE_CUDA_CUBIN_FLAG"),
            ("CUDA_FATBIN_COMPILATION", "_CMAKE_CUDA_FATBIN_FLAG"),
            ("CUDA_OPTIX_COMPILATION", "_CMAKE_CUDA_OPTIX_FLAG"),
        ] {
            if target.props.get(property).is_some_and(util::is_on) {
                mode.push_str(&self.ctx.var_safe(target.makefile, variable));
                normal = false;
                break;
            }
        }
        if normal {
            mode.push_str(&self.ctx.var_safe(target.makefile, "_CMAKE_CUDA_WHOLE_FLAG"));
        }
        mode
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

    fn write_compile_rule(&mut self, language: &str, target: &Target, scanned: bool) {
        let name = self.compiler_rule_name(language, target, scanned);
        if self.has_rule(&name) {
            return;
        }
        // These languages need generator paths (ISPC headers and object
        // variants, Swift module/library builds) that are not ported; say so
        // instead of writing a rule with unexpanded placeholders.
        if matches!(language, "ISPC" | "Swift") {
            self.ctx.error(
                target.backtrace.clone(),
                format!(
                    "The target \"{}\" has {language} sources, but cmake-rust does not implement {language} compilation in the Ninja generators.",
                    target.name
                ),
            );
        }
        let makefile = target.makefile;
        let mut rule = Rule {
            name,
            ..Rule::default()
        };
        let mut flags_text = "$FLAGS".to_owned();
        // `cmNinjaTargetGenerator::NeedExplicitPreprocessing`.
        let compilation_preprocesses = language != "Fortran" && language != "Swift";
        let depfile_format = self
            .ctx
            .var_safe(makefile, &format!("CMAKE_{language}_DEPFILE_FORMAT"));
        if !compilation_preprocesses {
            // The explicit scan step owns the preprocessing dependencies.
        } else if depfile_format == "msvc" {
            rule.deptype = "msvc".into();
        } else {
            rule.deptype = "gcc".into();
            rule.depfile = "$DEP_FILE".into();
        }
        // Explicitly preprocessed sources do not need the defines, unless
        // the compiler wants them for some other purpose.
        let mut defines = if scanned
            && matches!(language, "Fortran" | "Swift")
            && !util::is_on_opt(
                self.ctx
                    .var(makefile, &format!("CMAKE_{language}_COMPILE_WITH_DEFINES"))
                    .as_deref(),
            ) {
            ""
        } else {
            "$DEFINES"
        };
        let mut includes = "$INCLUDES";
        let mut source = "$in";
        // If using a response file, move defines, includes, and flags into it.
        let response_flag = self.response_file_flag(target, language);
        if !response_flag.is_empty() {
            rule.rspfile = "$RSP_FILE".into();
            rule.rspcontent = format!(" {defines} {includes} {flags_text}");
            flags_text = format!("{response_flag}$RSP_FILE");
            defines = "";
            includes = "";
            if language == "Swift" {
                rule.rspcontent.push_str(" $in");
                source = "";
            }
        }
        let depfile_flags = self
            .ctx
            .var_safe(makefile, &format!("CMAKE_DEPFILE_FLAGS_{language}"));
        if compilation_preprocesses && !depfile_flags.is_empty() {
            let expanded = flags::expand_rule(
                &self.ctx,
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
        if scanned
            && !self
                .ctx
                .var_safe(makefile, &format!("CMAKE_{language}_MODULE_MAP_FORMAT"))
                .is_empty()
        {
            let flag = self
                .ctx
                .var_safe(makefile, &format!("CMAKE_{language}_MODULE_MAP_FLAG"));
            flags_text.push(' ');
            flags_text.push_str(&flag.replace("<MODULE_MAP_FILE>", "$DYNDEP_MODULE_MAP_FILE"));
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
        let launcher = rule_launcher(&self.ctx, target, "RULE_LAUNCH_COMPILE");
        let vars: Vec<(&str, String)> = vec![
            ("TARGET_NAME", target.name.clone()),
            ("TARGET_TYPE", target.kind.type_name().to_owned()),
            ("LANGUAGE", language.to_owned()),
            ("SOURCE", source.into()),
            ("OBJECT", "$out".into()),
            ("DEFINES", defines.into()),
            ("INCLUDES", includes.into()),
            ("TARGET_PDB", "$TARGET_PDB".into()),
            ("TARGET_COMPILE_PDB", "$TARGET_COMPILE_PDB".into()),
            ("OBJECT_DIR", "$OBJECT_DIR".into()),
            ("OBJECT_FILE_DIR", "$OBJECT_FILE_DIR".into()),
            ("CONFIG", "$CONFIG".into()),
            ("FLAGS", flags_text),
            ("DEP_FILE", rule.depfile.clone()),
            ("DEP_TARGET", "$out".into()),
            (
                "CUDA_COMPILE_MODE",
                if language == "CUDA" {
                    self.cuda_compile_mode(target)
                } else {
                    String::new()
                },
            ),
        ];
        let expanded: Vec<String> = commands
            .iter()
            .map(|command| {
                flags::expand_rule(&self.ctx, target, &format!("{launcher}{command}"), &vars)
            })
            .collect();
        rule.command = self.build_command_line(&expanded);
        rule.comment = format!("Rule for compiling {language} files.");
        rule.description = format!("Building {language} object $out");
        self.add_rule(&rule);
    }

    /// The CMP0154 split in `cmNinjaTargetGenerator::WriteObjectBuildStatements`:
    /// custom command outputs (in order) into those other targets may need
    /// (interface file sets, or everything under OLD) and those only this
    /// target's compilations need. Non-includeable file-set members (C++
    /// modules) are dropped from both.
    fn split_private_outputs(
        &self,
        target: &Target,
        outputs: Vec<String>,
    ) -> (Vec<String>, Vec<String>) {
        let use_private = !target.file_sets.is_empty()
            && target
                .policies
                .get(crate::policy::index_of("CMP0154").unwrap())
                == Some(crate::policy::PolicyStatus::New);
        if !use_private {
            return (outputs, Vec::new());
        }
        // cmGeneratorTarget::BuildFileSetInfoCache; its diagnostics were
        // already issued when the file sets were first evaluated.
        let reported = self.ctx.errors.borrow().len();
        let mut file_set_of: HashMap<String, &crate::state::FileSet> = HashMap::new();
        for set in target.file_sets.values() {
            for (file, _, _) in gt::evaluate_file_set(&self.ctx, target, set) {
                file_set_of.insert(file, set);
            }
        }
        self.ctx.errors.borrow_mut().truncate(reported);
        let mut public = Vec::new();
        let mut private = Vec::new();
        for output in outputs {
            let set = file_set_of.get(&util::collapse_full_path(&output, ""));
            let includeable = set.is_none_or(|set| set.kind == "HEADERS");
            if set.is_some_and(|set| set.scope.interface()) && includeable {
                public.push(output);
            } else if includeable {
                private.push(output);
            }
        }
        (public, private)
    }

    /// `cmGeneratorTarget::CheckCxxModuleStatus`, plus the per-source
    /// diagnostics from cmNinjaTargetGenerator::WriteObjectBuildStatement
    /// (a `CXX_MODULES` file-set member not classified as CXX) and
    /// cmDyndepCollation (a `CXX_MODULES` file-set member not scheduled
    /// for compilation).
    fn check_cxx_module_status(&mut self, target: &Target, sources: &[gt::SourceInfo]) {
        let modules = ModuleSupport::new(&self.ctx, target, &self.config);
        let scannable = gt::have_cxx20_module_sources(target)
            || sources
                .iter()
                .any(|source| source.language == "CXX" && modules.need_dyndep_for_source(source));
        if !scannable {
            return;
        }
        // Ninja has module support; only the toolchain level can fail.
        let module_files = modules.files();
        let level = modules.level();
        match level {
            CxxSupportLevel::MissingCxx => self.ctx.error(
                self.ctx.dir_backtrace(target.makefile),
                format!(
                    "The target named \"{}\" has C++ sources that use modules, but the \"CXX\" language has not been enabled.",
                    target.name
                ),
            ),
            CxxSupportLevel::NoCxx20 => {
                let effective = match flags::standard::effective_standard(
                    &self.ctx,
                    target,
                    &self.config,
                    "CXX",
                ) {
                    Some(standard) => format!("; found \"cxx_std_{standard}\""),
                    None => "; no C++ standard found".to_owned(),
                };
                self.ctx.error(
                    self.ctx.dir_backtrace(target.makefile),
                    format!(
                        "The target named \"{}\" has C++ sources that use modules, but does not include \"cxx_std_20\" (or newer) among its `target_compile_features`{effective}.",
                        target.name
                    ),
                );
            }
            CxxSupportLevel::MissingRule => self.ctx.error(
                self.ctx.dir_backtrace(target.makefile),
                format!(
                    "The target named \"{}\" has C++ sources that may use modules, but the compiler does not provide a way to discover the import graph dependencies.  See the cmake-cxxmodules(7) manual for details.  Use the CMAKE_CXX_SCAN_FOR_MODULES variable to enable or disable scanning.",
                    target.name
                ),
            ),
            CxxSupportLevel::Supported => {}
        }
        for file in module_files {
            match sources
                .iter()
                .find(|source| &source.path == file && source.kind == SourceKind::ObjectSource)
            {
                // cmDyndepCollation: emitted during object builds upstream;
                // here it is reported only when module support is available,
                // matching collation's gating on NeedDyndep.
                None if level == CxxSupportLevel::Supported => self.ctx.error(
                    self.ctx.dir_backtrace(target.makefile),
                    format!(
                        "Target \"{}\" has source file\n  {file}\nin a \"FILE_SET TYPE CXX_MODULES\" but it is not scheduled for compilation.",
                        target.name
                    ),
                ),
                Some(source) if source.language != "CXX" => self.ctx.error(
                    self.ctx.dir_backtrace(target.makefile),
                    format!(
                        "Target \"{}\" contains the source\n  {file}\nin a file set of type \"CXX_MODULES\" but the source is not classified as a \"CXX\" source.",
                        target.name
                    ),
                ),
                _ => {}
            }
        }
    }

    /// `cmNinjaNormalTargetGenerator::Generate`.
    fn generate_normal_target(&mut self, target: &'a Target) {
        let config = self.config.clone();
        // The cmNinjaNormalTargetGenerator constructor ensures the output
        // directory exists for every config (needed at compile time on
        // Windows, e.g. the OutDir test).
        if target.kind != TargetKind::ObjectLibrary {
            for cfg in self.ctx.configs.clone() {
                let directory = gt::directory(
                    &self.ctx.for_config(&cfg),
                    target,
                    &cfg,
                    ArtifactType::Runtime,
                );
                self.ensure_directory_exists(&directory);
            }
        }
        // cmNinjaTargetGenerator::Configs[config] state for this target.
        self.mac_content_folders.clear();
        self.extra_files.clear();
        let link_language = gt::linker_language(&self.ctx, target, &config);
        if link_language.is_empty() {
            // cmSystemTools::Error marks the generation failed.
            self.ctx.report_unknown_linker_language(target);
            return;
        }
        // `EntriesProcessing` constructor in cmComputeLinkDepends::Compute:
        // the CMP0156 warning is emitted by the first GetLinkInformation
        // for each target, in target generation order.
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
                    "{}\nSince the policy is not set, legacy libraries de-duplication strategy will be applied.",
                    crate::policy::policy_warning(crate::policy::index_of("CMP0156").unwrap())
                ),
            );
        }
        let sources = gt::sources(&self.ctx, target, &config);
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
            if language == "Fortran" {
                self.write_fortran_scan_rules(target);
                self.write_compile_rule(language, target, true);
            }
            // `WriteLanguageRules`: the scanned CXX rule exists whenever the
            // toolchain supports module scanning (`NeedDyndep("CXX")`), even
            // when no source of this target is scanned.
            if language == "CXX"
                && module_support::support_level(&self.ctx, target, &config)
                    == CxxSupportLevel::Supported
            {
                self.write_cxx_scan_rules(target);
                self.write_compile_rule(language, target, true);
            }
            self.write_compile_rule(language, target, false);
        }
        // WriteObjectBuildStatements for each file config whose cross
        // configs include this config (only the config's own file without
        // cross-config builds).  `Configs[config].ScanningInfo` is fresh
        // per (target, config) and accumulates over the loop.
        let file_configs = self.file_configs_for(&config);
        self.scanning_info.clear();
        let saved_stream = self.stream.clone();
        let saved_file_config = self.file_config.clone();
        let mut objects = Vec::new();
        for (index, file_config) in file_configs.iter().enumerate() {
            self.file_config = file_config.clone();
            self.stream = Stream::Impl(file_config.clone());
            let written = self.write_object_build_statements(target, &sources, index == 0);
            if index == 0 {
                objects = written;
            }
        }
        if target.kind == TargetKind::ObjectLibrary {
            self.file_config = saved_file_config;
            self.stream = saved_stream;
            // WriteObjectLibStatement: the phony lives in the common file.
            let mut build = Build::new("phony");
            build.comment = format!("Object library {}", target.name);
            self.append_target_outputs(target, &mut build.outputs, DependOn::TargetArtifact);
            self.config_byproducts_for_clean
                .entry(config.clone())
                .or_default()
                .extend(build.outputs.iter().cloned());
            build.explicit_deps = objects;
            self.write_build_to(&Stream::Common, &build);
            self.add_target_alias(&target.name.clone(), target);
        } else {
            for (index, file_config) in file_configs.iter().enumerate() {
                self.file_config = file_config.clone();
                self.stream = Stream::Impl(file_config.clone());
                self.write_link_statement(target, &link_language, &objects, index == 0);
            }
            self.file_config = saved_file_config;
            self.stream = saved_stream;
        }
        if self.cross_enabled() {
            self.add_target_alias_cfg(&target.name.clone(), target, "all");
        }
        // Find ADDITIONAL_CLEAN_FILES.
        self.add_additional_clean_files(target.makefile, Some(target));
    }

    /// `cmNinjaTargetGenerator::WriteObjectBuildStatements(config,
    /// fileConfig, firstForConfig)`: returns the target's objects.
    fn write_object_build_statements(
        &mut self,
        target: &'a Target,
        sources: &[gt::SourceInfo],
        first_for_config: bool,
    ) -> Vec<String> {
        let config = self.config.clone();
        self.check_cxx_module_status(target, sources);
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
        let module_ctx = self.ctx.clone();
        let modules = ModuleSupport::new(&module_ctx, target, &config);
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
        // cmOSXBundleGenerator::GenerateMacOSXContentStatements over the
        // header and extra sources, writing COPY_OSX_CONTENT_* builds to
        // this file config's stream.
        self.write_mac_osx_content_statements(&mut out, target, sources);
        let object_dir = self.object_dir(target);
        if !self.supports_cwd_depend() {
            self.ensure_directory_exists(&object_dir);
        }
        let order_target = self.ctx.order_depends_name(&target.name, &config);
        let mut object_order_target = order_target.clone();
        {
            let mut build = Build::new("phony");
            build.comment = format!("Order-only phony target for {}", target.name);
            build.outputs.push(order_target.clone());
            let mut order = Vec::new();
            self.append_target_depends(target, &mut order, DependOn::TargetOrdering);
            // Order-only dependencies on custom command outputs; with
            // CMP0154 NEW, outputs outside the target's interface file sets
            // are private to its own compilations.
            let (public, private) = self.split_private_outputs(target, custom_outputs);
            // Order-only dependencies on other files associated with the
            // target (`Configs[config].ExtraFiles`: bundle content copies).
            order.extend(self.extra_files.iter().cloned());
            order.extend(public.iter().map(|path| self.ninja_path(path)));
            order.sort();
            order.dedup();
            if order.is_empty() {
                // The phony target must have at least one input.
                if self.supports_cwd_depend() {
                    order.push(self.ninja_path("."));
                } else {
                    // Any path that always exists will work here.
                    order.push(self.ninja_path(&self.object_dir(target)));
                }
            }
            build.order_only_deps = order;
            write_build(&mut out, &build, 0);
            if !private.is_empty() {
                let private_target = format!("{order_target}_private");
                let mut build = Build::new("phony");
                build.outputs.push(private_target.clone());
                let mut order = vec![order_target.clone()];
                order.extend(private.iter().map(|path| self.ninja_path(path)));
                order.sort();
                order.dedup();
                build.order_only_deps = order;
                write_build(&mut out, &build, 0);
                object_order_target = private_target;
            }
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
                &self.ctx,
                target,
                &config,
                source,
                &source_props,
            );
            let is_pch_obj = pch_extension
                .as_ref()
                .is_some_and(|ext| object.ends_with(ext));
            let scanned = !is_pch_obj
                && (source.language == "Fortran"
                    || (source.language == "CXX" && modules.need_dyndep_for_source(source)));
            let mut build = Build::new(&self.compiler_rule_name(&source.language, target, scanned));
            build.outputs.push(object.clone());
            // cmNinjaTargetGenerator::GetCompiledSourceNinjaPath: sources are
            // passed to the compiler by absolute path.
            let source_abs = self.ninja_abs_path(&source.path);
            build.explicit_deps.push(source_abs.clone());
            // Precompiled header dependencies.
            let pch_archs = super::pch::pch_archs(&self.ctx, target, &config, &source.language);
            let mut pch_sources = std::collections::HashSet::new();
            for arch in &pch_archs {
                let pch_source =
                    super::pch::pch_source(&self.ctx, target, &config, &source.language, arch);
                if !pch_source.is_empty() {
                    pch_sources.insert(pch_source);
                }
            }
            if !pch_sources.is_empty() && source_props.get("SKIP_PRECOMPILE_HEADERS").is_none() {
                for arch in &pch_archs {
                    let header =
                        super::pch::pch_header(&self.ctx, target, &config, &source.language, arch);
                    if !header.is_empty() {
                        build
                            .implicit_deps
                            .push(self.ninja_path(&util::collapse_full_path(&header, "")));
                    }
                    if !pch_sources.contains(&source.path) {
                        let file = super::pch::pch_file(
                            &self.ctx,
                            target,
                            &config,
                            &source.language,
                            arch,
                        );
                        if !file.is_empty() {
                            // Full paths in the dependency list are collapsed.
                            build
                                .implicit_deps
                                .push(self.ninja_path(&util::collapse_full_path(&file, "")));
                        }
                    }
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
            build.order_only_deps.push(object_order_target.clone());
            // A GENERATED source with no custom command (on itself or on
            // any other source) is assumed to be produced by one of the
            // order-only dependencies (cmNinjaTargetGenerator's
            // AddAssumedSourceDependencies).
            if source.generated
                && !source_props
                    .get("__CMAKE_GENERATED_BY_CMAKE")
                    .is_some_and(util::is_on)
                && self
                    .attached_command(target.makefile, &source.path)
                    .is_none()
                && !self.custom_command_outputs.contains(&source_abs)
            {
                self.assumed_source_dependencies
                    .entry(source_abs)
                    .or_default()
                    .extend(build.order_only_deps.iter().cloned());
            }
            build.variables.insert("FLAGS".into(), vars.flags.clone());
            build.variables.insert("DEFINES".into(), vars.defines);
            build.variables.insert("INCLUDES".into(), vars.includes);
            build.variables.insert("CONFIG".into(), config.clone());
            let depfile_format = self.ctx.var_safe(
                target.makefile,
                &format!("CMAKE_{}_DEPFILE_FORMAT", source.language),
            );
            if depfile_format != "msvc" {
                // cmNinjaTargetGenerator::AddDepfileBinding.
                let replace_ext = !source.language.is_empty()
                    && self
                        .ctx
                        .var(
                            target.makefile,
                            &format!("CMAKE_{}_DEPFILE_EXTENSION_REPLACE", source.language),
                        )
                        .is_some_and(|value| util::is_on(&value));
                let depfile = if replace_ext {
                    let name = util::filename_name(&object);
                    let stem = name.rsplit_once('.').map(|(stem, _)| stem).unwrap_or(&name);
                    format!("{object_file_dir}/{stem}.d")
                } else {
                    format!("{object}.d")
                };
                self.add_depfile_binding(&mut build.variables, depfile);
            }
            let object_dir_rel = self.ninja_path(&object_dir);
            let code_check = if source_props.get("SKIP_LINTING").is_some_and(util::is_on) {
                String::new()
            } else {
                self.code_check(target, source, &object_dir_rel)
            };
            if code_check.is_empty() {
                build.variables.insert(
                    "LAUNCHER".into(),
                    flags::compiler_launcher(&self.ctx, target, &source.language),
                );
            } else {
                build.variables.insert("CODE_CHECK".into(), code_check);
            }
            build
                .variables
                .insert("OBJECT_DIR".into(), self.shell(&object_dir_rel));
            build
                .variables
                .insert("OBJECT_FILE_DIR".into(), self.shell(&object_file_dir));
            self.set_msvc_target_pdb_variable(target, &mut build.variables);
            let mut export_vars = build.variables.clone();
            export_vars.insert("FLAGS".into(), vars.flags_untrimmed.clone());
            if first_for_config {
                if scanned && source.language == "CXX" {
                    self.export_object_compile_command_scanned(
                        target,
                        source,
                        &object,
                        &export_vars,
                    );
                } else {
                    self.export_object_compile_command(target, source, &object, &export_vars);
                }
            }
            if scanned && source.language == "Fortran" {
                let ddi =
                    self.write_fortran_scan(&mut out, target, source, &source_props, &mut build);
                if first_for_config {
                    self.scanning_info
                        .entry("Fortran".to_owned())
                        .or_default()
                        .push(ScanningFiles {
                            scanning_output: Some(ddi),
                            ..ScanningFiles::default()
                        });
                }
            } else if scanned && source.language == "CXX" {
                let (ddi, modmap) = self.write_cxx_scan(&mut out, target, &mut build);
                let files = ScanningFiles {
                    scanning_output: first_for_config.then_some(ddi),
                    module_map_file: modmap,
                };
                if !files.is_empty() {
                    self.scanning_info
                        .entry("CXX".to_owned())
                        .or_default()
                        .push(files);
                }
            }
            build.rsp_file = format!("{object}.rsp");
            let limit = self.compile_command_limit(target, &source.language);
            write_build(&mut out, &build, limit);
            if let Some(outputs) = source_props.get("OBJECT_OUTPUTS") {
                let evaluated = self.ctx.genex(outputs, None, None);
                if !evaluated.is_empty() {
                    let mut extra = Build::new("phony");
                    extra.comment = "Additional output files.".into();
                    extra.outputs = util::expand_list(&evaluated, false)
                        .iter()
                        .map(|output| self.ninja_path(output))
                        .collect();
                    extra.explicit_deps = build.outputs.clone();
                    write_build(&mut out, &extra, 0);
                }
            }
            // PCH objects are not linked.
            let is_pch = self
                .ctx
                .var(target.makefile, "CMAKE_PCH_EXTENSION")
                .is_some_and(|extension| object.ends_with(&extension));
            if !is_pch {
                objects.push(object);
            }
        }
        // `GetCxxModuleSources`: BMI-only C++ module sources (CXX_MODULES
        // file-set members of a non-normal target) get a scan build and a
        // BMI compile in every file config.
        for source in sources
            .iter()
            .filter(|s| s.kind == SourceKind::CxxModuleSource)
        {
            let (ddi, modmap) =
                self.write_cxx_bmi_build(&mut out, target, source, &order_target, first_for_config);
            let files = ScanningFiles {
                scanning_output: first_for_config.then_some(ddi),
                module_map_file: modmap,
            };
            if !files.is_empty() {
                self.scanning_info
                    .entry("CXX".to_owned())
                    .or_default()
                    .push(files);
            }
        }
        // Check if there are Fortran objects which need to participate in
        // forwarding module requirements.
        if self.has_fortran_sources(target) && !self.scanning_info.contains_key("Fortran") {
            self.scanning_info
                .entry("Fortran".to_owned())
                .or_default()
                .push(ScanningFiles::default());
            self.write_fortran_scan_rules(target);
            self.write_compile_rule("Fortran", target, true);
        }
        // `Configs[config].ScanningInfo` is a std::map (sorted): "CXX" comes
        // before "Fortran" in the dyndep build statements.  It accumulates
        // across file configs, so later configs' dyndep builds repeat the
        // `.ddi` inputs and re-list module-map outputs.
        let scanning: Vec<(String, Vec<ScanningFiles>)> = self
            .scanning_info
            .iter()
            .map(|(language, files)| (language.clone(), files.clone()))
            .collect();
        for (language, files) in scanning {
            match language.as_str() {
                "CXX" => self.write_cxx_dyndep(&mut out, target, sources, &files),
                "Fortran" => self.write_fortran_dyndep(&mut out, target, sources, &files),
                _ => {}
            }
        }
        out.push('\n');
        self.out().push_str(&out);
        objects
    }

    /// `cmOSXBundleGenerator::GenerateMacOSXContentStatements` driven over
    /// the target's header and extra source kinds, and the copy-build
    /// emission of `MacOSXContentGeneratorType::operator()` +
    /// `cmGlobalNinjaGenerator::WriteMacOSXContentBuild`.
    fn write_mac_osx_content_statements(
        &mut self,
        out: &mut String,
        target: &'a Target,
        sources: &[gt::SourceInfo],
    ) {
        // GenerateMacOSXContentStatements::MustSkip
        // (HaveWellDefinedOutputFiles).
        if !matches!(
            target.kind,
            TargetKind::StaticLibrary
                | TargetKind::SharedLibrary
                | TargetKind::ModuleLibrary
                | TargetKind::ObjectLibrary
                | TargetKind::Executable
        ) {
            return;
        }
        let config = self.config.clone();
        let file_config = self.file_config.clone();
        // Upstream iterates the header sources and then the extra sources
        // as two separate lists.
        for kind in [SourceKind::Header, SourceKind::Extra] {
            for source in sources.iter().filter(|source| source.kind == kind) {
                let flags =
                    osx_bundle::target_source_file_flags(&self.ctx, target, &source.path, None);
                if flags.kind == SourceFileType::Normal {
                    continue;
                }
                // MacOSXContentGeneratorType::operator(): skip content when
                // not building a framework or bundle.
                if !target.is_bundle_on_apple() {
                    continue;
                }
                let mut folders = std::mem::take(&mut self.mac_content_folders);
                let macdir = osx_bundle::init_mac_osx_content_directory(
                    &self.ctx,
                    target,
                    &flags.mac_folder,
                    &config,
                    &mut folders,
                );
                // Reject files that collide with files from the Ninja
                // file's native config.
                if config != file_config {
                    let native = osx_bundle::init_mac_osx_content_directory(
                        &self.ctx,
                        target,
                        &flags.mac_folder,
                        &file_config,
                        &mut folders,
                    );
                    if macdir == native {
                        self.mac_content_folders = folders;
                        continue;
                    }
                }
                self.mac_content_folders = folders;
                let input = self.ninja_path(&source.path);
                let output =
                    self.ninja_path(&format!("{}/{}", macdir, util::filename_name(&input)));
                // cmGlobalNinjaGenerator::AddMacOSXContentRule +
                // WriteMacOSXContentBuild.
                self.add_mac_osx_content_rules();
                let rule = if std::path::Path::new(&source.path).is_dir() {
                    "COPY_OSX_CONTENT_DIR"
                } else {
                    "COPY_OSX_CONTENT_FILE"
                };
                let mut build = Build::new(rule);
                build.outputs.push(output.clone());
                build.explicit_deps.push(input);
                write_build(out, &build, 0);
                // `Configs[config].ExtraFiles`.
                self.extra_files.push(output);
            }
        }
    }

    /// `cmGlobalNinjaGenerator::AddMacOSXContentRule`.
    fn add_mac_osx_content_rules(&mut self) {
        for (name, command) in [
            ("COPY_OSX_CONTENT_FILE", "copy"),
            ("COPY_OSX_CONTENT_DIR", "copy_directory"),
        ] {
            self.add_rule(&Rule {
                name: name.into(),
                command: format!("{} -E {command} $in $out", self.cmake_cmd),
                description: "Copying OS X Content $out".into(),
                comment: format!(
                    "Rule for copying OS X bundle content {}, with style.",
                    if name.ends_with("FILE") {
                        "file"
                    } else {
                        "dir"
                    }
                ),
                ..Rule::default()
            });
        }
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
        let compile_pdb = gt::target_compile_pdb(&self.ctx, target, &config);
        let pdb = if matches!(
            target.kind,
            TargetKind::Executable
                | TargetKind::StaticLibrary
                | TargetKind::SharedLibrary
                | TargetKind::ModuleLibrary
        ) {
            let names = gt::names(&self.ctx, target, &config);
            format!(
                "{}/{}",
                gt::pdb_directory(&self.ctx, target, &config),
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
        // EnsureParentDirectoryExists for both program databases.
        self.ensure_directory_exists(&util::filename_path(&pdb));
        self.ensure_directory_exists(&util::filename_path(&compile_pdb));
        true
    }

    /// `cmNinjaNormalTargetGenerator::ComputeLinkCmd` for host link rules.
    fn link_commands(&self, target: &Target, language: &str) -> Vec<String> {
        let create = match target.kind {
            // cmGeneratorTarget::GetCreateRuleVariable.
            TargetKind::Executable => {
                let with_exports = format!("CMAKE_{language}_LINK_EXECUTABLE_WITH_EXPORTS");
                if target.is_executable_with_exports()
                    && self.ctx.var(target.makefile, &with_exports).is_some()
                {
                    return self
                        .ctx
                        .var(target.makefile, &with_exports)
                        .map(|commands| util::expand_list(&commands, false))
                        .unwrap_or_default();
                }
                "LINK_EXECUTABLE"
            }
            TargetKind::StaticLibrary => "CREATE_STATIC_LIBRARY",
            TargetKind::SharedLibrary => {
                if target.is_archived_aix_shared_library() {
                    return self
                        .ctx
                        .var(
                            target.makefile,
                            &format!("CMAKE_{language}_CREATE_SHARED_LIBRARY_ARCHIVE"),
                        )
                        .map(|commands| util::expand_list(&commands, false))
                        .unwrap_or_default();
                }
                "CREATE_SHARED_LIBRARY"
            }
            TargetKind::ModuleLibrary => "CREATE_SHARED_MODULE",
            _ => return Vec::new(),
        };
        // cmGeneratorTarget::GetFeatureSpecificLinkRuleVariable applies to
        // every create rule (GetCreateRuleVariable) and the archive rules.
        let ipo = flags::ipo_enabled(&self.ctx, target, &self.config, language);
        let rule = |suffix| {
            let var = format!("CMAKE_{language}_{suffix}");
            if ipo && let Some(value) = self.ctx.var(target.makefile, &format!("{var}_IPO")) {
                return Some(value);
            }
            self.ctx.var(target.makefile, &var)
        };
        if let Some(commands) = rule(create) {
            let mut commands = commands;
            if gt::has_implib_gnu_to_ms(&self.ctx, target, &self.config) {
                let rule_var = format!(
                    "CMAKE_{}_GNUtoMS_RULE",
                    gt::linker_language(&self.ctx, target, &self.config)
                );
                if let Some(extra) = self.ctx.var(target.makefile, &rule_var) {
                    commands.push_str(&extra);
                }
            }
            return util::expand_list(&commands, false);
        }
        if target.kind != TargetKind::StaticLibrary {
            return Vec::new();
        }
        // Only the incremental archiver rules need the old archive removed.
        let mut commands = vec![format!("{} -E rm -f <TARGET>", self.cmake_cmd)];
        for suffix in ["ARCHIVE_CREATE", "ARCHIVE_FINISH"] {
            if let Some(value) = rule(suffix) {
                util::expand_list_into(&value, false, &mut commands);
            } else {
                self.ctx.error(target.backtrace.clone(), format!(
                    "Error required internal CMake variable not set, cmake may not be built correctly.\nMissing variable is:\nCMAKE_{language}_{suffix}"
                ));
            }
        }
        commands
    }

    fn write_link_rules(
        &mut self,
        target: &Target,
        language: &str,
        has_depfile: bool,
        symlink: bool,
        use_rsp: bool,
    ) {
        let makefile = target.makefile;
        let rule_name = self.linker_rule_name(language, target);
        if !self.has_rule(&rule_name) {
            // cmNinjaNormalTargetGenerator::WriteLinkStatement: PRE_BUILD and
            // PRE_LINK comments precede the link description, POST_BUILD
            // follows.
            let mut pre_link_comments: Vec<String> = Vec::new();
            let mut post_build_comments: Vec<String> = Vec::new();
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
                    if let Some(comment) = self.evaluate_command(command).comment {
                        if stage == CustomCommandKind::PostBuild {
                            post_build_comments.push(comment);
                        } else {
                            pre_link_comments.push(comment);
                        }
                    }
                }
            }
            let mut rule = Rule {
                name: rule_name.clone(),
                ..Rule::default()
            };
            if has_depfile {
                rule.deptype = self
                    .ctx
                    .var_safe(makefile, &format!("CMAKE_{language}_LINKER_DEPFILE_FORMAT"));
                rule.depfile = "$DEP_FILE".into();
            }
            let mut commands = self.link_commands(target, language);
            if matches!(
                target.kind,
                TargetKind::Executable | TargetKind::SharedLibrary | TargetKind::ModuleLibrary
            ) && target
                .props
                .get("LINK_WHAT_YOU_USE")
                .is_some_and(util::is_on)
                && !self
                    .ctx
                    .var_safe(
                        makefile,
                        &format!("CMAKE_{language}_LINK_WHAT_YOU_USE_FLAG"),
                    )
                    .is_empty()
                && let Some(checker) = self.ctx.var(makefile, "CMAKE_LINK_WHAT_YOU_USE_CHECK")
            {
                // GetFullPath(config, RuntimeBinaryArtifact, realname).
                let config = self.config.clone();
                let names = gt::names(&self.ctx, target, &config);
                let directory = gt::directory(&self.ctx, target, &config, ArtifactType::Runtime);
                let target_real = self.ninja_path(&format!("{directory}/{}", names.real));
                commands.push(format!(
                    "{} -E __run_co_compile --lwyu={} --source={}",
                    self.cmake_cmd,
                    flags::shell(&checker),
                    self.shell(&target_real),
                ));
            }
            let launcher = rule_launcher(&self.ctx, target, "RULE_LAUNCH_LINK");
            let mut objects = "$in".to_owned();
            let mut libraries = "$LINK_PATH $LINK_LIBRARIES";
            if use_rsp {
                let flag = self
                    .ctx
                    .var(
                        makefile,
                        &format!("CMAKE_{language}_RESPONSE_FILE_LINK_FLAG"),
                    )
                    .unwrap_or_else(|| "@".to_owned());
                if !flag.is_empty() {
                    rule.rspfile = "$RSP_FILE".into();
                    objects = format!("{flag}$RSP_FILE");
                    rule.rspcontent = if self.is_gcc_on_windows() {
                        // ar.exe can't handle backslashes in rsp files
                        // (implicitly used by gcc).
                        "$in".into()
                    } else {
                        "$in_newline".into()
                    };
                    if self
                        .ctx
                        .var(
                            makefile,
                            &format!("CMAKE_{language}_USE_RESPONSE_FILE_FOR_LIBRARIES"),
                        )
                        .is_none_or(|v| util::is_on(&v))
                    {
                        rule.rspcontent.push(' ');
                        rule.rspcontent.push_str(libraries);
                        libraries = "";
                    }
                }
            }
            let mut vars: Vec<(&str, String)> = vec![
                ("TARGET_NAME", target.name.clone()),
                ("TARGET_TYPE", target.kind.type_name().to_owned()),
                ("LANGUAGE", language.to_owned()),
                ("OBJECTS", objects),
                ("LINK_LIBRARIES", libraries.into()),
                ("OBJECT_DIR", "$OBJECT_DIR".into()),
                ("TARGET", "$TARGET_FILE".into()),
                ("SONAME_FLAG", "$SONAME_FLAG".into()),
                ("TARGET_SONAME", "$SONAME".into()),
                ("TARGET_INSTALLNAME_DIR", "$INSTALLNAME_DIR".into()),
                ("TARGET_PDB", "$TARGET_PDB".into()),
                ("FLAGS", "$FLAGS".into()),
                ("LINK_FLAGS", "$LINK_FLAGS".into()),
                ("MANIFESTS", "$MANIFESTS".into()),
                ("AIX_EXPORTS", "$AIX_EXPORTS".into()),
                ("CONFIG", "$CONFIG".into()),
                (
                    "TARGET_LABELS",
                    target
                        .props
                        .get("LABELS")
                        .map(|labels| labels.replace(';', ","))
                        .unwrap_or_default(),
                ),
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
                    flags::expand_rule(&self.ctx, target, &format!("{launcher}{command}"), &vars)
                })
                .filter(|command| command.trim() != ":")
                .collect();
            expanded.insert(0, "$PRE_LINK".into());
            expanded.push("$POST_BUILD".into());
            rule.command = self.build_command_line(&expanded);
            let visible = visible_type_name(target);
            rule.comment = format!("Rule for linking {language} {visible}.");
            let prelink = pre_link_comments.join("; ");
            let postbuild = post_build_comments.join("; ");
            let mut description = String::new();
            if !prelink.is_empty() {
                description.push_str(&prelink);
                description.push_str("; ");
            }
            description.push_str(&format!("Linking {language} {visible} $TARGET_FILE"));
            if !postbuild.is_empty() {
                description.push_str("; ");
                description.push_str(&postbuild);
            }
            rule.description = description;
            rule.restat = "$RESTAT".into();
            self.add_rule(&rule);
        }
        if symlink {
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
        if target.platform.apple && gt::has_import_library(&self.ctx, target, &self.config) {
            let names = gt::names(&self.ctx, target, &self.config);
            let mut rule = Rule {
                name: self.text_stubs_rule_name(target),
                ..Rule::default()
            };
            rule.comment = format!(
                "Rule for generating text-based stubs for {}.",
                visible_type_name(target)
            );
            rule.description = "Creating text-based stubs $out".into();
            let mut command = self
                .ctx
                .var_safe(target.makefile, "CMAKE_CREATE_TEXT_STUBS");
            // vars.Target = "$in"; SetTargetImpLib("$out").
            command = command.replace("<TARGET>", "$in");
            command = command.replace("<TARGET_IMPLIB>", "$out");
            rule.command = self.build_command_line(&[command]);
            self.add_rule(&rule);
            if names.import_output != names.import_real && !target.is_framework_on_apple() {
                self.add_rule(&Rule {
                    name: "CMAKE_SYMLINK_IMPORT_LIBRARY".into(),
                    command: format!(
                        "{} -E cmake_symlink_library $in $SONAME $out",
                        self.cmake_cmd
                    ),
                    description: "Creating import library symlink $out".into(),
                    comment: "Rule for creating import library symlink.".into(),
                    ..Rule::default()
                });
            }
        }
    }

    /// `cmNinjaNormalTargetGenerator::LanguageLinkerDeviceRule`.
    fn device_linker_rule_name(&self, language: &str, target: &Target) -> String {
        format!(
            "{}_{}_DEVICE_LINKER__{}_{}",
            language,
            target.kind.type_name(),
            encode_rule_name(&target.name),
            self.config
        )
    }

    /// `cmNinjaNormalTargetGenerator::WriteNvidiaDeviceLinkRule`.
    fn write_device_link_rule(&mut self, language: &str, target: &Target, use_rsp: bool) {
        let name = self.device_linker_rule_name(language, target);
        if self.has_rule(&name) {
            return;
        }
        let variable = if target.kind == TargetKind::Executable {
            "CMAKE_CUDA_DEVICE_LINK_EXECUTABLE"
        } else {
            "CMAKE_CUDA_DEVICE_LINK_LIBRARY"
        };
        let Some(template) = self.ctx.var(target.makefile, variable) else {
            self.ctx.error(
                target.backtrace.clone(),
                format!("{variable} is not defined"),
            );
            return;
        };
        let mut objects = "$in".to_owned();
        let mut libraries = "$LINK_PATH $LINK_LIBRARIES";
        let mut rspfile = String::new();
        let mut rspcontent = String::new();
        let response_flag = self
            .ctx
            .var_safe(target.makefile, "CMAKE_CUDA_RESPONSE_FILE_DEVICE_LINK_FLAG");
        if use_rsp && !response_flag.is_empty() {
            rspfile = "$RSP_FILE".into();
            objects = format!("{response_flag}$RSP_FILE");
            rspcontent = if self.is_gcc_on_windows() {
                "$in".into()
            } else {
                "$in_newline".into()
            };
            if self
                .ctx
                .var(
                    target.makefile,
                    "CMAKE_CUDA_USE_RESPONSE_FILE_FOR_LIBRARIES",
                )
                .is_none_or(|value| util::is_on(&value))
            {
                rspcontent.push_str(" $LINK_LIBRARIES");
                libraries = "";
            }
        }
        let vars: Vec<(&str, String)> = vec![
            ("TARGET_NAME", target.name.clone()),
            ("TARGET_TYPE", target.kind.type_name().to_owned()),
            (
                "TARGET_LABELS",
                target
                    .props
                    .get("LABELS")
                    .map(|labels| labels.replace(';', ","))
                    .unwrap_or_default(),
            ),
            ("LANGUAGE", "CUDA".into()),
            ("OBJECTS", objects),
            ("LINK_LIBRARIES", libraries.into()),
            ("OBJECT_DIR", "$OBJECT_DIR".into()),
            ("TARGET", "$TARGET_FILE".into()),
            ("SONAME_FLAG", "$SONAME_FLAG".into()),
            ("TARGET_SONAME", "$SONAME".into()),
            ("TARGET_PDB", "$TARGET_PDB".into()),
            ("TARGET_COMPILE_PDB", "$TARGET_COMPILE_PDB".into()),
            ("FLAGS", "$FLAGS".into()),
            ("LINK_FLAGS", "$LINK_FLAGS".into()),
            ("MANIFESTS", "$MANIFESTS".into()),
            ("CONFIG", "$CONFIG".into()),
            ("LANGUAGE_COMPILE_FLAGS", "$LANGUAGE_COMPILE_FLAGS".into()),
        ];
        let launcher = rule_launcher(&self.ctx, target, "RULE_LAUNCH_LINK");
        let expanded: Vec<String> = util::expand_list(&template, false)
            .iter()
            .map(|command| {
                flags::expand_rule(&self.ctx, target, &format!("{launcher}{command}"), &vars)
            })
            .filter(|command| !command.is_empty() && !command.starts_with(':'))
            .collect();
        let command = self.build_command_line(&expanded);
        self.add_rule(&Rule {
            name,
            command,
            rspfile,
            rspcontent,
            comment: format!(
                "Rule for linking {} {}.",
                language,
                visible_type_name(target)
            ),
            description: format!(
                "Linking {} {} $TARGET_FILE",
                language,
                visible_type_name(target)
            ),
            restat: "$RESTAT".into(),
            ..Rule::default()
        });
    }

    /// `cmNinjaNormalTargetGenerator::WriteDeviceLinkRules`: the three rules
    /// used by Clang CUDA device linking (`nvlink`, `fatbinary`, and the
    /// registration-stub compile).
    fn write_clang_device_link_rules(&mut self, language: &str, target: &Target) {
        let makefile = target.makefile;
        let linker_rule = format!(
            "{}_DEVICE_LINK__{}_{}",
            language,
            encode_rule_name(&target.name),
            self.config
        );
        if !self.has_rule(&linker_rule) {
            let linker = self.ctx.var_safe(makefile, "CMAKE_CUDA_DEVICE_LINKER");
            self.add_rule(&Rule {
                name: linker_rule,
                command: format!("{linker} -arch=$ARCH $REGISTER -o=$out $in"),
                comment: "Rule for CUDA device linking.".into(),
                description: "Linking CUDA $out".into(),
                ..Rule::default()
            });
        }

        let compile_rule = format!(
            "{}_DEVICE_LINK_COMPILE__{}_{}",
            language,
            encode_rule_name(&target.name),
            self.config
        );
        if !self.has_rule(&compile_rule)
            && let Some(template) = self.ctx.var(makefile, "CMAKE_CUDA_DEVICE_LINK_COMPILE")
        {
            let compile_flags = flags::cuda_compile_language_flags(&self.ctx, target, &self.config);
            let command = flags::expand_rule(
                &self.ctx,
                target,
                &template,
                &[
                    ("LANGUAGE", "CUDA".into()),
                    ("OBJECT", "$out".into()),
                    ("FATBINARY", "$FATBIN".into()),
                    ("REGISTER_FILE", "$REGISTER".into()),
                    ("LINK_FLAGS", "$LINK_FLAGS".into()),
                    ("FLAGS", compile_flags),
                    ("CONFIG", "$CONFIG".into()),
                ],
            );
            self.add_rule(&Rule {
                name: compile_rule,
                command,
                comment: "Rule for compiling CUDA device stubs.".into(),
                description: "Compiling CUDA device stub $out".into(),
                ..Rule::default()
            });
        }

        let fatbinary_rule = format!(
            "{}_FATBINARY__{}_{}",
            language,
            encode_rule_name(&target.name),
            self.config
        );
        if !self.has_rule(&fatbinary_rule) {
            let fatbinary = self.ctx.var_safe(makefile, "CMAKE_CUDA_FATBINARY");
            self.add_rule(&Rule {
                name: fatbinary_rule,
                command: format!(
                    "{fatbinary} -64 -cmdline=--compile-only -compress-all -link --embedded-fatbin=$out $PROFILES"
                ),
                comment: "Rule for CUDA fatbinaries.".into(),
                description: "Creating fatbinary $out".into(),
                ..Rule::default()
            });
        }
    }

    /// `cmNinjaNormalTargetGenerator::WriteDeviceLinkStatements` for Clang
    /// CUDA: per-architecture cubins, one embedded fatbinary header and the
    /// compiled registration stub.  Returns the final stub object.
    fn write_clang_device_link_statements(
        &mut self,
        target: &'a Target,
        language: &str,
        objects: &[String],
        output: &str,
    ) {
        let config = self.config.clone();
        let architectures = util::expand_list(
            target.props.get("CUDA_ARCHITECTURES").unwrap_or_default(),
            false,
        );
        self.write_clang_device_link_rules(language, target);

        let object_dir_abs = self.object_dir(target);
        let object_dir = self.ninja_path(&object_dir_abs);
        let line = link::device_link_line(&self.ctx, target, &config, language);
        let mut explicit_deps = Vec::new();
        let mut push_dep = |dep: String| {
            if !explicit_deps.contains(&dep) {
                explicit_deps.push(dep);
            }
        };
        for dep in line.depends.iter().map(|dep| self.ninja_path(dep)) {
            push_dep(dep);
        }
        for object in objects {
            push_dep(object.clone());
        }

        let linker_rule = format!(
            "{}_DEVICE_LINK__{}_{}",
            language,
            encode_rule_name(&target.name),
            self.config
        );
        let fatbinary_rule = format!(
            "{}_FATBINARY__{}_{}",
            language,
            encode_rule_name(&target.name),
            self.config
        );
        let compile_rule = format!(
            "{}_DEVICE_LINK_COMPILE__{}_{}",
            language,
            encode_rule_name(&target.name),
            self.config
        );
        let fatbinary_output = format!("{object_dir}/cmake_cuda_fatbin.h");
        let mut fatbinary = Build::new(&fatbinary_rule);
        fatbinary.outputs.push(fatbinary_output.clone());
        let mut profiles = String::new();
        for architecture_kind in architectures {
            let architecture = architecture_kind
                .split('-')
                .next()
                .unwrap_or(&architecture_kind);
            let cubin = format!("{object_dir}/sm_{architecture}.cubin");
            let mut dlink = Build::new(&linker_rule);
            dlink.explicit_deps = explicit_deps.clone();
            dlink.outputs.push(cubin.clone());
            dlink
                .variables
                .insert("ARCH".into(), format!("sm_{architecture}"));
            if fatbinary.explicit_deps.is_empty() {
                dlink.variables.insert(
                    "REGISTER".into(),
                    format!(
                        "--register-link-binaries={}/cmake_cuda_register.h",
                        object_dir
                    ),
                );
            }
            profiles.push_str(&format!(" -im=profile=sm_{architecture},file={cubin}"));
            fatbinary.explicit_deps.push(cubin);
            write_build(self.out_at(&Stream::Common), &dlink, 0);
        }
        fatbinary
            .variables
            .insert("PROFILES".into(), profiles.trim_start().to_owned());
        write_build(self.out_at(&Stream::Common), &fatbinary, 0);

        let mut compile = Build::new(&compile_rule);
        compile.outputs.push(output.to_owned());
        compile.explicit_deps.push(fatbinary_output);
        compile.variables.insert(
            "FATBIN".into(),
            self.shell(&format!("{object_dir_abs}/cmake_cuda_fatbin.h")),
        );
        compile.variables.insert(
            "REGISTER".into(),
            self.shell(&format!("{object_dir_abs}/cmake_cuda_register.h")),
        );
        compile.variables.insert(
            "LINK_FLAGS".into(),
            flags::device_link_flag_vars(&self.ctx, target, &config, false).link_flags,
        );
        write_build(self.out_at(&Stream::Common), &compile, 0);
    }

    /// `cmNinjaNormalTargetGenerator::WriteDeviceLinkStatement` and
    /// `WriteNvidiaDeviceLinkStatement` for the NVIDIA driver device-link
    /// mode.  Returns the `cmake_device_link` object when one is needed.
    fn write_device_link_statement(
        &mut self,
        target: &'a Target,
        language: &str,
        objects: &[String],
    ) -> Option<String> {
        let config = self.config.clone();
        if !link::requires_device_linking(&self.ctx, target, &config, language) {
            return None;
        }
        let makefile = target.makefile;
        let object_extension = self.ctx.var_safe(makefile, "CMAKE_CUDA_OUTPUT_EXTENSION");
        let object_dir = self.ninja_path(&self.object_dir(target));
        let output = self.ninja_path(&format!(
            "{}/cmake_device_link{object_extension}",
            self.object_dir(target)
        ));
        let clang_cuda = self
            .ctx
            .var(target.makefile, "CMAKE_CUDA_COMPILER_ID")
            .as_deref()
            == Some("Clang");
        if clang_cuda {
            let architectures = target.props.get("CUDA_ARCHITECTURES").unwrap_or_default();
            if util::is_off(architectures) {
                self.ctx.error(
                    target.backtrace.clone(),
                    "CUDA_SEPARABLE_COMPILATION on Clang requires CUDA_ARCHITECTURES to be set.",
                );
                return None;
            }
        }

        let mut out = String::new();
        write_divider(&mut out);
        let _ = write!(
            out,
            "# Device Link build statements for {} target {}\n\n",
            target.kind.type_name(),
            target.name
        );
        if clang_cuda {
            self.out_at(&Stream::Common).push_str(&out);
            self.write_clang_device_link_statements(target, language, objects, &output);
            return Some(output);
        }

        let mut build = Build::new(&self.device_linker_rule_name(language, target));
        build.comment = format!("Link the {} {}", visible_type_name(target), output);
        build.outputs.push(output.clone());
        build.explicit_deps = objects.to_vec();

        let dependency_ipo = link::link_information(&self.ctx, target, &config, language)
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
                                && flags::ipo_enabled(&self.ctx, dependency, &config, "CUDA")
                        })
                })
            })
            .unwrap_or(false);
        let device_flags = flags::device_link_flag_vars(&self.ctx, target, &config, dependency_ipo);
        let line = link::device_link_line(&self.ctx, target, &config, language);
        let mut device_libraries = line.libraries;
        let mut device_link_path = line.framework_path.clone() + &line.link_path;
        if self.is_gcc_on_windows() {
            device_libraries = device_libraries.replace('\\', "/");
            device_link_path = device_link_path.replace('\\', "/");
        }
        build
            .variables
            .insert("LINK_LIBRARIES".into(), device_libraries);
        build
            .variables
            .insert("LINK_FLAGS".into(), device_flags.link_flags);
        build.variables.insert("LINK_PATH".into(), device_link_path);
        build.variables.insert(
            "LANGUAGE_COMPILE_FLAGS".into(),
            device_flags.language_compile_flags,
        );
        build.implicit_deps = line
            .depends
            .iter()
            .map(|dep| self.ninja_path(dep))
            .collect();
        if let Some(mdi) = gt::module_definition_info(&self.ctx, target, &config) {
            for source in &mdi.sources {
                build.implicit_deps.push(self.ninja_path(source));
            }
        }
        for source in gt::sources(&self.ctx, target, &config).iter() {
            if source.kind == SourceKind::Manifest {
                build.implicit_deps.push(self.ninja_path(&source.path));
            }
        }
        build
            .implicit_deps
            .extend(line.link_depends.iter().map(|dep| self.ninja_path(dep)));

        build
            .variables
            .insert("TARGET_FILE".into(), self.shell(&output));
        let manifest_flag = self
            .ctx
            .var_safe(makefile, &format!("CMAKE_{language}_LINKER_MANIFEST_FLAG"));
        let manifests: Vec<String> = gt::sources(&self.ctx, target, &config)
            .iter()
            .filter(|source| source.kind == SourceKind::Manifest)
            .map(|source| {
                format!(
                    "{manifest_flag}{}",
                    self.shell(&self.ninja_path(&source.path))
                )
            })
            .collect();
        build
            .variables
            .insert("MANIFESTS".into(), manifests.join(" "));
        let names = gt::names(&self.ctx, target, &config);
        if (matches!(
            target.kind,
            TargetKind::SharedLibrary | TargetKind::ModuleLibrary
        ) && gt::has_soname(&self.ctx, target, &config))
            || target.is_archived_aix_shared_library()
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
                .insert("SONAME".into(), self.shell(&names.shared_object));
            if target.kind == TargetKind::SharedLibrary {
                let install_dir =
                    install_name::install_name_dir_for_build_tree(&self.ctx, target, &config);
                if !install_dir.is_empty() {
                    build
                        .variables
                        .insert("INSTALLNAME_DIR".into(), self.shell(&install_dir));
                }
            }
        }
        if !names.import_library.is_empty() {
            build.variables.insert(
                "TARGET_IMPLIB".into(),
                self.shell(&self.ninja_path(&gt::full_path(
                    &self.ctx,
                    target,
                    &config,
                    ArtifactType::Import,
                    false,
                ))),
            );
        }
        build
            .variables
            .insert("OBJECT_DIR".into(), self.shell(&object_dir));
        self.ensure_directory_exists(&self.object_dir(target));
        if !self.set_msvc_target_pdb_variable(target, &mut build.variables) {
            let parts = gt::full_name_components(&self.ctx, target, &config, ArtifactType::Runtime);
            let suffix = self
                .ctx
                .var(makefile, "CMAKE_DEBUG_SYMBOL_SUFFIX")
                .unwrap_or_else(|| ".dbg".to_owned());
            build.variables.insert(
                "TARGET_PDB".into(),
                format!("{}{}{suffix}", parts.base, parts.suffix),
            );
        }
        if let Some(pool) = target.props.get("JOB_POOL_LINK") {
            build.variables.insert("pool".into(), pool.to_owned());
        }
        let mut order = Vec::new();
        self.append_target_depends(target, &mut order, DependOn::TargetArtifact);
        build.order_only_deps = order;
        build.rsp_file = self.ninja_path(&format!(
            "CMakeFiles/{}{}.rsp",
            target.name,
            if self.ctx.multi_config {
                format!(".{config}")
            } else {
                String::new()
            }
        ));
        let limit = command_line_length_limit() - self.rule_command_length(&build.rule);
        let use_rsp = write_build(&mut out, &build, limit);
        self.out_at(&Stream::Common).push_str(&out);
        self.write_device_link_rule(language, target, use_rsp);
        Some(output)
    }

    /// `cmNinjaNormalTargetGenerator::WriteLinkStatement`.
    fn write_link_statement(
        &mut self,
        target: &'a Target,
        language: &str,
        objects: &[String],
        first_for_config: bool,
    ) {
        let config = self.config.clone();
        let file_config = self.file_config.clone();
        let makefile = target.makefile;
        let names = gt::names(&self.ctx, target, &config);
        let directory = gt::directory(&self.ctx, target, &config, ArtifactType::Runtime);
        let mut target_output = self.ninja_path(&format!("{directory}/{}", names.output));
        let mut target_real = self.ninja_path(&format!("{directory}/{}", names.real));
        let target_output_implib = self.ninja_path(&gt::full_path(
            &self.ctx,
            target,
            &config,
            ArtifactType::Import,
            false,
        ));
        if config != file_config {
            // A cross-config link whose outputs coincide with the file
            // config's own (no per-config output directory) is skipped.
            let file_ctx = self.ctx.for_config(&file_config);
            let file_output = self.ninja_path(&gt::full_path(
                &file_ctx,
                target,
                &file_config,
                ArtifactType::Runtime,
                false,
            ));
            let file_real = self.ninja_path(&gt::full_path(
                &file_ctx,
                target,
                &file_config,
                ArtifactType::Runtime,
                true,
            ));
            let file_implib = self.ninja_path(&gt::full_path(
                &file_ctx,
                target,
                &file_config,
                ArtifactType::Import,
                false,
            ));
            if target_output == file_output
                || target_real == file_real
                || (!gt::full_name(&self.ctx, target, &config, ArtifactType::Import).is_empty()
                    && !gt::full_name(&file_ctx, target, &file_config, ArtifactType::Import)
                        .is_empty()
                    && target_output_implib == file_implib)
            {
                return;
            }
        }

        // cmOSXBundleGenerator: create the bundle layout at generate time.
        if target.is_app_bundle_on_apple() {
            let outpath = osx_bundle::create_app_bundle(
                &self.ctx,
                target,
                &names.output,
                &directory,
                &config,
            );
            target_output = self.ninja_path(&format!("{outpath}/{}", names.output));
            target_real = self.ninja_path(&format!("{outpath}/{}", names.real));
        } else if target.is_framework_on_apple() {
            let mut skip = SkipParts::default();
            if self.ctx.multi_config {
                // Skip creating Info.plist when there are multiple
                // configurations and this one has a postfix; the
                // non-postfix configuration's plist serves all.
                if !gt::postfix(&self.ctx, target, &config).is_empty() {
                    skip.info_plist = true;
                }
            }
            if gt::has_import_library(&self.ctx, target, &config) {
                skip.text_stubs = false;
            }
            let mut folders = std::mem::take(&mut self.mac_content_folders);
            osx_bundle::create_framework(
                &self.ctx,
                target,
                &names.output,
                &directory,
                &config,
                skip,
                &mut folders,
            );
            self.mac_content_folders = folders;
        } else if target.is_cf_bundle_on_apple() {
            osx_bundle::create_cf_bundle(&self.ctx, target, &names.output, &directory, &config);
        }

        let device_link_object = self.write_device_link_statement(target, language, objects);

        let rule_name = self.linker_rule_name(language, target);
        let link_depfile = flags::link_dependency_file(&self.ctx, target, language);
        let has_depfile = link_depfile.is_some();
        let mut out = String::new();
        write_divider(&mut out);
        let _ = write!(
            out,
            "# Link build statements for {} target {}\n\n",
            target.kind.type_name(),
            target.name
        );
        let mut build = Build::new(&rule_name);
        let visible = visible_type_name(target);
        build.comment = format!("Link the {visible} {}", target_real);
        build.outputs.push(target_real.clone());
        // GetByproductsForCleanTarget(config) (firstForConfig: always,
        // without cross-config builds).
        let mut clean_outputs = Vec::new();
        build.explicit_deps = objects.to_vec();
        if let Some(device_link_object) = device_link_object {
            if first_for_config {
                clean_outputs.push(device_link_object.clone());
            }
            build.explicit_deps.push(device_link_object);
        }
        clean_outputs.push(target_real.clone());
        // Static libraries only need the diagnostics of their link items.
        let _ = link::link_information(&self.ctx, target, &config, language);
        let link_vars = flags::link_flag_vars(&self.ctx, target, &config, language);
        let mut shared_symlinks: Vec<String> = Vec::new();
        let object_dir = self.ninja_path(&self.object_dir(target));
        let mut link_flags = link_vars.link_flags;
        if target.kind != TargetKind::StaticLibrary {
            let line = link::link_line(&self.ctx, target, &config, language);
            build
                .variables
                .insert("LINK_LIBRARIES".into(), line.libraries);
            build
                .variables
                .insert("LINK_PATH".into(), line.framework_path + &line.link_path);
            // cmNinjaTargetGenerator::ComputeLinkDeps: the link information,
            // module definition files, user-specified manifest sources and
            // LINK_DEPENDS.
            build.implicit_deps = line
                .depends
                .iter()
                .map(|dep| self.ninja_path(dep))
                .collect();
            // cmNinjaTargetGenerator::ComputeLinkDeps: the module
            // definition sources and user-specified manifest sources.
            if let Some(mdi) = gt::module_definition_info(&self.ctx, target, &config) {
                for source in &mdi.sources {
                    build.implicit_deps.push(self.ninja_path(source));
                }
            }
            for source in gt::sources(&self.ctx, target, &config).iter() {
                if source.kind == SourceKind::Manifest {
                    build.implicit_deps.push(self.ninja_path(&source.path));
                }
            }
            build
                .implicit_deps
                .extend(line.link_depends.iter().map(|dep| self.ninja_path(dep)));
            shared_symlinks = line
                .shared_symlinks
                .iter()
                .map(|dep| self.ninja_path(dep))
                .collect();
        }
        // cmCommonTargetGenerator::AppendOSXVerFlag: MACHO version flags.
        if matches!(
            target.kind,
            TargetKind::SharedLibrary | TargetKind::ModuleLibrary
        ) {
            self.append_osx_ver_flag(&mut link_flags, language, "COMPATIBILITY", true, target);
            self.append_osx_ver_flag(&mut link_flags, language, "CURRENT", false, target);
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
        build.variables.insert("LINK_FLAGS".into(), link_flags);
        // cmCommonTargetGenerator::GetManifests + GetAIXExports.
        let manifest_flag = self
            .ctx
            .var_safe(makefile, &format!("CMAKE_{language}_LINKER_MANIFEST_FLAG"));
        let manifests: Vec<String> = gt::sources(&self.ctx, target, &config)
            .iter()
            .filter(|source| source.kind == SourceKind::Manifest)
            .map(|source| {
                format!(
                    "{manifest_flag}{}",
                    self.shell(&self.ninja_path(&source.path))
                )
            })
            .collect();
        build
            .variables
            .insert("MANIFESTS".into(), manifests.join(" "));
        if target.platform.aix
            && target
                .props
                .get("AIX_EXPORT_ALL_SYMBOLS")
                .is_some_and(|value| !util::is_on(value))
        {
            build.variables.insert("AIX_EXPORTS".into(), "-n".into());
        }
        build.variables.insert("CONFIG".into(), config.clone());
        // cmNinjaTargetGenerator::addPoolNinjaVariable("JOB_POOL_LINK").
        if let Some(pool) = target.props.get("JOB_POOL_LINK") {
            build.variables.insert("pool".into(), pool.to_owned());
        }
        if (matches!(
            target.kind,
            TargetKind::SharedLibrary | TargetKind::ModuleLibrary
        ) && gt::has_soname(&self.ctx, target, &config))
            || target.is_archived_aix_shared_library()
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
                .insert("SONAME".into(), self.shell(&names.shared_object));
            if target.kind == TargetKind::SharedLibrary {
                let install_dir =
                    install_name::install_name_dir_for_build_tree(&self.ctx, target, &config);
                if !install_dir.is_empty() {
                    build
                        .variables
                        .insert("INSTALLNAME_DIR".into(), self.shell(&install_dir));
                }
            }
        }
        let mut byproducts = CcOutputs::default();
        // TARGET_IMPLIB: the import library exists on non-Apple platforms
        // (on Apple it is a text-based stub generated by its own rule).
        if !target.platform.apple && !names.import_library.is_empty() {
            build
                .variables
                .insert("TARGET_IMPLIB".into(), self.shell(&target_output_implib));
            self.ensure_parent_directory_exists(&target_output_implib);
            if gt::has_import_library(&self.ctx, target, &config) {
                // Some linkers may update a binary without touching its
                // import library.
                byproducts.explicit.push(target_output_implib.clone());
                clean_outputs.push(target_output_implib.clone());
            }
        }
        if !self.set_msvc_target_pdb_variable(target, &mut build.variables) {
            // It is common to place debug symbols at a specific place, so a
            // plain target name is made available to the rule.
            let parts = gt::full_name_components(&self.ctx, target, &config, ArtifactType::Runtime);
            let suffix = self
                .ctx
                .var(makefile, "CMAKE_DEBUG_SYMBOL_SUFFIX")
                .unwrap_or_else(|| ".dbg".to_owned());
            build.variables.insert(
                "TARGET_PDB".into(),
                format!("{}{}{suffix}", parts.base, parts.suffix),
            );
        }
        build
            .variables
            .insert("OBJECT_DIR".into(), self.shell(&object_dir));
        self.ensure_directory_exists(&self.object_dir(target));
        build
            .variables
            .insert("TARGET_FILE".into(), self.shell(&target_real));
        // GCC-on-Windows linkers cannot consume backslashes in the response
        // file or library paths.
        if self.is_gcc_on_windows() {
            for name in ["LINK_LIBRARIES", "LINK_PATH"] {
                if let Some(value) = build.variables.get_mut(name) {
                    *value = value.replace('\\', "/");
                }
            }
        }
        // Build-event commands: PRE_BUILD and PRE_LINK run before the link,
        // POST_BUILD after it.
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
                if config != file_config
                    && !crate::commands::custom_actions::has_unique_byproducts(
                        self.ctx.interp,
                        command.makefile,
                        &command.byproducts,
                        &command.backtrace,
                    )
                {
                    continue;
                }
                // cmCustomCommandGenerator(cc, fileConfig, lg, true, config).
                let evaluated = custom::evaluate_spec_cfg(
                    &self.ctx,
                    &custom::spec_from_command(&self.ctx, command),
                    &config,
                    &file_config,
                );
                let lines = self.custom_command_lines(&evaluated, makefile);
                if stage == CustomCommandKind::PostBuild {
                    post_build.extend(lines);
                } else {
                    pre_link.extend(lines);
                }
                self.add_cc_outputs(&mut byproducts, &evaluated.byproducts);
                let cleaned: Vec<String> = evaluated
                    .byproducts
                    .iter()
                    .map(|path| self.ninja_path(path))
                    .collect();
                self.byproducts_for_clean.extend(cleaned);
            }
        }
        // After PRE_LINK commands, go back to CMAKE_BINARY_DIR for the link.
        if !pre_link.is_empty() {
            pre_link.push(format!("cd {}", self.shell(&self.home_binary)));
        }
        // Maybe create a .def file from the list of objects
        // (GetModuleDefinitionInfo with DefFileGenerated).
        if let Some(mdi) = gt::module_definition_info(&self.ctx, target, &config)
            && mdi.def_file_generated
        {
            let obj_list_file = format!("{}.objs", mdi.def_file);
            let mut command = format!(
                "{} -E __create_def {} {}",
                self.cmake_cmd,
                self.shell(&mdi.def_file),
                self.shell(&obj_list_file)
            );
            let nm = self.ctx.var_safe(makefile, "CMAKE_NM");
            if !nm.is_empty() {
                command.push_str(&format!(" --nm={}", flags::shell(&nm)));
            }
            pre_link.push(command);
            // The list of object files and definition sources that
            // __create_def reads.
            let mut list = String::new();
            if mdi.windows_export_all_symbols {
                for object in objects {
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
            let _ = crate::fsutil::write_generated(std::path::Path::new(&obj_list_file), &list);
        }
        build.variables.insert(
            "PRE_LINK".into(),
            self.build_command_line_step(
                &pre_link,
                "pre-link",
                Some(target),
                makefile,
                &config,
                &file_config,
            ),
        );
        let post_build_line = self.build_command_line_step(
            &post_build,
            "post-build",
            Some(target),
            makefile,
            &config,
            &file_config,
        );
        let symlink_needed = names.output != names.real
            && !target.is_framework_on_apple()
            && !target.is_archived_aix_shared_library();
        build.variables.insert(
            "POST_BUILD".into(),
            if symlink_needed {
                SHELL_NOOP.to_owned()
            } else {
                post_build_line.clone()
            },
        );
        // Order-only deps: other targets' outputs.
        let mut order = Vec::new();
        self.append_target_depends(target, &mut order, DependOn::TargetArtifact);
        // Order-only dependencies on versioning symlinks of shared libs we
        // link. If our target is not producing a runtime binary, it does
        // not need the symlinks.
        if !target.platform.dll && gt::is_runtime_binary(target) {
            for lib in shared_symlinks {
                if !build.implicit_deps.contains(&lib) {
                    order.push(lib);
                }
            }
        }
        build.order_only_deps = order;
        // Ninja should restat after linking if and only if there are byproducts.
        if !byproducts.explicit.is_empty() {
            build.variables.insert("RESTAT".into(), "1".into());
        }
        build.outputs.extend(byproducts.explicit);
        build.workdir_outs = byproducts.workdir;
        let suffix = if self.ctx.multi_config {
            format!(".{config}")
        } else {
            String::new()
        };
        build.rsp_file = self.ninja_path(&format!("CMakeFiles/{}{suffix}.rsp", target.name));
        let supports_response = language != "RC"
            && (language != "CUDA"
                || self
                    .ctx
                    .var(target.makefile, "CMAKE_CUDA_RESPONSE_FILE_LINK_FLAG")
                    .is_some());
        let limit = if supports_response && self.force_response_file(target) {
            -1
        } else {
            command_line_length_limit() - self.rule_command_length(&build.rule)
        };
        let use_rsp = write_build(&mut out, &build, limit);
        self.write_link_rules(
            target,
            language,
            has_depfile,
            names.output != names.real && !target.is_framework_on_apple(),
            use_rsp,
        );
        // Symlinks.
        if symlink_needed {
            let mut symlink = Build::new(if target.kind == TargetKind::Executable {
                "CMAKE_SYMLINK_EXECUTABLE"
            } else {
                "CMAKE_SYMLINK_LIBRARY"
            });
            if target.kind == TargetKind::Executable {
                symlink.comment = format!("Create executable symlink {target_output}");
                symlink.outputs.push(target_output.clone());
                clean_outputs.push(target_output.clone());
            } else {
                symlink.comment = format!("Create library symlink {target_output}");
                let so = self.ninja_path(&format!("{directory}/{}", names.shared_object));
                if target_real == so || target_output == so {
                    symlink.variables.insert("SONAME".into(), self.shell(&so));
                } else {
                    symlink.outputs.push(so.clone());
                    clean_outputs.push(so.clone());
                }
                symlink.outputs.push(target_output.clone());
                clean_outputs.push(target_output.clone());
            }
            symlink.explicit_deps.push(target_real.clone());
            symlink
                .variables
                .insert("POST_BUILD".into(), post_build_line);
            write_build(&mut out, &symlink, 0);
        }
        // On Apple the import library is a text-based stub generated from
        // the linked binary.
        if target.platform.apple && gt::has_import_library(&self.ctx, target, &config) {
            let dir_tbd = gt::directory(&self.ctx, target, &config, ArtifactType::Import);
            let target_tbd = self.ninja_path(&format!("{dir_tbd}/{}", names.import_real));
            self.ensure_parent_directory_exists(&target_tbd);
            let mut build = Build::new(&self.text_stubs_rule_name(target));
            build.comment = format!("Generate the text-based stubs file {target_tbd}");
            build.outputs.push(target_tbd.clone());
            build.explicit_deps.push(target_real.clone());
            write_build(&mut out, &build, 0);
            if names.import_output != names.import_real && !target.is_framework_on_apple() {
                let output_tbd = self.ninja_path(&format!("{dir_tbd}/{}", names.import_output));
                let soname_tbd = self.ninja_path(&format!("{dir_tbd}/{}", names.import_library));
                let mut build = Build::new("CMAKE_SYMLINK_IMPORT_LIBRARY");
                build.comment = format!("Create import library symlink {output_tbd}");
                if target_tbd == soname_tbd || output_tbd == soname_tbd {
                    build
                        .variables
                        .insert("SONAME".into(), self.shell(&soname_tbd));
                } else {
                    build.outputs.push(soname_tbd.clone());
                    clean_outputs.push(soname_tbd);
                }
                build.outputs.push(output_tbd.clone());
                clean_outputs.push(output_tbd);
                build.explicit_deps.push(target_tbd);
                write_build(&mut out, &build, 0);
            }
        }
        self.out().push_str(&out);
        if first_for_config {
            self.config_byproducts_for_clean
                .entry(config.clone())
                .or_default()
                .extend(clean_outputs);
        }
        // Add aliases for the file name and the target name.
        self.add_target_alias(&names.output.clone(), target);
        self.add_target_alias(&target.name.clone(), target);
        // Add an alias for the import file name (text-based stubs on
        // Apple, import library elsewhere).
        if target.platform.apple && gt::has_import_library(&self.ctx, target, &config) {
            self.add_target_alias(&names.import_output.clone(), target);
        }
    }

    /// `cmCommonTargetGenerator::AppendOSXVerFlag`: append the
    /// `CMAKE_<lang>_OSX_<name>_VERSION_FLAG` + MACHO version flag when a
    /// non-zero version is specified.
    fn append_osx_ver_flag(
        &self,
        flags_text: &mut String,
        language: &str,
        name: &str,
        so: bool,
        target: &Target,
    ) {
        let flag = self.ctx.var(
            target.makefile,
            &format!("CMAKE_{language}_OSX_{name}_VERSION_FLAG"),
        );
        let Some(flag) = flag.filter(|flag| !flag.is_empty()) else {
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

    /// `cmNinjaNormalTargetGenerator::TextStubsGeneratorRule`.
    fn text_stubs_rule_name(&self, target: &Target) -> String {
        format!(
            "TEXT_STUBS_GENERATOR__{}_{}",
            encode_rule_name(&target.name),
            self.config
        )
    }

    /// `cmNinjaTargetGenerator::ExportObjectCompileCommand` +
    /// `cmGlobalNinjaGenerator::AddCXXCompileCommand`.
    /// cmCommonTargetGenerator::GenerateCodeCheckRules.
    fn code_check(&mut self, target: &Target, source: &gt::SourceInfo, object_dir: &str) -> String {
        let lang = &source.language;
        let mut options = Vec::new();
        let mut needs_source = false;
        let driver_mode = |tool| {
            self.ctx
                .var(target.makefile, &format!("CMAKE_{lang}_{tool}_DRIVER_MODE"))
                .filter(|mode| !mode.is_empty())
                .unwrap_or_else(|| if lang == "C" { "gcc" } else { "g++" }.into())
        };
        for (property, option) in [
            ("INCLUDE_WHAT_YOU_USE", "iwyu"),
            ("CLANG_TIDY", "tidy"),
            ("CPPLINT", "cpplint"),
            ("CPPCHECK", "cppcheck"),
            ("ICSTAT", "icstat"),
        ] {
            if property != "CLANG_TIDY" && !matches!(lang.as_str(), "C" | "CXX") {
                continue;
            }
            let Some(raw) = target.props.get(&format!("{lang}_{property}")) else {
                continue;
            };
            let mut command = self.ctx.genex(raw, Some(target), Some(lang));
            if command.is_empty() {
                continue;
            }
            match option {
                "iwyu" if !command.contains("--driver-mode=") => {
                    let _ = write!(command, ";--driver-mode={}", driver_mode(property));
                }
                "tidy" => {
                    let _ = write!(
                        command,
                        ";--extra-arg-before=--driver-mode={}",
                        driver_mode(property)
                    );
                    if let Some(dir) = target
                        .props
                        .get(&format!("{lang}_CLANG_TIDY_EXPORT_FIXES_DIR"))
                        .filter(|dir| !dir.is_empty())
                    {
                        let binary = &self.ctx.interp.makefiles[target.makefile].binary_dir;
                        let dir = util::collapse_full_path(dir, binary);
                        let safe = object_dir
                            .trim_start_matches('/')
                            .replace([':', ' '], "_")
                            .replace("../", "__/");
                        let fixes = format!("{dir}/{safe}/{}.yaml", source.object_name);
                        let _ = std::fs::create_dir_all(util::filename_path(&fixes));
                        let _ = write!(command, ";--export-fixes={}", self.ninja_path(&fixes));
                        self.tidy_fixes_dirs.insert(dir);
                        self.tidy_fixes_files.insert(fixes);
                    }
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
            options.push(format!(" --{option}={}", flags::shell(&command)));
        }
        if options.is_empty() {
            return String::new();
        }
        let mut command = format!("{} -E __run_co_compile", self.cmake_cmd);
        let launcher = flags::compiler_launcher_list(&self.ctx, target, lang);
        if !launcher.is_empty() {
            let _ = write!(command, " --launcher={}", flags::shell(&launcher));
        }
        command.extend(options);
        if needs_source {
            let _ = write!(command, " --source={}", self.shell(&source.path));
        }
        command.push_str(" -- ");
        command
    }

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
            (
                "CUDA_COMPILE_MODE",
                if language == "CUDA" {
                    self.cuda_compile_mode(target)
                } else {
                    String::new()
                },
            ),
        ];
        let template = self
            .ctx
            .var_safe(target.makefile, &format!("CMAKE_{language}_COMPILE_OBJECT"));
        // No launcher for CMAKE_EXPORT_COMPILE_COMMANDS.
        let commands: Vec<String> = util::expand_list(&template, false)
            .iter()
            .map(|command| flags::expand_rule(&self.ctx, target, command, &rule_vars))
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
        let interp = self.ctx.interp;
        let mut seen = HashSet::new();
        let sources = self
            .ctx
            .configs
            .iter()
            .flat_map(|config| {
                gt::sources(&self.ctx, target, config)
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
        )
    }

    /// The shared body of `WriteSummary(target)`.  A default global target
    /// (`edit_cache`, `install`, ...) has no LABELS property and no sources,
    /// but still gets the files when its directory has labels.
    fn write_labels_files(
        &self,
        name: &str,
        target_labels: Option<&str>,
        makefile: usize,
        sources: Vec<(String, Option<String>)>,
        directory: &str,
    ) -> std::io::Result<()> {
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
            return Ok(());
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
            let mut entry = std::collections::BTreeMap::new();
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
        let mut target_json = std::collections::BTreeMap::new();
        target_json.insert(b"labels".to_vec(), Json::Array(all_labels));
        target_json.insert(b"name".to_vec(), text(name));
        let mut root = std::collections::BTreeMap::new();
        root.insert(b"sources".to_vec(), Json::Array(source_json));
        root.insert(b"target".to_vec(), Json::Object(target_json));
        let _ = std::fs::create_dir_all(directory);
        // cmGlobalGenerator::WriteSummary: plain cmGeneratedFileStreams.
        crate::fsutil::write_generated(std::path::Path::new(&file), &out)?;
        crate::fsutil::write_generated(
            std::path::Path::new(&json_file),
            &write_json_ostream(&Json::Object(root)),
        )
    }

    /// `cmNinjaUtilityTargetGenerator::WriteUtilBuildStatements`.
    fn generate_utility_target(&mut self, target: &'a Target) {
        let config = self.config.clone();
        let saved_stream = self.stream.clone();
        let saved_file_config = self.file_config.clone();
        for file_config in self.file_configs_for(&config) {
            self.file_config = file_config.clone();
            self.stream = Stream::Impl(file_config);
            self.write_util_build_statements(target);
        }
        self.file_config = saved_file_config;
        self.stream = saved_stream;
    }

    /// `cmNinjaUtilityTargetGenerator::WriteUtilBuildStatements(config,
    /// fileConfig)`.
    fn write_util_build_statements(&mut self, target: &'a Target) {
        let config = self.config.clone();
        let file_config = self.file_config.clone();
        let makefile = target.makefile;
        let binary_dir = self.ctx.interp.makefiles[makefile].binary_dir.clone();
        let config_dir = self.ctx.config_directory(&file_config);
        let util_command_name = self.ninja_path(&format!(
            "{binary_dir}/CMakeFiles{config_dir}/{}.util",
            target.name
        ));
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
                // cmCustomCommandGenerator ccg(ci, fileConfig, lg).
                let evaluated = custom::evaluate_spec_cfg(
                    &self.ctx,
                    &custom::spec_from_command(&self.ctx, command),
                    &file_config,
                    &file_config,
                );
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
        for source in gt::sources(&self.ctx, target, &config).iter() {
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
        let mut cleaned = Vec::new();
        self.append_target_outputs(target, &mut cleaned, DependOn::TargetArtifact);
        self.byproducts_for_clean.extend(cleaned);
        self.byproducts_for_clean
            .extend(util_outputs.explicit.iter().cloned());
        self.append_target_depends(target, &mut deps, DependOn::TargetArtifact);
        if commands.is_empty() {
            phony.comment = format!("Utility command for {}", target.name);
            phony.explicit_deps = deps;
            self.write_build(&phony);
        } else {
            let mut command = self.build_command_line_step(
                &commands,
                "utility",
                Some(target),
                makefile,
                &config,
                &file_config,
            );
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
            if self.ctx.multi_config {
                // cmGlobalNinjaMultiGenerator::ExpandCFGIntDir.
                command = command.replace("${CONFIGURATION}", &config);
            }
            if config == file_config
                || self
                    .ctx
                    .interp
                    .ext
                    .per_config_utility_targets
                    .contains(&target.name)
            {
                self.write_custom_command_build(
                    &command,
                    &desc,
                    &format!("Utility command for {}", target.name),
                    "",
                    "",
                    uses_terminal,
                    true,
                    &config,
                    util_outputs,
                    deps,
                    Vec::new(),
                );
            }
            phony.explicit_deps.push(util_command_name);
            self.write_build(&phony);
        }
        // Find ADDITIONAL_CLEAN_FILES.
        self.add_additional_clean_files(makefile, Some(target));
        self.add_target_alias(&target.name.clone(), target);
    }

    /// `cmLocalNinjaGenerator::WriteCustomCommandBuildStatements`.
    fn write_custom_command_build_statements(&mut self, commands: &[(CcRef, Vec<&'a Target>)]) {
        for (cc, targets) in commands {
            self.write_custom_command_build_statement(*cc, targets);
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
        custom::evaluate_spec(&self.ctx, &custom::spec_from_command(&self.ctx, command))
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
            let program = outconv::escape_for_shell(
                &argv[0],
                outconv::NINJA_UNIX
                    | outconv::FLAG_ALLOW_MAKE_VARIABLES
                    | flags::unescape_configuration(),
            );
            let mut line = format!("{launcher}{program}");
            evaluated.append_arguments(&self.ctx, index, &mut line);
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
                    custom::maybe_relative_to(&self.ctx, makefile, binary_dir, output)
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
            .replace("<TARGET_NAME>", "TARGET_NAME")
            .replace("<CMAKE_CURRENT_BINARY_DIR>", &self.shell(binary_dir))
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
        makefile: usize,
        output_config: &str,
        command_config: &str,
    ) -> String {
        if lines.is_empty() {
            return SHELL_NOOP.to_owned();
        }
        if !custom_step.is_empty() {
            let total: usize = lines.iter().map(|line| line.len() + 6).sum();
            if total as i64 > command_line_length_limit() / 2 {
                // cmLocalNinjaGenerator::WriteCommandScript.
                let mut script = match target {
                    Some(target) => gt::support_directory(&self.ctx, target),
                    None => format!(
                        "{}/CMakeFiles",
                        self.ctx.interp.makefiles[makefile].binary_dir
                    ),
                };
                script.push_str(&self.ctx.config_directory(output_config));
                let _ = std::fs::create_dir_all(&script);
                script.push('/');
                script.push_str(custom_step);
                if self.ctx.multi_config {
                    script.push('-');
                    script.push_str(command_config);
                }
                script.push_str(".sh");
                let mut content = String::from("set -e\n\n");
                for line in lines {
                    content.push_str(&line.replace("$$", "$"));
                    content.push('\n');
                }
                let _ = std::fs::write(&script, &*crate::bytes::encode(&content));
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
                let abs = self.ninja_abs_path(path);
                self.custom_command_outputs.insert(abs.clone());
                self.assumed_source_dependencies.remove(&abs);
            }
            self.custom_command_outputs.insert(out.clone());
            // SeenCustomCommandOutput: a known producer retires the assumed
            // dependencies recorded for the same path.
            self.assumed_source_dependencies.remove(&out);
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
        config: &str,
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
        let stream = if config.is_empty() {
            Stream::Common
        } else {
            Stream::Impl(config.to_owned())
        };
        self.write_build_to(&stream, &build);
    }

    /// `cmGlobalNinjaGenerator::AppendTargetDependsClosure(target, outputs,
    /// config, fileConfig, genexOutput, omit_self)`.
    fn depends_closure(
        &mut self,
        target: &Target,
        config: &str,
        file_config: &str,
        genex_output: bool,
        omit_self: bool,
    ) -> Rc<BTreeSet<Rc<str>>> {
        let key = format!(
            "{file_config}\0{config}\0{genex_output}\0{}:{}",
            target.makefile, target.name
        );
        let cached = match self.depends_closures.get(&key) {
            Some(cached) => cached.clone(),
            None => {
                let mut outs = BTreeSet::new();
                for depend in link::target_depends(&self.ctx, target).iter() {
                    let dep = &self.ctx.interp.ext.targets[&depend.key];
                    let is_cross = depend.cross;
                    if !self.is_single_config_utility(target)
                        && !self.is_single_config_utility(dep)
                        && self.cross_enabled()
                        && !is_cross
                        && !genex_output
                    {
                        continue;
                    }
                    let dep_config = if is_cross { file_config } else { config };
                    let closure =
                        self.depends_closure(dep, dep_config, file_config, genex_output, false);
                    outs.extend(closure.iter().cloned());
                }
                let outs = Rc::new(outs);
                self.depends_closures.insert(key, outs.clone());
                outs
            }
        };
        let mut result = cached;
        if !omit_self {
            let mut own = Vec::new();
            self.append_target_outputs_cfg(target, &mut own, config, DependOn::TargetArtifact);
            Rc::make_mut(&mut result).extend(own.into_iter().map(Rc::from));
        }
        result
    }

    /// `cmLocalNinjaGenerator::WriteCustomCommandBuildStatement` for the
    /// current file config.
    fn write_custom_command_build_statement(&mut self, cc: CcRef, targets: &[&'a Target]) {
        let file_config = self.file_config.clone();
        if !self.seen_custom_commands.insert((cc, file_config.clone())) {
            return;
        }
        let command = self.command(cc);
        let ccgs = custom::make_generators(&self.ctx, command, &file_config);
        let genex_output = ccgs.len() > 1;
        for (output_config, evaluated) in ccgs {
            self.write_custom_command_ccg(
                cc,
                targets,
                &output_config,
                &file_config,
                genex_output,
                evaluated,
            );
        }
    }

    /// The per-`cmCustomCommandGenerator` body of
    /// `cmLocalNinjaGenerator::WriteCustomCommandBuildStatement`.
    fn write_custom_command_ccg(
        &mut self,
        cc: CcRef,
        targets: &[&'a Target],
        output_config: &str,
        file_config: &str,
        genex_output: bool,
        evaluated: custom::EvaluatedCommand,
    ) {
        let command = self.command(cc);
        let makefile = cc.0;
        if evaluated.outputs.is_empty() && evaluated.byproducts.is_empty() {
            // Generator expressions evaluate to no output for this config.
            return;
        }
        let mut order_only: Rc<BTreeSet<Rc<str>>> = Rc::default();
        if !command.depends_explicit_only {
            // The intersection of the target-depends closures of all
            // targets using the command.
            let mut iter = targets.iter();
            if let Some(first) = iter.next() {
                order_only =
                    self.depends_closure(first, output_config, file_config, genex_output, true);
            }
            for target in iter {
                let deps =
                    self.depends_closure(target, output_config, file_config, genex_output, true);
                Rc::make_mut(&mut order_only).retain(|dep| deps.contains(dep));
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
        let order_only: Vec<String> = order_only.iter().map(ToString::to_string).collect();
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
        let mut custom_step = util::filename_name(&main_output);
        if self.ctx.multi_config {
            // fileConfig and ccg.GetOutputConfig() (equal without
            // cross-config builds).
            custom_step = format!("{custom_step}-{file_config}-{output_config}");
        }
        let custom_step = format!("{custom_step}-{}", &hash[..7]);
        let mut depfile = evaluated.depfile.clone();
        if !depfile.is_empty() {
            let status = crate::policy::index_of("CMP0116")
                .and_then(|index| command.policies.get(index))
                .unwrap_or(crate::policy::PolicyStatus::Warn);
            match status {
                crate::policy::PolicyStatus::New => {
                    depfile = custom::internal_depfile_name(&self.ctx, &evaluated.full_depfile);
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
        let description = evaluated.description(&self.ctx, makefile, "");
        let command_line = self.build_command_line_step(
            &lines,
            &custom_step,
            None,
            makefile,
            output_config,
            file_config,
        );
        self.write_custom_command_build(
            &command_line,
            &description,
            &format!("Custom command for {main_output}"),
            &depfile,
            &command.job_pool,
            command.uses_terminal,
            !symbolic || !evaluated.byproducts.is_empty(),
            file_config,
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
/// `cmNinjaNormalTargetGenerator::GetVisibleTypeName`.
fn visible_type_name(target: &Target) -> &'static str {
    match target.kind {
        TargetKind::StaticLibrary => "static library",
        TargetKind::SharedLibrary => "shared library",
        TargetKind::ModuleLibrary if target.is_cf_bundle_on_apple() => "CFBundle shared module",
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
