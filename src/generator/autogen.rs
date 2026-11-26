//! Qt AUTOGEN (AUTOMOC, AUTOUIC and AUTORCC) at generate time:
//! `cmQtAutoGenGlobalInitializer` and `cmQtAutoGenInitializer`.
//!
//! `initialize` runs inside `cmGlobalGenerator::Compute` while targets are
//! still mutable: per target it first evaluates everything read-only and then
//! adds the generated sources, include directories, custom commands and
//! `_autogen` targets.  `write_info_files` is `SetupCustomTargets`, run at the
//! start of `cmGlobalGenerator::Generate`.  The build-time half is
//! `cmcmd::autogen`.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::Path;

use serde_json::{Value, json};

use super::ctx::{Diagnostic, GenCtx};
use super::target as gt;
use crate::cache::convert_to_unix_slashes;
use crate::cmcmd::autogen::{
    RccLister, libmap::LibMap, quoted, quoted_command, rcc_merge_options, str_to_ulong,
    sub_dir_prefix,
};
use crate::commands::custom_actions;
use crate::commands::target::{
    find_known_source, generated_for_user, get_or_create_source, register_target, resolve_source,
    source_index,
};
use crate::genex::GenexHost;
use crate::interp::Interp;
use crate::policy::{PolicyMap, PolicyStatus, index_of, policy_warning};
use crate::state::{CustomCommand, CustomCommandKind, GeneratorAction, Target, TargetKind};
use crate::util::{
    collapse_full_path, expand_list, filename_last_extension, filename_name, filename_path,
    filename_without_last_extension, is_off, is_on, is_on_opt, is_subdirectory,
};

#[derive(Clone, Copy, Default, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct IntegerVersion {
    major: u32,
    minor: u32,
}

/// cmStrToULong for the version variables.
fn to_uint(value: Option<&str>) -> u32 {
    value
        .map(str::trim_start)
        .and_then(|value| value.parse::<u64>().ok())
        .map_or(0, |value| value as u32)
}

/// `parseMocVersion`: the "major.minor" after "moc ".
fn parse_moc_version(output: &str) -> IntegerVersion {
    let Some(pos) = output.find("moc ") else {
        return IntegerVersion::default();
    };
    let mut parts = output[pos + 4..].splitn(3, '.');
    let (Some(major), Some(minor)) = (parts.next(), parts.next()) else {
        return IntegerVersion::default();
    };
    // std::stoi: leading digits.
    let leading = |text: &str| {
        let text = text.trim_start();
        let digits: String = text.chars().take_while(char::is_ascii_digit).collect();
        digits.parse::<u32>().unwrap_or(0)
    };
    IntegerVersion {
        major: leading(major),
        minor: leading(minor),
    }
}

/// `GetMocVersion`: `moc --version`.
fn moc_version(executable: &str) -> IntegerVersion {
    match std::process::Command::new(executable)
        .arg("--version")
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
    {
        Ok(output) if output.status.code() == Some(0) => {
            parse_moc_version(&String::from_utf8_lossy(&output.stdout))
        }
        _ => IntegerVersion::default(),
    }
}

/// `cmQtAutoGenInitializer::GetQtVersion`: the version found and the
/// version the target requested through QT_MAJOR_VERSION.
fn qt_version(ctx: &GenCtx, target: &Target, moc_executable: &str) -> (IntegerVersion, u32) {
    let host = super::genex_host::CtxHost::new(ctx, target.makefile);
    let requested = host
        .compatible_interface_property(
            &ctx.target_key(target),
            "QT_MAJOR_VERSION",
            &ctx.config,
            None,
            false,
        )
        .ok()
        .flatten();
    let requested = to_uint(requested.as_deref());
    let keys = [
        ("Qt6Core_VERSION_MAJOR", "Qt6Core_VERSION_MINOR"),
        ("Qt5Core_VERSION_MAJOR", "Qt5Core_VERSION_MINOR"),
        ("QT_VERSION_MAJOR", "QT_VERSION_MINOR"),
    ];
    let mut known = Vec::new();
    for (major, minor) in keys {
        let version = IntegerVersion {
            major: to_uint(ctx.var(target.makefile, major).as_deref()),
            minor: to_uint(ctx.var(target.makefile, minor).as_deref()),
        };
        if version.major != 0 {
            known.push(version);
        }
    }
    let props = &ctx.interp.makefiles[target.makefile].props;
    for (major, minor) in keys {
        let version = IntegerVersion {
            major: to_uint(props.get(major)),
            minor: to_uint(props.get(minor)),
        };
        if version.major != 0 {
            known.push(version);
        }
    }
    let mut found = IntegerVersion::default();
    if !known.is_empty() {
        if requested == 0 {
            found = known[0];
        } else if let Some(version) = known.iter().find(|version| version.major == requested) {
            found = *version;
        }
    }
    if found.major == 0 {
        let mut executable = moc_executable.to_owned();
        if executable.is_empty()
            && let Some(moc) = ctx.target_at(target.makefile, &format!("Qt{requested}::moc"))
        {
            executable = moc
                .props
                .get("IMPORTED_LOCATION")
                .unwrap_or_default()
                .to_owned();
        }
        if !executable.is_empty() {
            found = moc_version(&executable);
        }
    }
    (found, requested)
}

/// `cmQtAutoGen::Tools`.
fn tools(moc: bool, uic: bool, rcc: bool) -> String {
    let list: Vec<&str> = [(moc, "AUTOMOC"), (uic, "AUTOUIC"), (rcc, "AUTORCC")]
        .into_iter()
        .filter_map(|(on, name)| on.then_some(name))
        .collect();
    match list.as_slice() {
        [one] => (*one).to_owned(),
        [a, b] => format!("{a} and {b}"),
        [a, b, c] => format!("{a}, {b} and {c}"),
        _ => String::new(),
    }
}

/// `cmake::HeaderFileExtensions`.
const HEADER_EXTENSIONS: &[&str] = &["h", "hh", "h++", "hm", "hpp", "hxx", "in", "txx"];
/// `cmake::CLikeSourceFileExtensions`.
const C_LIKE_EXTENSIONS: &[&str] = &[
    "c", "C", "c++", "cc", "cpp", "cxx", "cu", "mpp", "m", "M", "mm", "ixx", "cppm", "ccm", "cxxm",
    "c++m",
];
/// `cmQtAutoGen::ParallelMax`.
const PARALLEL_MAX: usize = 64;

/// `cmQtAutoGen::ConfigStrings`: the default value plus one per
/// configuration, in insertion order.
#[derive(Clone, Debug, Default)]
struct Cfg<T> {
    default: T,
    config: Vec<(String, T)>,
}

impl<T> Cfg<T> {
    fn get(&self, config: &str) -> Option<&T> {
        self.config
            .iter()
            .find(|(name, _)| name == config)
            .map(|(_, value)| value)
    }
}

impl<T: Clone> Cfg<T> {
    /// `Config` in `std::unordered_map` iteration order.
    fn unordered(&self) -> Vec<(String, T)> {
        let mut map = LibMap::default();
        for (config, value) in &self.config {
            map.insert(config.clone(), value.clone());
        }
        map.iter().cloned().collect()
    }
}

/// `cmQtAutoGenInitializer::MUFile`.
#[derive(Clone, Debug)]
struct MuFile {
    full_path: String,
    configs: Vec<usize>,
    generated: bool,
    skip_moc: bool,
    skip_uic: bool,
    moc_it: bool,
    uic_it: bool,
    /// `SF->GetProperty("SKIP_PRECOMPILE_HEADERS")` is set.
    skip_pch: bool,
}

/// `cmQtAutoGenInitializer::Qrc`.
#[derive(Clone, Debug, Default)]
struct Qrc {
    lock_file: String,
    qrc_file: String,
    qrc_name: String,
    path_checksum: String,
    info_file: String,
    settings_file: Cfg<String>,
    output_file: String,
    generated: bool,
    unique: bool,
    options: Vec<String>,
    resources: Cfg<Vec<String>>,
}

/// `cmQtAutoGenInitializer::GenVarsT` executable part.
#[derive(Clone, Debug, Default)]
struct Tool {
    /// `ExecutableTargetName`.
    target_name: String,
    /// `ExecutableTarget`'s target name.
    target: Option<String>,
    executable: Cfg<String>,
    /// `ExecutableFeatures`: the `-h` output (`None` before evaluation).
    help: Cfg<String>,
    list_options: Cfg<Vec<String>>,
}

#[derive(Debug, Default)]
struct Moc {
    tool: Tool,
    relaxed: bool,
    path_prefix: bool,
    compilation_file: Cfg<String>,
    compilation_genex: String,
    predefs_cmd: Vec<String>,
    predefs_file: Cfg<String>,
    defines: Cfg<Vec<String>>,
    includes: Cfg<Vec<String>>,
    options: Vec<String>,
    macro_names: Vec<String>,
    depend_filters: Vec<(String, String)>,
}

#[derive(Debug, Default)]
struct Uic {
    tool: Tool,
    skip_ui: BTreeSet<String>,
    files_no_options: Vec<String>,
    files_with_options: Vec<(String, Vec<String>)>,
    options: Cfg<Vec<String>>,
    search_paths: Vec<String>,
    headers: Vec<(Cfg<String>, String)>,
}

#[derive(Debug, Default)]
struct Rcc {
    tool: Tool,
    qrcs: Vec<Qrc>,
}

/// `cmQtAutoGenInitializer`.
#[derive(Debug, Default)]
pub struct Initializer {
    key: String,
    name: String,
    dir: usize,
    qt: IntegerVersion,
    moc_enabled: bool,
    uic_enabled: bool,
    rcc_enabled: bool,
    multi: bool,
    cross: bool,
    better_graph: bool,
    verbosity: u64,
    config_default: String,
    configs: Vec<String>,
    folder: String,
    cmp0071: Handling,
    cmp0100: Handling,
    dir_info: String,
    dir_build: String,
    dir_relative_build: String,
    dir_work: String,
    dir_include: Cfg<String>,
    include_genex: String,
    clean_files: Vec<String>,
    target_name: String,
    parallel: u64,
    info_file: String,
    settings_file: Cfg<String>,
    parse_cache_file: Cfg<String>,
    depend_origin: bool,
    depend_files: BTreeSet<String>,
    depend_targets: BTreeSet<String>,
    dep_file: Cfg<String>,
    dep_file_rule: Cfg<String>,
    headers: Vec<MuFile>,
    sources: Vec<MuFile>,
    /// Headers found on disk next to the sources (`Makefile->CreateSource`).
    created_headers: Vec<String>,
    moc: Moc,
    uic: Uic,
    rcc: Rcc,
    rcc_no_zstd: Option<String>,
}

/// Handling of a file kind under CMP0071 / CMP0100.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum Handling {
    #[default]
    Ignore,
    Warn,
    Accept,
}

impl Handling {
    fn new(status: PolicyStatus) -> Self {
        match status {
            PolicyStatus::New => Handling::Accept,
            PolicyStatus::Warn => Handling::Warn,
            PolicyStatus::Old => Handling::Ignore,
        }
    }
}

/// Reports what a read-only phase recorded; false when it failed.
fn flush(interp: &mut Interp, diagnostics: Vec<Diagnostic>) -> bool {
    let mut ok = true;
    for diagnostic in diagnostics {
        ok &= !diagnostic.is_error(interp.dev_warnings_as_errors);
        match diagnostic {
            Diagnostic::Message(kind, backtrace, text) => interp.issue_at(kind, &text, &backtrace),
            Diagnostic::Bare(text) => crate::configure::system_error(Some(interp), &text),
        }
    }
    ok
}

/// `cmSystemTools::RelativePath`.
fn relative_path(local: &str, remote: &str) -> String {
    let local: Vec<&str> = local.split('/').filter(|part| !part.is_empty()).collect();
    let remote: Vec<&str> = remote.split('/').filter(|part| !part.is_empty()).collect();
    let common = local
        .iter()
        .zip(&remote)
        .take_while(|(a, b)| a == b)
        .count();
    let mut parts = vec![".."; local.len() - common];
    parts.extend(&remote[common..]);
    parts.join("/")
}

/// `FileProjectRelativePath`.
fn project_relative_path(interp: &Interp, dir: usize, file: &str) -> String {
    let source = relative_path(&interp.makefiles[dir].source_dir, file);
    let binary = relative_path(&interp.makefiles[dir].binary_dir, file);
    if source.len() < binary.len() {
        source
    } else if binary.len() < file.len() {
        binary
    } else {
        file.to_owned()
    }
}

fn real_path(path: &str) -> String {
    std::fs::canonicalize(path).map_or_else(
        |_| collapse_full_path(path, ""),
        |path| path.to_string_lossy().into_owned(),
    )
}

/// `cmFilePathChecksum::getPart` with the default part length.
fn path_checksum(interp: &Interp, dir: usize, file: &str) -> String {
    let mf = &interp.makefiles[dir];
    let file = real_path(file);
    let parents = [
        (&mf.source_dir, "CurrentSource"),
        (&mf.binary_dir, "CurrentBinary"),
        (&interp.home_source, "ProjectSource"),
        (&interp.home_binary, "ProjectBinary"),
    ];
    let (parent, seed) = parents
        .iter()
        .map(|(parent, seed)| (real_path(parent), *seed))
        .find(|(parent, _)| !parent.is_empty() && is_subdirectory(&file, parent))
        .unwrap_or(("/".to_owned(), "FileSystemRoot"));
    let relative = relative_path(&parent, &filename_path(&file));
    let hash = <sha2::Sha256 as sha2::Digest>::digest(format!("{seed}{relative}").as_bytes());
    // cmBase32Encoder: ten digits are 50 bits of the hash.
    let bits = hash[..8]
        .iter()
        .fold(0u64, |bits, byte| bits << 8 | u64::from(*byte));
    (0..10)
        .map(|digit| {
            char::from(
                b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567"[(bits >> (59 - 5 * digit) & 0x1f) as usize],
            )
        })
        .collect()
}

/// `cmGlobalGenerator::GetCompilerFeatures`' cache, per executable.
type HelpCache = HashMap<String, String>;

/// Runs the tool's `-h`; the error text on failure.
fn tool_help(generator: &str, executable: &str, cache: &mut HelpCache) -> Result<String, String> {
    if let Some(help) = cache.get(executable) {
        return Ok(help.clone());
    }
    if !Path::new(executable).is_file() {
        return Err(format!(
            "The \"{generator}\" executable {} does not exist.",
            quoted(executable)
        ));
    }
    let output = std::process::Command::new(executable)
        .arg("-h")
        .stdin(std::process::Stdio::null())
        .output()
        .map_err(|_| {
            format!(
                "Test run of \"{generator}\" executable {} failed.\n{}\n\n",
                quoted(executable),
                quoted_command(&[executable.to_owned(), "-h".to_owned()])
            )
        })?;
    let help = String::from_utf8_lossy(&output.stdout).into_owned();
    cache.insert(executable.to_owned(), help.clone());
    Ok(help)
}

/// The constructor loop of `cmQtAutoGenGlobalInitializer` (C# targets, which
/// it skips, are not supported by cmake-rust at all).
fn find_targets(ctx: &GenCtx) -> Vec<Initializer> {
    let mut found = Vec::new();
    for dir in 0..ctx.interp.makefiles.len() {
        for target in ctx.targets_in_order() {
            if target.makefile != dir
                || target.imported
                || target.alias_of.is_some()
                || !matches!(
                    target.kind,
                    TargetKind::Executable
                        | TargetKind::StaticLibrary
                        | TargetKind::SharedLibrary
                        | TargetKind::ModuleLibrary
                        | TargetKind::ObjectLibrary
                )
            {
                continue;
            }
            let on = |name: &str| is_on_opt(target.props.get(name));
            let (moc, uic, rcc) = (on("AUTOMOC"), on("AUTOUIC"), on("AUTORCC"));
            if !(moc || uic || rcc) {
                continue;
            }
            let exec = |name: &str| target.props.get(name).unwrap_or_default().to_owned();
            let (moc_exec, uic_exec, rcc_exec) = (
                exec("AUTOMOC_EXECUTABLE"),
                exec("AUTOUIC_EXECUTABLE"),
                exec("AUTORCC_EXECUTABLE"),
            );
            let (version, requested) = qt_version(ctx, target, &moc_exec);
            let valid = matches!(version.major, 4..=6);
            let moc_valid = moc && (valid || !moc_exec.is_empty());
            let uic_valid = uic && (valid || !uic_exec.is_empty());
            let rcc_valid = rcc && (valid || !rcc_exec.is_empty());
            let (moc_off, uic_off, rcc_off) =
                (moc && !moc_valid, uic && !uic_valid, rcc && !rcc_valid);
            if moc_off || uic_off || rcc_off {
                let version = if requested == 0 {
                    "<QTVERSION>".to_owned()
                } else {
                    requested.to_string()
                };
                let component = if uic_off { "Widgets" } else { "Core" };
                ctx.warning(
                    gt::directory_backtrace(ctx, target.makefile),
                    format!(
                        "AUTOGEN: No valid Qt version found for target {}.  {} disabled.  Consider adding:\n  find_package(Qt{version} COMPONENTS {component})\nto your CMakeLists.txt file.",
                        target.name,
                        tools(moc_off, uic_off, rcc_off)
                    ),
                );
            }
            if moc_valid || uic_valid || rcc_valid {
                let better = target.props.get("AUTOGEN_BETTER_GRAPH_MULTI_CONFIG");
                found.push(Initializer {
                    key: ctx.target_key(target),
                    name: target.name.clone(),
                    dir,
                    qt: version,
                    moc_enabled: moc_valid,
                    uic_enabled: uic_valid,
                    rcc_enabled: rcc_valid,
                    cross: !ctx.var_safe(dir, "CMAKE_CROSS_CONFIGS").is_empty(),
                    better_graph: better
                        .map_or(version >= IntegerVersion { major: 6, minor: 8 }, is_on),
                    ..Initializer::default()
                });
            }
        }
    }
    found
}

/// The `cmQtAutoGenGlobalInitializer` constructor and
/// `InitializeCustomTargets`.  `None` when generation must stop.
pub fn initialize(interp: &mut Interp) -> Option<Vec<Initializer>> {
    let (mut found, globals, diagnostics) = {
        let ctx = GenCtx::new(interp);
        let mut globals = Vec::new();
        for (variable, fallback, comment) in [
            (
                "CMAKE_GLOBAL_AUTOGEN_TARGET",
                "autogen",
                "Global AUTOGEN target",
            ),
            (
                "CMAKE_GLOBAL_AUTORCC_TARGET",
                "autorcc",
                "Global AUTORCC target",
            ),
        ] {
            let mut names = BTreeMap::new();
            for dir in 0..interp.makefiles.len() {
                if is_on_opt(ctx.var(dir, variable).as_deref()) {
                    let name = ctx.var_safe(dir, &format!("{variable}_NAME"));
                    names.insert(
                        dir,
                        if name.is_empty() {
                            fallback.to_owned()
                        } else {
                            name
                        },
                    );
                }
            }
            globals.push((names, comment));
        }
        (find_targets(&ctx), globals, ctx.errors.take())
    };
    if !flush(interp, diagnostics) {
        return None;
    }
    // AUTOGEN_BETTER_GRAPH_MULTI_CONFIG is set explicitly because it is read
    // by the Qt library.
    for init in &found {
        interp.ext.targets.get_mut(&init.key).unwrap().props.set(
            "AUTOGEN_BETTER_GRAPH_MULTI_CONFIG",
            if init.better_graph { "ON" } else { "OFF" }.to_owned(),
        );
    }
    for (names, comment) in &globals {
        for (dir, name) in names {
            if crate::state::find_target_key_at(interp, *dir, name).is_some() {
                continue;
            }
            let folder = interp
                .global_props
                .get("AUTOGEN_TARGETS_FOLDER")
                .map(str::to_owned);
            add_utility(
                interp,
                name,
                folder.as_deref(),
                CustomCommand {
                    working_directory: interp.home_binary.clone(),
                    comment: Some((*comment).to_owned()),
                    ..generator_command(*dir, CustomCommandKind::Utility, true)
                },
            );
        }
    }
    let mut cache = HelpCache::new();
    for init in &mut found {
        let (ok, diagnostics) = {
            let ctx = GenCtx::new(interp);
            (init.evaluate(&ctx, &mut cache), ctx.errors.take())
        };
        // Errors of target computations, such as missing source files, do
        // not fail the initializer itself.
        flush(interp, diagnostics);
        if !ok {
            interp.error = true;
            return None;
        }
        init.apply(interp, &globals[0].0, &globals[1].0);
    }
    Some(found)
}

/// A command created by the generator (`cmCommandOrigin::Generator`): every
/// recorded policy keeps its NEW default.
fn generator_command(dir: usize, kind: CustomCommandKind, verbatim: bool) -> CustomCommand {
    let mut policies = PolicyMap::default();
    policies.set(index_of("CMP0116").expect("CMP0116"), PolicyStatus::New);
    CustomCommand {
        kind,
        makefile: dir,
        verbatim,
        policies,
        ..Default::default()
    }
}

/// `cmLocalGenerator::AddUtilityCommand(name, true, cc)` and the FOLDER.
fn add_utility(interp: &mut Interp, name: &str, folder: Option<&str>, mut command: CustomCommand) {
    let dir = command.makefile;
    let backtrace = directory_backtrace(interp, dir);
    let mut target = Target::new(name, TargetKind::Utility, dir);
    target
        .backtrace
        .clone_from(&interp.makefiles[dir].backtrace);
    for frame in &interp.makefiles[dir].policies {
        target.policies.overlay(&frame.map);
    }
    target.generator_provided = true;
    target.props.set("EXCLUDE_FROM_ALL", "TRUE".to_owned());
    let key = register_target(interp, target);
    if let Some(folder) = folder {
        let target = interp.ext.targets.get_mut(&key).unwrap();
        target.props.set("FOLDER", folder.to_owned());
    }
    if command.command_lines.is_empty() && command.depends.is_empty() {
        return;
    }
    command.backtrace = backtrace;
    mark_outputs(interp, &command);
    custom_actions::run_action(
        interp,
        &mut super::expand_output_genex,
        GeneratorAction::Utility {
            target: key,
            command,
        },
    );
}

/// `cmLocalGenerator::AddCustomCommandToOutput`.
fn add_output_command(interp: &mut Interp, mut command: CustomCommand) {
    command.backtrace = directory_backtrace(interp, command.makefile);
    mark_outputs(interp, &command);
    custom_actions::run_action(
        interp,
        &mut super::expand_output_genex,
        GeneratorAction::Output {
            command,
            replace: false,
        },
    );
}

/// `CreateGeneratedSource` with `cmCommandOrigin::Generator`.
fn mark_outputs(interp: &mut Interp, command: &CustomCommand) {
    for output in command.outputs.iter().chain(&command.byproducts) {
        crate::commands::target::mark_generated(interp, command.makefile, output);
    }
}

fn directory_backtrace(interp: &Interp, dir: usize) -> Vec<crate::messages::Frame> {
    let ctx = GenCtx::new(interp);
    ctx.dir_backtrace(dir)
}

impl Initializer {
    fn moc_or_uic(&self) -> bool {
        self.moc_enabled || self.uic_enabled
    }

    /// `ConfigFileNames`.
    fn file_names(&self, prefix: &str, suffix: &str) -> Cfg<String> {
        let mut names = Cfg {
            default: format!("{prefix}{suffix}"),
            config: Vec::new(),
        };
        if self.multi {
            for config in &self.configs {
                names
                    .config
                    .push((config.clone(), format!("{prefix}_{config}{suffix}")));
            }
        }
        names
    }

    /// `ConfigFileNamesAndGenex`.
    fn file_names_genex(&self, prefix: &str, suffix: &str) -> (Cfg<String>, String) {
        let names = self.file_names(prefix, suffix);
        let genex = if self.multi {
            format!("{prefix}_$<CONFIG>{suffix}")
        } else {
            names.default.clone()
        };
        (names, genex)
    }

    /// `ConfigFileClean`.
    fn clean(&mut self, names: &Cfg<String>) {
        self.clean_files.push(names.default.clone());
        if self.multi {
            self.clean_files
                .extend(names.unordered().into_iter().map(|(_, file)| file));
        }
    }

    /// The configurations one evaluation is made for: (is per-config, name).
    fn evaluations(&self) -> Vec<String> {
        if self.multi {
            self.configs.clone()
        } else {
            vec![self.config_default.clone()]
        }
    }

    /// Stores per-configuration results as upstream does: only the default
    /// for single-config generators, and otherwise every result that differs
    /// from the (empty) default.
    fn collect<T: PartialEq + Default>(&self, value: impl Fn(&str) -> T) -> Cfg<T> {
        let mut result = Cfg::default();
        for config in self.evaluations() {
            let value = value(&config);
            if !self.multi {
                result.default = value;
            } else if value != result.default {
                result.config.push((config, value));
            }
        }
        result
    }

    /// The read-only part of `InitCustomTargets`.
    fn evaluate(&mut self, ctx: &GenCtx, cache: &mut HelpCache) -> bool {
        let interp = ctx.interp;
        let target = &interp.ext.targets[&self.key];
        let dir = self.dir;
        self.multi = ctx.multi_config;
        self.configs = ctx.configs.clone();
        self.config_default = if self.multi {
            String::new()
        } else {
            ctx.config.clone()
        };
        let prop = |name: &str| target.props.get(name).unwrap_or_default().to_owned();

        let verbose = ctx.var_safe(dir, "CMAKE_AUTOGEN_VERBOSE");
        if !verbose.is_empty() {
            self.verbosity = str_to_ulong(&verbose)
                .map_or_else(|| u64::from(is_on(&verbose)), |v| u64::from(v as u32));
        }
        self.folder = interp
            .global_props
            .get("AUTOMOC_TARGETS_FOLDER")
            .or_else(|| interp.global_props.get("AUTOGEN_TARGETS_FOLDER"))
            .or_else(|| target.props.get("FOLDER"))
            .unwrap_or_default()
            .to_owned();
        self.cmp0071 = Handling::new(interp.policy_status_at(dir, "CMP0071"));
        self.cmp0100 = Handling::new(interp.policy_status_at(dir, "CMP0100"));

        let binary = collapse_full_path("", &interp.makefiles[dir].binary_dir);
        self.dir_info = format!("{binary}/CMakeFiles/{}_autogen.dir", self.name);
        self.dir_build = prop("AUTOGEN_BUILD_DIR");
        if self.dir_build.is_empty() {
            self.dir_build = format!("{binary}/{}_autogen", self.name);
        }
        self.dir_build = convert_to_unix_slashes(&self.dir_build);
        self.dir_relative_build = relative_path(&binary, &self.dir_build);
        self.clean_files.push(self.dir_build.clone());
        self.dir_work = binary;
        (self.dir_include, self.include_genex) =
            self.file_names_genex(&format!("{}/include", self.dir_build), "");

        if self.moc_or_uic() {
            if self.moc_enabled && !self.init_moc(ctx, target, cache) {
                return false;
            }
            if self.uic_enabled && !self.init_uic(ctx, target, cache) {
                return false;
            }
            self.target_name = format!("{}_autogen", self.name);
            let parallel = prop("AUTOGEN_PARALLEL");
            self.parallel = if parallel.is_empty() || parallel == "AUTO" {
                crate::commands::sysinfo::physical_cores().clamp(1, PARALLEL_MAX) as u64
            } else {
                match str_to_ulong(&parallel) {
                    Some(value) if value > 0 && value <= u64::from(u32::MAX) => value,
                    _ => {
                        ctx.warning(
                            ctx.dir_backtrace(dir),
                            format!(
                                "AUTOGEN_PARALLEL=\"{parallel}\" for target \"{}\" is not valid. Using AUTOGEN_PARALLEL=1",
                                self.name
                            ),
                        );
                        1
                    }
                }
            };
            self.info_file = format!("{}/AutogenInfo.json", self.dir_info);
            self.settings_file = self.file_names(&format!("{}/AutogenUsed", self.dir_info), ".txt");
            self.clean(&self.settings_file.clone());
            self.parse_cache_file =
                self.file_names(&format!("{}/ParseCache", self.dir_info), ".txt");
            self.clean(&self.parse_cache_file.clone());

            self.depend_origin = is_on(&prop("AUTOGEN_ORIGIN_DEPENDS"));
            for name in expand_list(&prop("AUTOGEN_TARGET_DEPENDS"), false) {
                match ctx.target_at(dir, &name) {
                    Some(target) => self.depend_targets.insert(target.name.clone()),
                    None => self.depend_files.insert(name),
                };
            }
            if self.moc_enabled {
                self.moc.path_prefix = is_on(&prop("AUTOMOC_PATH_PREFIX"));
                if is_on_opt(ctx.var(dir, "CMAKE_AUTOMOC_RELAXED_MODE").as_deref()) {
                    self.moc.relaxed = true;
                    ctx.warning(
                        ctx.dir_backtrace(dir),
                        format!(
                            "AUTOMOC: CMAKE_AUTOMOC_RELAXED_MODE is deprecated an will be removed in the future.  Consider disabling it and converting the target {} to regular mode.",
                            self.name
                        ),
                    );
                }
                self.moc.options = expand_list(&prop("AUTOMOC_MOC_OPTIONS"), false);
                self.moc.macro_names = unique(expand_list(&prop("AUTOMOC_MACRO_NAMES"), false));
                let filters = expand_list(&prop("AUTOMOC_DEPEND_FILTERS"), false);
                if !filters.len().is_multiple_of(2) {
                    ctx.system_error(format!(
                        "AutoMoc: AUTOMOC_DEPEND_FILTERS predefs size {} is not a multiple of 2.",
                        filters.len()
                    ));
                    return false;
                }
                self.moc.depend_filters.push((
                    "Q_PLUGIN_METADATA".to_owned(),
                    "[\n][ \t]*Q_PLUGIN_METADATA[ \t]*\\([^\\)]*FILE[ \t]*\"([^\"]+)\"".to_owned(),
                ));
                for pair in filters.chunks(2) {
                    self.moc
                        .depend_filters
                        .push((pair[0].clone(), pair[1].clone()));
                }
            }
        }
        if self.rcc_enabled && !self.init_rcc(ctx, target, cache) {
            return false;
        }
        if !self.scan_files(ctx, target) {
            return false;
        }
        if self.moc_or_uic() && self.depend_origin {
            self.add_origin_depends(ctx, target);
        }
        true
    }

    /// `InitMoc`.
    fn init_moc(&mut self, ctx: &GenCtx, target: &Target, cache: &mut HelpCache) -> bool {
        let dir = self.dir;
        (self.moc.compilation_file, self.moc.compilation_genex) =
            self.file_names_genex(&format!("{}/mocs_compilation", self.dir_build), ".cpp");
        if is_on_opt(target.props.get("AUTOMOC_COMPILER_PREDEFINES"))
            && self.qt >= (IntegerVersion { major: 5, minor: 8 })
        {
            self.moc.predefs_cmd = expand_list(
                &ctx.var_safe(dir, "CMAKE_CXX_COMPILER_PREDEFINES_COMMAND"),
                false,
            );
            if !self.moc.predefs_cmd.is_empty() {
                self.moc.predefs_file =
                    self.file_names(&format!("{}/moc_predefs", self.dir_build), ".h");
            }
        }
        if let Some(value) = target.props.get("AUTOMOC_INCLUDE_DIRECTORIES") {
            for config in self.evaluations() {
                let view = ctx.for_config(&config);
                let dirs = expand_list(
                    &view.genex_full(value, dir, None, None, false, &ctx.dir_backtrace(dir)),
                    false,
                );
                if let Some(path) = dirs
                    .iter()
                    .find(|path| !path.starts_with("$<") && !Path::new(path).is_absolute())
                {
                    ctx.error(
                        ctx.dir_backtrace(dir),
                        format!("AUTOMOC_INCLUDE_DIRECTORIES: path '{path}' is not absolute."),
                    );
                    return false;
                }
                if self.multi {
                    self.moc.includes.config.push((config, dirs));
                } else {
                    self.moc.includes.default = dirs;
                }
            }
        } else {
            let append_implicit = self.qt.major >= 5;
            self.moc.includes = self.collect(|config| {
                sanitize(
                    ctx,
                    dir,
                    super::flags::include_directories(ctx, target, config, "CXX", append_implicit)
                        .into_iter()
                        .map(|item| item.0)
                        .collect(),
                )
            });
        }
        let windows =
            self.moc.predefs_cmd.is_empty() && ctx.var_safe(dir, "CMAKE_SYSTEM_NAME") == "Windows";
        self.moc.defines = self.collect(|config| {
            let mut defines: BTreeSet<String> =
                super::flags::define_items(ctx, target, config, "CXX")
                    .into_iter()
                    .map(|(define, _)| define)
                    .collect();
            if windows {
                defines.insert("WIN32".to_owned());
            }
            defines.into_iter().collect()
        });
        let Some(tool) = self.qt_executable(ctx, target, "moc", "AUTOMOC", false, cache) else {
            return false;
        };
        self.depend_targets.extend(tool.target.clone());
        self.moc.tool = tool;
        true
    }

    /// `InitUic`.
    fn init_uic(&mut self, ctx: &GenCtx, target: &Target, cache: &mut HelpCache) -> bool {
        let paths = target.props.get("AUTOUIC_SEARCH_PATHS").unwrap_or_default();
        if !paths.is_empty() {
            self.uic.search_paths = sanitize(ctx, self.dir, expand_list(paths, false));
        }
        let options = |config: &str| -> Vec<String> {
            let view = ctx.for_config(config);
            let host = super::genex_host::CtxHost::new(&view, self.dir);
            let value = host
                .compatible_interface_property(&self.key, "AUTOUIC_OPTIONS", config, None, false)
                .ok()
                .flatten()
                .or_else(|| target.props.get("AUTOUIC_OPTIONS").map(str::to_owned));
            value.map_or_else(Vec::new, |value| {
                expand_list(
                    &view.genex_with_dag(&value, target, target, "AUTOUIC_OPTIONS", None),
                    false,
                )
            })
        };
        // The default configuration of a multi-config generator is empty.
        let mut result = Cfg {
            default: options(&self.config_default),
            config: Vec::new(),
        };
        if self.multi {
            for config in &self.configs {
                let value = options(config);
                if value != result.default {
                    result.config.push((config.clone(), value));
                }
            }
        }
        self.uic.options = result;
        let Some(tool) = self.qt_executable(ctx, target, "uic", "AUTOUIC", true, cache) else {
            return false;
        };
        self.depend_targets.extend(tool.target.clone());
        self.uic.tool = tool;
        true
    }

    /// `InitRcc`.
    fn init_rcc(&mut self, ctx: &GenCtx, target: &Target, cache: &mut HelpCache) -> bool {
        let Some(mut tool) = self.qt_executable(ctx, target, "rcc", "AUTORCC", false, cache) else {
            return false;
        };
        let list_options = |help: &str| -> Vec<String> {
            if !matches!(self.qt.major, 5 | 6) {
                return Vec::new();
            }
            ["--list", "-list"]
                .iter()
                .find(|option| help.contains(**option))
                .map(|option| vec![(*option).to_owned()])
                .unwrap_or_default()
        };
        tool.list_options.default = list_options(&tool.help.default);
        tool.list_options.config = tool
            .help
            .config
            .iter()
            .map(|(config, help)| (config.clone(), list_options(help)))
            .collect();
        self.rcc.tool = tool;
        if self.qt.major >= 6
            && let Some(zstd) = ctx.var(self.dir, "QT_FEATURE_zstd")
            && is_off(&zstd)
        {
            let mut options = target
                .props
                .get("AUTORCC_OPTIONS")
                .unwrap_or_default()
                .to_owned();
            if !options.contains("--no-zstd") {
                options.push_str(";--no-zstd;");
            }
            self.rcc_no_zstd = Some(options);
        }
        true
    }

    /// `GetQtExecutable`.
    fn qt_executable(
        &self,
        ctx: &GenCtx,
        target: &Target,
        executable: &str,
        upper: &str,
        ignore_missing: bool,
        cache: &mut HelpCache,
    ) -> Option<Tool> {
        let dir = self.dir;
        let per_config = self.multi && self.better_graph;
        let mut tool = Tool::default();
        let error = |text: &str| {
            ctx.system_error(format!("{upper} for target {}: {text}", self.name));
        };
        let property = format!("{upper}_EXECUTABLE");
        let value = target.props.get(&property).unwrap_or_default();
        if !value.is_empty() {
            let evaluate = |config: &str| {
                ctx.for_config(config).genex_full(
                    value,
                    dir,
                    None,
                    None,
                    false,
                    &ctx.dir_backtrace(dir),
                )
            };
            if per_config {
                for config in &self.configs {
                    tool.executable
                        .config
                        .push((config.clone(), evaluate(config)));
                    tool.help.config.push((config.clone(), String::new()));
                }
            } else {
                tool.executable.default = evaluate("");
            }
            if tool.executable.default.is_empty()
                && tool.executable.config.is_empty()
                && !ignore_missing
            {
                error(&format!("{property} evaluates to an empty value"));
                return None;
            }
            return Some(tool);
        }
        let prefix = match self.qt.major {
            4..=6 => format!("Qt{}::", self.qt.major),
            _ => String::new(),
        };
        let target_name = format!("{prefix}{executable}");
        let Some(found) = ctx.target_at(dir, &target_name) else {
            if ignore_missing {
                if per_config {
                    for config in &self.configs {
                        tool.help.config.push((config.clone(), String::new()));
                    }
                }
                return Some(tool);
            }
            error(&format!(
                "Could not find {executable} executable target {target_name}"
            ));
            return None;
        };
        tool.target_name = target_name;
        tool.target = Some(found.name.clone());
        let location = |config: &str| {
            if found.imported {
                gt::imported_full_path(ctx, found, config, gt::ArtifactType::Runtime)
            } else {
                gt::full_path(
                    &ctx.for_config(config),
                    found,
                    config,
                    gt::ArtifactType::Runtime,
                    false,
                )
            }
        };
        if per_config {
            for config in &self.configs {
                tool.executable
                    .config
                    .push((config.clone(), location(config)));
            }
            for config in &self.configs {
                let path = tool.executable.get(config).cloned().unwrap_or_default();
                match tool_help(executable, &path, cache) {
                    Ok(help) => tool.help.config.push((config.clone(), help)),
                    Err(text) => {
                        error(&text);
                        return None;
                    }
                }
            }
        } else {
            tool.executable.default = location(&self.config_default);
            match tool_help(executable, &tool.executable.default, cache) {
                Ok(help) => tool.help.default = help,
                Err(text) => {
                    error(&text);
                    return None;
                }
            }
        }
        Some(tool)
    }

    fn make_mu_file(
        &self,
        ctx: &GenCtx,
        full_path: &str,
        generated: bool,
        configs: &[usize],
        mu_it: bool,
    ) -> MuFile {
        let on = |name: &str| is_on_opt(gt::source_prop(ctx, self.dir, full_path, name).as_deref());
        let skip = on("SKIP_AUTOGEN");
        let skip_moc = self.moc_enabled && (skip || on("SKIP_AUTOMOC"));
        let skip_uic = self.uic_enabled && (skip || on("SKIP_AUTOUIC"));
        MuFile {
            full_path: full_path.to_owned(),
            configs: if configs.len() == self.configs.len() {
                Vec::new()
            } else {
                configs.to_vec()
            },
            generated,
            skip_moc,
            skip_uic,
            moc_it: mu_it && self.moc_enabled && !skip_moc,
            uic_it: mu_it && self.uic_enabled && !skip_uic,
            skip_pch: gt::source_prop(ctx, self.dir, full_path, "SKIP_PRECOMPILE_HEADERS")
                .is_some(),
        }
    }

    /// `InitScanFiles`.
    #[allow(clippy::too_many_lines)]
    fn scan_files(&mut self, ctx: &GenCtx, target: &Target) -> bool {
        let interp = ctx.interp;
        let dir = self.dir;
        let mut files_generated: Vec<MuFile> = Vec::new();
        let mut hh_warn: Vec<String> = Vec::new();
        let is_header = |ext: &str| HEADER_EXTENSIONS.contains(&ext);
        let is_source = |ext: &str| C_LIKE_EXTENSIONS.contains(&ext);
        let extension = |path: &str| {
            filename_last_extension(path)
                .trim_start_matches('.')
                .to_owned()
        };

        // cmGeneratorTarget::GetAllConfigSources.
        let mut all: Vec<(String, bool, Vec<usize>)> = Vec::new();
        let mut index: HashMap<String, usize> = HashMap::new();
        for (ci, config) in self.configs.iter().enumerate() {
            let view = ctx.for_config(config);
            for source in gt::sources(&view, target, config).iter() {
                let slot = *index.entry(source.path.clone()).or_insert_with(|| {
                    all.push((source.path.clone(), source.generated, Vec::new()));
                    all.len() - 1
                });
                all[slot].2.push(ci);
            }
        }
        let cmp0100 = self.cmp0100;
        let mut add_header = |file: MuFile,
                              ext: &str,
                              headers: &mut Vec<MuFile>,
                              generated: &mut Vec<MuFile>|
         -> bool {
            let mu_it = file.moc_it || file.uic_it;
            if cmp0100 == Handling::Accept || ext != "hh" {
                if mu_it && file.generated {
                    generated.push(file.clone());
                }
                headers.push(file);
                return true;
            }
            if mu_it && cmp0100 == Handling::Warn {
                hh_warn.push(file.full_path);
            }
            false
        };
        let add_source = |file: MuFile, sources: &mut Vec<MuFile>, generated: &mut Vec<MuFile>| {
            if (file.moc_it || file.uic_it) && file.generated {
                generated.push(file.clone());
            }
            sources.push(file);
        };
        let (mut headers, mut sources) = (Vec::new(), Vec::new());
        for (path, generated, configs) in &all {
            let ext = extension(path).to_ascii_lowercase();
            if self.moc_or_uic() {
                let file = || self.make_mu_file(ctx, path, *generated, configs, true);
                if is_header(&ext) {
                    add_header(file(), &ext, &mut headers, &mut files_generated);
                } else if is_source(&ext) {
                    add_source(file(), &mut sources, &mut files_generated);
                }
            }
            if self.rcc_enabled && ext == "qrc" {
                let property = |name: &str| gt::source_prop(ctx, dir, path, name);
                if !is_on_opt(property("SKIP_AUTOGEN").as_deref())
                    && !is_on_opt(property("SKIP_AUTORCC").as_deref())
                {
                    self.rcc.qrcs.push(Qrc {
                        qrc_file: path.clone(),
                        qrc_name: filename_without_last_extension(path),
                        generated: *generated,
                        options: expand_list(
                            &property("AUTORCC_OPTIONS").unwrap_or_default(),
                            false,
                        ),
                        ..Qrc::default()
                    });
                }
            }
        }

        let mut known_headers: HashSet<String> =
            headers.iter().map(|file| file.full_path.clone()).collect();
        // The default and private headers of the source files.
        if self.moc_or_uic() {
            // Upstream iterates an unordered_map keyed by pointers.  Without
            // bucket collisions libstdc++ visits the latest insertion first.
            for source in sources
                .iter()
                .rev()
                .filter(|file| file.moc_it || file.uic_it)
            {
                let base = format!(
                    "{}{}",
                    sub_dir_prefix(&source.full_path),
                    filename_without_last_extension(&source.full_path)
                );
                for suffix in ["", "_p"] {
                    for ext in HEADER_EXTENSIONS {
                        let path = format!("{base}{suffix}.{ext}");
                        let exists = Path::new(&path).exists();
                        let mut generated = false;
                        if let Some(index) = find_known_source(interp, dir, &path) {
                            if known_headers.contains(&path) {
                                continue;
                            }
                            generated = source_generated(interp, dir, index, &path);
                            if !generated && !exists {
                                continue;
                            }
                        } else if exists {
                            if known_headers.contains(&path) {
                                continue;
                            }
                            self.created_headers.push(path.clone());
                        } else {
                            continue;
                        }
                        let mut file =
                            self.make_mu_file(ctx, &path, generated, &source.configs, true);
                        file.moc_it &= source.moc_it;
                        file.uic_it &= source.uic_it;
                        if add_header(file, ext, &mut headers, &mut files_generated) {
                            known_headers.insert(path);
                        }
                    }
                }
            }
            // Non-target source files contribute their moc and uic parameters.
            let known_sources: HashSet<String> =
                sources.iter().map(|file| file.full_path.clone()).collect();
            for (index, source) in interp
                .ext
                .sources
                .get(&dir)
                .into_iter()
                .flatten()
                .enumerate()
            {
                let Ok(resolved) = resolve_source(interp, dir, &source.location.path()) else {
                    continue;
                };
                let path = resolved.path;
                if path.is_empty() {
                    continue;
                }
                let ext = extension(&path).to_ascii_lowercase();
                let generated = source_generated(interp, dir, index, &path);
                if is_header(&ext) {
                    if !known_headers.contains(&path) {
                        let file = self.make_mu_file(ctx, &path, generated, &[], false);
                        if file.skip_moc || file.skip_uic {
                            add_header(file, &ext, &mut headers, &mut files_generated);
                        }
                    }
                } else if is_source(&ext) {
                    if !known_sources.contains(&path) {
                        let file = self.make_mu_file(ctx, &path, generated, &[], false);
                        if file.skip_moc || file.skip_uic {
                            add_source(file, &mut sources, &mut files_generated);
                        }
                    }
                } else if self.uic_enabled && ext == "ui" {
                    let on = |name: &str| is_on_opt(source.props.get(name));
                    if on("SKIP_AUTOGEN") || on("SKIP_AUTOUIC") {
                        self.uic.skip_ui.insert(path);
                        continue;
                    }
                    let options = source.props.get("AUTOUIC_OPTIONS").unwrap_or_default();
                    if options.is_empty() {
                        self.uic.files_no_options.push(path.clone());
                    } else {
                        self.uic
                            .files_with_options
                            .push((path.clone(), expand_list(options, false)));
                    }
                    let mut relative =
                        relative_path(&interp.makefiles[dir].source_dir, &filename_path(&path));
                    if !relative.is_empty() && !relative.ends_with('/') {
                        relative.push('/');
                    }
                    let suffix =
                        format!("/{relative}ui_{}.h", filename_without_last_extension(&path));
                    self.uic.headers.push(
                        self.file_names_genex(&format!("{}/include", self.dir_build), &suffix),
                    );
                }
            }
        }
        self.headers = headers;
        self.sources = sources;

        let property = match (self.moc_enabled, self.uic_enabled) {
            (true, true) => "SKIP_AUTOGEN",
            (true, false) => "SKIP_AUTOMOC",
            _ => "SKIP_AUTOUIC",
        };
        let list = |files: &mut dyn Iterator<Item = &String>| -> String {
            files.map(|file| format!("  {}\n", quoted(file))).collect()
        };
        if self.moc_or_uic() && !files_generated.is_empty() {
            match self.cmp0071 {
                Handling::Accept => {
                    for file in &files_generated {
                        if self.multi && !self.cross && !file.configs.is_empty() {
                            for ci in &file.configs {
                                self.depend_files.insert(format!(
                                    "$<$<CONFIG:{}>:{}>",
                                    self.configs[*ci], file.full_path
                                ));
                            }
                        } else {
                            self.depend_files.insert(file.full_path.clone());
                        }
                    }
                }
                Handling::Warn => ctx.warning(
                    ctx.dir_backtrace(dir),
                    format!(
                        "{}\nFor compatibility, CMake is excluding the GENERATED source file(s):\n{}from processing by {}.  If any of the files should be processed, set CMP0071 to NEW.  If any of the files should not be processed, explicitly exclude them by setting the source file property {property}:\n  set_property(SOURCE file.h PROPERTY {property} ON)\n",
                        policy_warning(index_of("CMP0071").expect("CMP0071")),
                        list(&mut files_generated.iter().map(|file| &file.full_path)),
                        tools(self.moc_enabled, self.uic_enabled, false),
                    ),
                ),
                Handling::Ignore => {}
            }
        }
        if self.moc_or_uic() && !hh_warn.is_empty() {
            ctx.warning(
                ctx.dir_backtrace(dir),
                format!(
                    "{}\nFor compatibility, CMake is excluding the header file(s):\n{}from processing by {}.  If any of the files should be processed, set CMP0100 to NEW.  If any of the files should not be processed, explicitly exclude them by setting the source file property {property}:\n  set_property(SOURCE file.hh PROPERTY {property} ON)\n",
                    policy_warning(index_of("CMP0100").expect("CMP0100")),
                    list(&mut hh_warn.iter()),
                    tools(self.moc_enabled, self.uic_enabled, false),
                ),
            );
        }
        self.scan_qrcs(ctx, target)
    }

    /// The qrc part of `InitScanFiles`.
    fn scan_qrcs(&mut self, ctx: &GenCtx, target: &Target) -> bool {
        if self.rcc.qrcs.is_empty() {
            return true;
        }
        let modern = self.qt.major >= 5;
        let target_options = expand_list(
            self.rcc_no_zstd
                .as_deref()
                .or_else(|| target.props.get("AUTORCC_OPTIONS"))
                .unwrap_or_default(),
            false,
        );
        let mut qrcs = std::mem::take(&mut self.rcc.qrcs);
        let names: Vec<String> = qrcs.iter().map(|qrc| qrc.qrc_name.clone()).collect();
        let per_config = self.multi && self.better_graph;
        for qrc in &mut qrcs {
            qrc.unique = names.iter().filter(|name| **name == qrc.qrc_name).count() == 1;
            qrc.path_checksum = path_checksum(ctx.interp, self.dir, &qrc.qrc_file);
            qrc.output_file = format!(
                "{}/{}{}/qrc_{}.cpp",
                self.dir_build,
                qrc.path_checksum,
                if per_config { "_$<CONFIG>" } else { "" },
                qrc.qrc_name
            );
            let base = format!(
                "{}/AutoRcc_{}_{}",
                self.dir_info, qrc.qrc_name, qrc.path_checksum
            );
            qrc.lock_file = format!("{base}_Lock.lock");
            qrc.info_file = format!("{base}_Info.json");
            qrc.settings_file = self.file_names(&format!("{base}_Used"), ".txt");

            let mut options = target_options.clone();
            let mut name = qrc.qrc_name.replace('-', "_");
            if !qrc.unique {
                name.push('_');
                name.push_str(&qrc.path_checksum);
            }
            rcc_merge_options(&mut options, &["-name".to_owned(), name], modern);
            rcc_merge_options(&mut options, &qrc.options, modern);
            qrc.options = options;
        }
        for qrc in qrcs.iter_mut().filter(|qrc| !qrc.generated) {
            let mut error = String::new();
            let list = |executable: &str, options: &[String], error: &mut String| {
                let lister = RccLister {
                    executable: executable.to_owned(),
                    list_options: options.to_vec(),
                };
                let mut files = Vec::new();
                lister
                    .list(&qrc.qrc_file, &mut files, error, false)
                    .then_some(files)
            };
            let tool = &self.rcc.tool;
            if per_config {
                for config in &self.configs {
                    let files = list(
                        tool.executable.get(config).map_or("", String::as_str),
                        tool.list_options.get(config).map_or(&[][..], Vec::as_slice),
                        &mut error,
                    );
                    let Some(files) = files else {
                        ctx.system_error(error);
                        return false;
                    };
                    qrc.resources.config.push((config.clone(), files));
                }
            } else {
                let files = list(
                    &tool.executable.default,
                    &tool.list_options.default,
                    &mut error,
                );
                let Some(files) = files else {
                    ctx.system_error(error);
                    return false;
                };
                qrc.resources.default = files;
            }
        }
        self.rcc.qrcs = qrcs;
        true
    }

    /// `AUTOGEN_ORIGIN_DEPENDS`: link library targets found in every
    /// configuration.
    fn add_origin_depends(&mut self, ctx: &GenCtx, target: &Target) {
        let mut counts: BTreeMap<String, usize> = BTreeMap::new();
        for config in &self.configs {
            let mut seen = HashSet::new();
            for dep in super::link::implementation_library_targets(ctx, target, config) {
                if !static_library_cycle(ctx, target, dep, config) && seen.insert(dep.name.clone())
                {
                    *counts.entry(dep.name.clone()).or_default() += 1;
                }
            }
        }
        let all = self.configs.len();
        self.depend_targets.extend(
            counts
                .into_iter()
                .filter(|(_, count)| *count == all)
                .map(|(name, _)| name),
        );
    }

    /// The mutating part of `InitCustomTargets`.
    fn apply(
        &mut self,
        interp: &mut Interp,
        global_autogen: &BTreeMap<usize, String>,
        global_autorcc: &BTreeMap<usize, String>,
    ) {
        let dir = self.dir;
        let backtrace = directory_backtrace(interp, dir);
        if let Some(options) = &self.rcc_no_zstd {
            let target = interp.ext.targets.get_mut(&self.key).unwrap();
            target.props.set("AUTORCC_OPTIONS", options.clone());
        }
        for header in &self.created_headers {
            source_index(interp, dir, header, true);
        }
        for file in &self.clean_files {
            let target = interp.ext.targets.get_mut(&self.key).unwrap();
            target.props.append("ADDITIONAL_CLEAN_FILES", file, false);
        }
        // Add autogen include directory to the origin target.
        if self.moc_or_uic() || (self.rcc_enabled && self.multi) {
            let system_policy = interp.policy_status_at(dir, "CMP0151") == PolicyStatus::New;
            let configs = self.configs.clone();
            let target = interp.ext.targets.get_mut(&self.key).unwrap();
            let system = target
                .props
                .get("AUTOGEN_USE_SYSTEM_INCLUDE")
                .map_or(system_policy, is_on);
            if system {
                for config in &configs {
                    let path = if config.is_empty() {
                        self.include_genex.clone()
                    } else {
                        self.include_genex.replace("$<CONFIG>", config)
                    };
                    target.system_include_directories.insert(path);
                }
            }
            target.insert_entry(
                "INCLUDE_DIRECTORIES",
                &self.include_genex,
                &backtrace,
                !system,
            );
        }
        if self.moc_or_uic() {
            self.add_autogen_target(interp, global_autogen);
        }
        if self.rcc_enabled {
            self.add_rcc_targets(interp, global_autorcc);
        }
    }

    /// `RegisterGeneratedSource`.
    fn register_generated(&self, interp: &mut Interp, path: &str) {
        let index = source_index(interp, self.dir, path, true);
        crate::commands::target::mark_source_generated(interp, self.dir, index);
        let source = get_or_create_source(interp, self.dir, path);
        for (name, value) in [
            ("SKIP_AUTOGEN", "1"),
            ("SKIP_LINTING", "ON"),
            ("CXX_SCAN_FOR_MODULES", "0"),
        ] {
            source.props.set(name, value.to_owned());
        }
    }

    /// `AddGeneratedSource(std::string const&, ...)`.
    fn add_generated(&self, interp: &mut Interp, path: &str, upper: &str, prepend: bool) {
        // cmMakefile::GetOrCreateSource does not track expressions.
        self.register_generated(interp, path);
        let backtrace = directory_backtrace(interp, self.dir);
        let target = interp.ext.targets.get_mut(&self.key).unwrap();
        target.insert_entry("SOURCES", path, &backtrace, prepend);
        self.add_to_source_group(interp, path, upper);
    }

    /// `AddGeneratedSource(ConfigString const&, ...)`.
    fn add_generated_configs(
        &self,
        interp: &mut Interp,
        names: &Cfg<String>,
        upper: &str,
        prepend: bool,
    ) {
        let skip_pch = !self
            .sources
            .iter()
            .any(|file| !file.generated && !file.skip_pch);
        let handle_skip_pch = |interp: &mut Interp, path: &str| {
            if skip_pch {
                get_or_create_source(interp, self.dir, path)
                    .props
                    .set("SKIP_PRECOMPILE_HEADERS", "ON".to_owned());
            }
        };
        if !self.multi {
            self.add_generated(interp, &names.default, upper, prepend);
            handle_skip_pch(interp, &names.default);
            return;
        }
        let backtrace = directory_backtrace(interp, self.dir);
        for config in &self.configs {
            let path = names.get(config).cloned().unwrap_or_default();
            self.register_generated(interp, &path);
            handle_skip_pch(interp, &path);
            let target = interp.ext.targets.get_mut(&self.key).unwrap();
            target.insert_entry(
                "SOURCES",
                &format!("$<$<CONFIG:{config}>:{path}>"),
                &backtrace,
                prepend,
            );
            self.add_to_source_group(interp, &path, upper);
        }
    }

    /// `AddToSourceGroup`.
    fn add_to_source_group(&self, interp: &mut Interp, path: &str, upper: &str) {
        let group = [
            format!("{upper}_SOURCE_GROUP"),
            "AUTOGEN_SOURCE_GROUP".to_owned(),
        ]
        .iter()
        .find_map(|name| {
            interp
                .global_props
                .get(name)
                .filter(|value| !value.is_empty())
                .map(str::to_owned)
        });
        let Some(group) = group else {
            return;
        };
        // cmMakefile::GetOrCreateSourceGroup(std::string const&).
        let delimiters = {
            let ctx = GenCtx::new(interp);
            ctx.var(self.dir, "SOURCE_GROUP_DELIMITER")
        };
        let name = crate::commands::directory::normalized_group(
            &group,
            delimiters.as_deref().unwrap_or("/\\"),
        );
        crate::state::source_group_entry(
            interp.ext.source_groups.entry(self.dir).or_default(),
            name,
        )
        .files
        .insert(path.to_owned());
    }

    /// `AddCMakeProcessToCommandLines`.
    fn process_commands(&self, interp: &Interp, info: &str, process: &str) -> Vec<Vec<String>> {
        let line = |args: &[&str]| -> Vec<String> {
            let mut line = vec![
                interp.cmake_command.clone(),
                "-E".to_owned(),
                process.to_owned(),
                info.to_owned(),
            ];
            line.extend(args.iter().map(|arg| (*arg).to_owned()));
            line
        };
        if self.cross && self.better_graph {
            vec![line(&["$<CONFIG>", "$<COMMAND_CONFIG:$<CONFIG>>"])]
        } else if self.cross {
            // GetQtAutoGenConfigs: every configuration under Ninja
            // Multi-Config, `$<CONFIG>` otherwise.
            let configs = if process == "cmake_autorcc" || self.multi {
                self.configs.clone()
            } else {
                vec!["$<CONFIG>".to_owned()]
            };
            configs.iter().map(|config| line(&[config])).collect()
        } else {
            vec![line(&["$<CONFIG>"])]
        }
    }

    /// `AddAutogenExecutableToDependencies`.
    fn executable_depends(&self, tool: &Tool, depends: &mut Vec<String>) {
        if let Some(target) = &tool.target {
            depends.push(target.clone());
        } else if self.multi && self.better_graph {
            let (begin, end) = if self.cross {
                ("$<COMMAND_CONFIG:$<$<CONFIG:", ">>")
            } else {
                ("$<$<CONFIG:", ">")
            };
            for (config, executable) in tool.executable.unordered() {
                depends.push(format!("{begin}{config}>:{executable}{end}"));
            }
        } else if !tool.executable.default.is_empty() {
            depends.push(tool.executable.default.clone());
        }
    }

    /// `InitAutogenTarget` (Visual Studio's PRE_BUILD variant aside).
    fn add_autogen_target(
        &mut self,
        interp: &mut Interp,
        global_autogen: &BTreeMap<usize, String>,
    ) {
        let dir = self.dir;
        interp.makefiles[dir]
            .output_files
            .push(self.info_file.clone());
        let use_depfile = self.qt
            >= (IntegerVersion {
                major: 5,
                minor: 15,
            });
        let mut autogen_byproducts = Vec::new();
        let mut timestamp_byproducts = Vec::new();
        if self.moc_enabled {
            let names = self.moc.compilation_file.clone();
            self.add_generated_configs(interp, &names, "AUTOMOC", true);
            if use_depfile {
                if self.cross && !self.better_graph {
                    // The timestamp command runs cmake_autogen for every
                    // configuration, so all their files are its byproducts.
                    timestamp_byproducts
                        .extend(names.unordered().into_iter().map(|(_, file)| file));
                } else {
                    timestamp_byproducts.push(self.moc.compilation_genex.clone());
                }
            } else {
                autogen_byproducts.push(self.moc.compilation_genex.clone());
            }
        }
        if self.uic_enabled {
            for (names, genex) in self.uic.headers.clone() {
                self.add_generated_configs(interp, &names, "AUTOUIC", false);
                autogen_byproducts.push(genex);
            }
        }
        let tools = match (self.moc_enabled, self.uic_enabled) {
            (true, true) => "MOC and UIC",
            (true, false) => "MOC",
            _ => "UIC",
        };
        let mut comment = format!("Automatic {tools} for target {}", self.name);
        let mut commands = self.process_commands(interp, &self.info_file, "cmake_autogen");
        let mut depends: Vec<String> = self.depend_files.iter().cloned().collect();
        let folder = (!self.folder.is_empty()).then(|| self.folder.clone());
        let mut order_target = self.target_name.clone();

        if use_depfile {
            // '_autogen_timestamp_deps' carries the dependencies that must
            // not rerun the timestamp command (order-only for it).
            let deps_target = format!("{}_autogen_timestamp_deps", self.name);
            add_utility(
                interp,
                &deps_target,
                folder.as_deref(),
                CustomCommand {
                    working_directory: self.dir_work.clone(),
                    depends: std::mem::take(&mut depends),
                    ..generator_command(dir, CustomCommandKind::Utility, true)
                },
            );
            depends.push(deps_target.clone());
            order_target = deps_target;
            if self.moc_enabled {
                self.executable_depends(&self.moc.tool, &mut depends);
            }
            if self.uic_enabled {
                self.executable_depends(&self.uic.tool, &mut depends);
            }
            let output;
            let dep_file;
            let touch = |output: &str| {
                vec![
                    interp.cmake_command.clone(),
                    "-E".to_owned(),
                    "touch".to_owned(),
                    output.to_owned(),
                ]
            };
            if self.multi && self.better_graph {
                output = format!("{}/timestamp_$<CONFIG>", self.dir_build);
                dep_file = format!("{}/deps_$<CONFIG>", self.dir_build);
                commands.push(touch(&output));
                let mut timestamps = Cfg::default();
                for config in &self.configs {
                    let timestamp = format!("timestamp_{config}");
                    self.dep_file_rule.config.push((
                        config.clone(),
                        format!("{}/{timestamp}", self.dir_relative_build),
                    ));
                    self.dep_file
                        .config
                        .push((config.clone(), format!("{}/deps_{config}", self.dir_build)));
                    timestamps.config.push((config.clone(), timestamp));
                }
                self.add_generated_configs(interp, &timestamps, "AUTOMOC", false);
            } else {
                output = format!("{}/timestamp", self.dir_build);
                dep_file = format!("{}/deps", self.dir_build);
                self.dep_file.default.clone_from(&dep_file);
                self.dep_file_rule.default = format!("{}/timestamp", self.dir_relative_build);
                commands.push(touch(&output));
                self.add_generated(interp, &output, "AUTOMOC", false);
            }
            add_output_command(
                interp,
                CustomCommand {
                    outputs: vec![output.clone()],
                    byproducts: timestamp_byproducts,
                    depends: std::mem::take(&mut depends),
                    command_lines: std::mem::take(&mut commands),
                    comment: Some(std::mem::take(&mut comment)),
                    working_directory: self.dir_work.clone(),
                    depfile: dep_file,
                    ..generator_command(dir, CustomCommandKind::Output, true)
                },
            );
            depends.push(output);
        }
        add_utility(
            interp,
            &self.target_name,
            folder.as_deref(),
            CustomCommand {
                working_directory: self.dir_work.clone(),
                byproducts: autogen_byproducts,
                depends,
                command_lines: commands,
                comment: Some(comment),
                ..generator_command(dir, CustomCommandKind::Utility, true)
            },
        );

        // Order the autogen target(s) just before the original target.
        let backtrace = directory_backtrace(interp, dir);
        let mut utilities: BTreeSet<String> = self.depend_targets.clone();
        if self.depend_origin {
            let origin = &interp.ext.targets[&self.key];
            utilities.extend(origin.dependencies.keys().cloned());
            utilities.extend(origin.traced_utilities.iter().map(|(name, _)| name.clone()));
        }
        for name in utilities {
            add_dependency(interp, dir, &order_target, &name, &backtrace);
        }
        let target = interp.ext.targets.get_mut(&self.key).unwrap();
        target.add_dependency(&self.target_name, &backtrace);
        if let Some(global) = global_autogen.get(&dir) {
            add_dependency(interp, dir, global, &self.target_name, &backtrace);
        }
    }

    /// `InitRccTargets`.
    fn add_rcc_targets(&mut self, interp: &mut Interp, global_autorcc: &BTreeMap<usize, String>) {
        let dir = self.dir;
        let backtrace = directory_backtrace(interp, dir);
        let folder = (!self.folder.is_empty()).then(|| self.folder.clone());
        for qrc in self.rcc.qrcs.clone() {
            interp.makefiles[dir]
                .output_files
                .push(qrc.info_file.clone());
            self.add_generated(interp, &qrc.output_file, "AUTORCC", false);
            get_or_create_source(interp, dir, &qrc.output_file)
                .props
                .set("SKIP_UNITY_BUILD_INCLUSION", "On".to_owned());
            let commands = self.process_commands(interp, &qrc.info_file, "cmake_autorcc");
            let mut depends = vec![qrc.qrc_file.clone(), qrc.info_file.clone()];
            let comment = format!(
                "Automatic RCC for {}",
                project_relative_path(interp, dir, &qrc.qrc_file)
            );
            if qrc.generated || global_autorcc.contains_key(&dir) {
                let mut name = format!("{}_arcc_{}", self.name, qrc.qrc_name);
                if !qrc.unique {
                    name.push('_');
                    name.push_str(&qrc.path_checksum);
                }
                add_utility(
                    interp,
                    &name,
                    folder.as_deref(),
                    CustomCommand {
                        working_directory: self.dir_work.clone(),
                        command_lines: commands,
                        comment: Some(comment),
                        byproducts: vec![qrc.output_file.clone()],
                        depends,
                        ..generator_command(dir, CustomCommandKind::Utility, true)
                    },
                );
                if !self.rcc.tool.target_name.is_empty() {
                    add_dependency(interp, dir, &name, &self.rcc.tool.target_name, &backtrace);
                }
                let target = interp.ext.targets.get_mut(&self.key).unwrap();
                target.add_dependency(&name, &backtrace);
                if let Some(global) = global_autorcc.get(&dir) {
                    add_dependency(interp, dir, global, &name, &backtrace);
                }
            } else {
                if self.multi && self.better_graph {
                    for (config, files) in &qrc.resources.config {
                        depends.push(format!("$<$<CONFIG:{config}>:{}>", files.join(";")));
                    }
                } else {
                    depends.extend(qrc.resources.default.iter().cloned());
                }
                if !self.rcc.tool.target_name.is_empty() {
                    depends.push(self.rcc.tool.target_name.clone());
                }
                self.executable_depends(&self.rcc.tool, &mut depends);
                // SetEscapeOldStyle(false) is not requested here.
                add_output_command(
                    interp,
                    CustomCommand {
                        outputs: vec![qrc.output_file.clone()],
                        working_directory: self.dir_work.clone(),
                        command_lines: commands,
                        comment: Some(comment),
                        depends,
                        ..generator_command(dir, CustomCommandKind::Output, false)
                    },
                );
                // Reconfigure when the .qrc file changes.
                interp.makefiles[dir].list_files.push(qrc.qrc_file.clone());
            }
        }
    }

    /// `GetMocBuildPath`.
    fn moc_build_path(&self, ctx: &GenCtx, file: &MuFile, emitted: &mut HashSet<String>) -> String {
        if !file.moc_it {
            return String::new();
        }
        let mut base = format!(
            "{}/moc_{}",
            path_checksum(ctx.interp, self.dir, &file.full_path),
            filename_without_last_extension(&file.full_path)
        );
        let mut result = format!("{base}.cpp");
        if emitted.insert(result.clone()) {
            return result;
        }
        base = format!(
            "{base}_{}",
            filename_last_extension(&file.full_path).trim_start_matches('.')
        );
        result = format!("{base}.cpp");
        if emitted.insert(result.clone()) {
            return result;
        }
        for number in 2..256 {
            result = format!("{base}_{number}.cpp");
            if emitted.insert(result.clone()) {
                return result;
            }
        }
        ctx.system_error(format!(
            "moc output file name conflict for {}",
            file.full_path
        ));
        result
    }

    /// `SetupWriteAutogenInfo`.
    fn write_autogen_info(&self, ctx: &GenCtx) {
        let target = &ctx.interp.ext.targets[&self.key];
        let dir = self.dir;
        let def = |name: &str| ctx.var_safe(dir, name);
        let (mut moc_skip, mut uic_skip) = (BTreeSet::new(), BTreeSet::new());
        let mut headers = Vec::new();
        for file in &self.headers {
            if file.skip_moc {
                moc_skip.insert(file.full_path.clone());
            }
            if file.skip_uic {
                uic_skip.insert(file.full_path.clone());
            }
            if (!file.generated || self.cmp0071 == Handling::Accept) && (file.moc_it || file.uic_it)
            {
                headers.push(file);
            }
        }
        headers.sort_by(|a, b| a.full_path.cmp(&b.full_path));
        let mut sources = Vec::new();
        for file in &self.sources {
            if file.generated && self.cmp0071 != Handling::Accept {
                continue;
            }
            if file.skip_moc {
                moc_skip.insert(file.full_path.clone());
            }
            if file.skip_uic {
                uic_skip.insert(file.full_path.clone());
            }
            if file.moc_it || file.uic_it {
                sources.push(file);
            }
        }
        sources.sort_by(|a, b| a.full_path.cmp(&b.full_path));

        let mut info = serde_json::Map::new();
        let set_config =
            |info: &mut serde_json::Map<String, Value>, key: &str, value: &Cfg<String>| {
                info.insert(key.to_owned(), json!(value.default));
                for (config, value) in &value.config {
                    info.insert(format!("{key}_{config}"), json!(value));
                }
            };
        info.insert("MULTI_CONFIG".into(), json!(self.multi));
        info.insert("CROSS_CONFIG".into(), json!(self.cross));
        info.insert("USE_BETTER_GRAPH".into(), json!(self.better_graph));
        info.insert("PARALLEL".into(), json!(self.parallel));
        info.insert("VERBOSITY".into(), json!(self.verbosity));
        for name in [
            "CMAKE_SOURCE_DIR",
            "CMAKE_BINARY_DIR",
            "CMAKE_CURRENT_SOURCE_DIR",
            "CMAKE_CURRENT_BINARY_DIR",
        ] {
            info.insert(name.into(), json!(def(name)));
        }
        info.insert("BUILD_DIR".into(), json!(self.dir_build));
        set_config(&mut info, "INCLUDE_DIR", &self.dir_include);
        info.insert("QT_VERSION_MAJOR".into(), json!(self.qt.major));
        info.insert("QT_VERSION_MINOR".into(), json!(self.qt.minor));
        set_config(&mut info, "QT_MOC_EXECUTABLE", &self.moc.tool.executable);
        set_config(&mut info, "QT_UIC_EXECUTABLE", &self.uic.tool.executable);
        info.insert("CMAKE_EXECUTABLE".into(), json!(ctx.interp.cmake_command));
        set_config(&mut info, "SETTINGS_FILE", &self.settings_file);
        set_config(&mut info, "PARSE_CACHE_FILE", &self.parse_cache_file);
        set_config(&mut info, "DEP_FILE", &self.dep_file);
        set_config(&mut info, "DEP_FILE_RULE_NAME", &self.dep_file_rule);
        info.insert(
            "CMAKE_LIST_FILES".into(),
            json!(ctx.interp.makefiles[dir].list_files),
        );
        info.insert("HEADER_EXTENSIONS".into(), json!(HEADER_EXTENSIONS));
        let configs = |file: &MuFile| -> Value {
            if file.configs.is_empty() {
                Value::Null
            } else {
                json!(
                    file.configs
                        .iter()
                        .map(|ci| &self.configs[*ci])
                        .collect::<Vec<_>>()
                )
            }
        };
        let flags = |file: &MuFile| {
            format!(
                "{}{}",
                if file.moc_it { 'M' } else { 'm' },
                if file.uic_it { 'U' } else { 'u' }
            )
        };
        let mut emitted = HashSet::new();
        let headers: Vec<Value> = headers
            .into_iter()
            .map(|file| {
                json!([
                    file.full_path,
                    flags(file),
                    self.moc_build_path(ctx, file, &mut emitted),
                    configs(file)
                ])
            })
            .collect();
        info.insert("HEADERS".into(), Value::Array(headers));
        let sources: Vec<Value> = sources
            .into_iter()
            .map(|file| json!([file.full_path, flags(file), configs(file)]))
            .collect();
        info.insert("SOURCES".into(), Value::Array(sources));

        if self.moc_enabled {
            info.insert("MOC_SKIP".into(), json!(moc_skip));
            set_config_array(&mut info, "MOC_DEFINITIONS", &self.moc.defines);
            set_config_array(&mut info, "MOC_INCLUDES", &self.moc.includes);
            info.insert("MOC_OPTIONS".into(), json!(self.moc.options));
            info.insert("MOC_RELAXED_MODE".into(), json!(self.moc.relaxed));
            info.insert("MOC_PATH_PREFIX".into(), json!(self.moc.path_prefix));
            // AddInterfaceEntries(INTERFACE_AUTOMOC_MACRO_NAMES).
            let mut names = self.moc.macro_names.clone();
            for config in self.evaluations() {
                if self.multi && config.is_empty() {
                    continue;
                }
                names.extend(super::flags::interface_values(
                    ctx,
                    target,
                    &config,
                    "AUTOMOC_MACRO_NAMES",
                    "CXX",
                ));
            }
            info.insert("MOC_MACRO_NAMES".into(), json!(unique(names)));
            info.insert(
                "MOC_DEPEND_FILTERS".into(),
                json!(
                    self.moc
                        .depend_filters
                        .iter()
                        .map(|(key, filter)| [key, filter])
                        .collect::<Vec<_>>()
                ),
            );
            set_config(
                &mut info,
                "MOC_COMPILATION_FILE",
                &self.moc.compilation_file,
            );
            set_config(&mut info, "MOC_PREDEFS_FILE", &self.moc.predefs_file);
            let mut predefs = self.moc.predefs_cmd.clone();
            let option = super::flags::standard_option(ctx, target, "CXX");
            if !option.is_empty() && predefs.len() >= 3 {
                predefs.insert(1, option);
            }
            info.insert("MOC_PREDEFS_CMD".into(), json!(predefs));
        }
        if self.uic_enabled {
            uic_skip.extend(self.uic.skip_ui.iter().cloned());
            info.insert("UIC_SKIP".into(), json!(uic_skip));
            info.insert(
                "UIC_UI_FILES".into(),
                Value::Array(
                    self.uic
                        .files_with_options
                        .iter()
                        .map(|(file, options)| json!([file, options]))
                        .collect(),
                ),
            );
            set_config_array(&mut info, "UIC_OPTIONS", &self.uic.options);
            info.insert("UIC_SEARCH_PATHS".into(), json!(self.uic.search_paths));
        }
        // SetupWriteAutogenInfo ignores the result of Save.
        let _ = save(&self.info_file, &Value::Object(info));
    }

    /// `SetupWriteRccInfo`.
    fn write_rcc_info(&self, ctx: &GenCtx) {
        let def = |name: &str| ctx.var_safe(self.dir, name);
        for qrc in &self.rcc.qrcs {
            let mut info = serde_json::Map::new();
            let set_config =
                |info: &mut serde_json::Map<String, Value>, key: &str, value: &Cfg<String>| {
                    info.insert(key.to_owned(), json!(value.default));
                    for (config, value) in &value.config {
                        info.insert(format!("{key}_{config}"), json!(value));
                    }
                };
            info.insert("MULTI_CONFIG".into(), json!(self.multi));
            info.insert("CROSS_CONFIG".into(), json!(self.cross));
            info.insert("USE_BETTER_GRAPH".into(), json!(self.better_graph));
            info.insert("VERBOSITY".into(), json!(self.verbosity));
            info.insert(
                "GENERATOR".into(),
                json!(if self.multi {
                    "Ninja Multi-Config"
                } else {
                    "Ninja"
                }),
            );
            info.insert("LOCK_FILE".into(), json!(qrc.lock_file));
            set_config(&mut info, "SETTINGS_FILE", &qrc.settings_file);
            for name in [
                "CMAKE_SOURCE_DIR",
                "CMAKE_BINARY_DIR",
                "CMAKE_CURRENT_SOURCE_DIR",
                "CMAKE_CURRENT_BINARY_DIR",
            ] {
                info.insert(name.into(), json!(def(name)));
            }
            info.insert("BUILD_DIR".into(), json!(self.dir_build));
            set_config(&mut info, "INCLUDE_DIR", &self.dir_include);
            set_config(&mut info, "RCC_EXECUTABLE", &self.rcc.tool.executable);
            // generateListOptions selects on MultiConfig alone.
            let options = Cfg {
                default: if self.multi {
                    Vec::new()
                } else {
                    self.rcc.tool.list_options.default.clone()
                },
                config: if self.multi {
                    self.rcc.tool.list_options.config.clone()
                } else {
                    Vec::new()
                },
            };
            set_config_array(&mut info, "RCC_LIST_OPTIONS", &options);
            info.insert("SOURCE".into(), json!(qrc.qrc_file));
            info.insert("OUTPUT_CHECKSUM".into(), json!(qrc.path_checksum));
            info.insert("OUTPUT_NAME".into(), json!(filename_name(&qrc.output_file)));
            info.insert("OPTIONS".into(), json!(qrc.options));
            set_config_array(&mut info, "INPUTS", &qrc.resources);
            let _ = save(&qrc.info_file, &Value::Object(info));
        }
    }
}

/// `cmQtAutoGenGlobalInitializer::SetupCustomTargets`.
pub fn write_info_files(ctx: &GenCtx, initializers: &[Initializer]) -> bool {
    for init in initializers {
        if std::fs::create_dir_all(&init.dir_info).is_err() {
            ctx.system_error(format!(
                "AutoGen: Could not create directory: {}",
                quoted(&init.dir_info)
            ));
            return false;
        }
        if init.moc_or_uic() {
            init.write_autogen_info(ctx);
        }
        if init.rcc_enabled {
            init.write_rcc_info(ctx);
        }
    }
    true
}

fn set_config_array(
    info: &mut serde_json::Map<String, Value>,
    key: &str,
    value: &Cfg<Vec<String>>,
) {
    info.insert(key.to_owned(), json!(value.default));
    for (config, value) in &value.config {
        info.insert(format!("{key}_{config}"), json!(value));
    }
}

/// `target->AddUtility(name, false, makefile)` for a target named in `dir`.
fn add_dependency(
    interp: &mut Interp,
    dir: usize,
    target: &str,
    name: &str,
    backtrace: &[crate::messages::Frame],
) {
    if let Some(key) = crate::state::find_target_key_at(interp, dir, target)
        && let Some(target) = interp.ext.targets.get_mut(&key)
    {
        target.add_dependency(name, backtrace);
    }
}

/// `cmRemoveDuplicates`.
fn unique(values: Vec<String>) -> Vec<String> {
    let mut seen = HashSet::new();
    values
        .into_iter()
        .filter(|value| seen.insert(value.clone()))
        .collect()
}

/// `SearchPathSanitizer`.
fn sanitize(ctx: &GenCtx, dir: usize, paths: Vec<String>) -> Vec<String> {
    let source = &ctx.interp.makefiles[dir].source_dir;
    paths
        .iter()
        .map(|path| {
            collapse_full_path(path, source)
                .trim_end_matches('/')
                .to_owned()
        })
        .filter(|path| !path.is_empty())
        .collect()
}

/// `cmSourceFile::GetIsGenerated()` (global and local scope).
fn source_generated(interp: &Interp, dir: usize, index: usize, path: &str) -> bool {
    let source = &interp.ext.sources[&dir][index];
    interp.ext.generated_sources.contains(path)
        || generated_for_user(interp, dir, source.generated, source.props.get("GENERATED")) == "1"
}

/// `StaticLibraryCycle`.
fn static_library_cycle(ctx: &GenCtx, origin: &Target, depend: &Target, config: &str) -> bool {
    if origin.kind != TargetKind::StaticLibrary || depend.kind != TargetKind::StaticLibrary {
        return false;
    }
    let mut known: HashSet<*const Target> = HashSet::from([std::ptr::from_ref(depend)]);
    let mut pending = std::collections::VecDeque::from([depend]);
    while let Some(test) = pending.pop_front() {
        if std::ptr::eq(test, origin) {
            return true;
        }
        for dep in super::link::implementation_library_targets(ctx, test, config) {
            if dep.kind == TargetKind::StaticLibrary && known.insert(std::ptr::from_ref(dep)) {
                pending.push_back(dep);
            }
        }
    }
    false
}

/// `InfoWriter::Save`.
fn save(path: &str, value: &Value) -> std::io::Result<()> {
    let mut writer = StyledStreamWriter {
        out: String::new(),
        indent: String::new(),
        indented: true,
    };
    writer.write(value);
    writer.out.push('\n');
    crate::fsutil::write_if_different(Path::new(path), &writer.out).map(|_| ())
}

/// `Json::StyledStreamWriter` with its default tab indentation.
struct StyledStreamWriter {
    out: String,
    indent: String,
    indented: bool,
}

impl StyledStreamWriter {
    /// jsoncpp's right margin.
    const MARGIN: usize = 74;

    fn scalar(value: &Value) -> Option<String> {
        Some(match value {
            Value::Null => "null".to_owned(),
            Value::Bool(value) => value.to_string(),
            Value::Number(value) => value.to_string(),
            Value::String(value) => {
                let mut out = String::new();
                crate::debugger::J::quote(value, &mut out);
                out
            }
            Value::Array(items) if items.is_empty() => "[]".to_owned(),
            Value::Object(items) if items.is_empty() => "{}".to_owned(),
            Value::Array(_) | Value::Object(_) => return None,
        })
    }

    fn write_indent(&mut self) {
        self.out.push('\n');
        self.out.push_str(&self.indent);
    }

    fn write_with_indent(&mut self, text: &str) {
        if !self.indented {
            self.write_indent();
        }
        self.out.push_str(text);
        self.indented = false;
    }

    fn write(&mut self, value: &Value) {
        if let Some(text) = Self::scalar(value) {
            self.out.push_str(&text);
            return;
        }
        match value {
            Value::Array(items) => self.write_array(items),
            Value::Object(fields) => {
                let fields: BTreeMap<_, _> = fields.iter().collect();
                self.write_with_indent("{");
                self.indent.push('\t');
                let count = fields.len();
                for (position, (name, value)) in fields.into_iter().enumerate() {
                    let mut text = String::new();
                    crate::debugger::J::quote(name, &mut text);
                    self.write_with_indent(&text);
                    self.out.push_str(" : ");
                    self.write(value);
                    if position + 1 < count {
                        self.out.push(',');
                    }
                    // writeCommentAfterValueOnSameLine
                    self.indented = false;
                }
                self.indent.pop();
                self.write_with_indent("}");
            }
            _ => {}
        }
    }

    fn write_array(&mut self, items: &[Value]) {
        let children: Option<Vec<String>> = if items.len() * 3 >= Self::MARGIN {
            None
        } else {
            items.iter().map(Self::scalar).collect()
        };
        if let Some(children) = &children {
            let length =
                4 + (items.len() - 1) * 2 + children.iter().map(String::len).sum::<usize>();
            if length < Self::MARGIN {
                self.out.push_str("[ ");
                self.out.push_str(&children.join(", "));
                self.out.push_str(" ]");
                return;
            }
        }
        self.write_with_indent("[");
        self.indent.push('\t');
        for (position, item) in items.iter().enumerate() {
            if let Some(children) = &children {
                self.write_with_indent(&children[position]);
            } else {
                if !self.indented {
                    self.write_indent();
                }
                self.indented = true;
                self.write(item);
                self.indented = false;
            }
            if position + 1 < items.len() {
                self.out.push(',');
            }
            self.indented = false;
        }
        self.indent.pop();
        self.write_with_indent("]");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn moc_versions() {
        assert!(
            parse_moc_version("moc 5.15.2\n")
                == IntegerVersion {
                    major: 5,
                    minor: 15
                }
        );
        assert!(parse_moc_version("moc-qt5 5.12.8") == IntegerVersion::default());
        assert!(parse_moc_version("nothing") == IntegerVersion::default());
    }
}
