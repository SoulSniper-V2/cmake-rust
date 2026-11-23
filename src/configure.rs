//! Project configuration pipeline (port of `cmake::Run` / `cmake::Configure`
//! / `cmGlobalGenerator::Configure` / `cmMakefile::Configure` for the root
//! directory). The CLI builds a `ConfigureOptions` and calls `configure`.

use std::time::Instant;

use crate::cache::CacheType;
use crate::interp::{Interp, Mode};
use crate::messages::{Frame, MessageType};
use crate::util;

/// One `-D` cache definition from the command line.
#[derive(Clone, Debug)]
pub struct CacheDefinition {
    pub name: String,
    pub value: String,
    /// None when the type was not given (UNINITIALIZED).
    pub kind: Option<CacheType>,
}

#[derive(Clone, Debug, Default)]
pub struct ConfigureOptions {
    pub source_dir: String,
    pub binary_dir: String,
    pub project_file: Option<String>,
    pub generator: Option<String>,
    pub toolset: Option<String>,
    pub platform: Option<String>,
    pub toolchain_file: Option<String>,
    pub install_prefix: Option<String>,
    pub definitions: Vec<CacheDefinition>,
    /// `-U` glob patterns.
    pub undefine: Vec<String>,
    /// `-C <initial-cache>` scripts.
    pub preload_scripts: Vec<String>,
    /// The `-C` values as given, for "loading initial cache file".
    pub preload_script_args: Vec<String>,
    /// `cmake::PrintPresetVariables`/`PrintPresetEnvironment` output,
    /// printed after the cache arguments (with `-C`) are processed.
    pub preset_report: Option<String>,
    pub fresh: bool,
    pub log_level: Option<String>,
    pub log_context: bool,
    pub warn_dev: Option<bool>,
    pub warn_dev_error: Option<bool>,
    pub warn_deprecated: Option<bool>,
    pub warn_deprecated_error: Option<bool>,
    pub warn_uninitialized: bool,
    /// `--check-system-vars`.
    pub check_system_vars: bool,
    /// `--no-warn-unused-cli` (or a preset's "unusedCli": false).
    pub no_warn_unused_cli: bool,
    pub debug_trycompile: bool,
    /// `--debug-output`, `--debug-find`, `--debug-find-pkg`, `--debug-find-var`.
    pub debug: crate::interp::DebugOptions,
    /// `cmake::SetIgnoreCompileWarningAsError` / `SetIgnoreLinkWarningAsError`.
    pub ignore_compile_warning_as_error: bool,
    pub ignore_link_warning_as_error: bool,
    pub trace: bool,
    /// `--trace-expand` (cmake::TraceExpand).
    pub trace_expand: bool,
    /// `--trace-format=json-v1`.
    pub trace_json: bool,
    /// `--trace-source` files.
    pub trace_sources: Vec<String>,
    /// `--trace-redirect` file, already created.
    pub trace_redirect: Option<String>,
    /// `--explain <target> <flag>` request (cmake-rust extension).
    pub explain: Option<(String, String)>,
    /// Configure for a try_compile project (no progress output).
    pub in_try_compile: bool,
    /// `--graphviz=<file>` (or the preset's "graphviz" field).
    pub graphviz: Option<String>,
    /// `--sarif-output=<file>`, validated in the CLI.
    pub sarif_output: Option<String>,
    /// Full invocation, recorded when an instrumentation query is active.
    pub command_args: Vec<String>,
    /// A path argument named an existing build tree whose cache was read
    /// (`cmState::IsCacheLoaded` during argument processing).
    pub cache_loaded: bool,
}

impl ConfigureOptions {
    pub fn project_list_file(&self, dir: &str) -> String {
        project_list_file(dir, self.project_file.as_deref().unwrap_or_default())
    }
}

/// `cmake::GetCMakeListFile`: child directories fall back to CMakeLists.txt.
pub(crate) fn project_list_file(dir: &str, name: &str) -> String {
    if !name.is_empty() && name != "CMakeLists.txt" {
        let path = format!("{dir}/{name}");
        if std::path::Path::new(&path).is_file() {
            return path;
        }
    }
    format!("{dir}/CMakeLists.txt")
}

/// `cmSystemTools::Error`.
pub fn system_error(interp: Option<&mut Interp>, message: &str) {
    eprintln!("CMake Error: {message}");
    if let Some(interp) = interp {
        interp.error = true;
    }
}

/// Locates the CMake data root (Modules/, Templates/).
pub fn find_cmake_root() -> Option<String> {
    if let Ok(root) = std::env::var("CMAKE_RUST_DATA_ROOT")
        && std::path::Path::new(&root).join("Modules").is_dir()
    {
        return Some(root);
    }
    let exe = std::env::current_exe().ok()?.canonicalize().ok()?;
    let bin = exe.parent()?;
    let prefix = bin.parent()?;
    let candidate = prefix.join(format!(
        "share/cmake-{}.{}",
        crate::interp::VERSION.0,
        crate::interp::VERSION.1
    ));
    if candidate.join("Modules").is_dir() {
        return Some(candidate.to_string_lossy().into_owned());
    }
    None
}

/// `cmake::AddCMakePaths` plus the default definitions every cmMakefile
/// starts with.
pub fn add_cmake_paths(interp: &mut Interp) {
    let command = interp.cmake_command.clone();
    let directory = util::filename_path(&command);
    let root = interp.cmake_root.clone();
    let entries = [
        (
            "CMAKE_COMMAND",
            command.clone(),
            "Path to CMake executable.",
        ),
        (
            "CMAKE_CPACK_COMMAND",
            format!("{directory}/cpack"),
            "Path to cpack program executable.",
        ),
        (
            "CMAKE_CTEST_COMMAND",
            format!("{directory}/ctest"),
            "Path to ctest program executable.",
        ),
        ("CMAKE_ROOT", root, "Path to CMake installation."),
    ];
    for (name, value, help) in entries {
        interp
            .cache
            .add(name, Some(&value), Some(help), CacheType::Internal);
    }
}

pub fn add_default_definitions(interp: &mut Interp) {
    let (major, minor, patch) = crate::interp::VERSION;
    interp.set_def("CMAKE_MAJOR_VERSION", major.to_string());
    interp.set_def("CMAKE_MINOR_VERSION", minor.to_string());
    interp.set_def("CMAKE_PATCH_VERSION", patch.to_string());
    interp.set_def("CMAKE_TWEAK_VERSION", "0");
    interp.set_def("CMAKE_VERSION", crate::interp::VERSION_STRING);
    interp.set_def("CMAKE_FILES_DIRECTORY", "/CMakeFiles");
    interp.set_def("CMAKE_HOST_EXECUTABLE_SUFFIX", "");
    if cfg!(target_os = "macos") {
        interp.set_def("CMAKE_HOST_APPLE", "1");
        interp.set_def("APPLE", "1");
    } else {
        interp.set_def("CMAKE_HOST_LINUX", "1");
        interp.set_def("LINUX", "1");
    }
    interp.set_def("CMAKE_HOST_UNIX", "1");
    interp.set_def("UNIX", "1");
    interp.set_def(
        "CMAKE_HOST_SYSTEM_NAME",
        if cfg!(target_os = "macos") {
            "Darwin"
        } else {
            "Linux"
        },
    );
}

/// Applies `-D`/`-U` arguments (`cmake::SetCacheArgs`).
fn apply_cache_args(interp: &mut Interp, options: &ConfigureOptions) -> bool {
    if interp.cache.value("CMAKE_POLICY_VERSION_MINIMUM").is_none()
        && let Ok(value) = std::env::var("CMAKE_POLICY_VERSION_MINIMUM")
        && !value.is_empty()
    {
        interp.cache.add(
            "CMAKE_POLICY_VERSION_MINIMUM",
            Some(&value),
            Some("Override policy version for cmake_minimum_required calls."),
            CacheType::String,
        );
        if let Some(entry) = interp.cache.entries.get_mut("CMAKE_POLICY_VERSION_MINIMUM") {
            entry.properties.insert("ADVANCED".into(), "1".into());
        }
    }
    for definition in &options.definitions {
        // cmake::ProcessCacheArg
        let cached = interp
            .cache
            .get(&definition.name)
            .filter(|entry| entry.initialized)
            .map(|entry| entry.value.clone());
        interp.cache.add(
            &definition.name,
            Some(&definition.value),
            Some("No help, variable specified on the command line."),
            definition.kind.unwrap_or(CacheType::Uninitialized),
        );
        let mut vars = interp.cli_vars.borrow_mut();
        vars.remove(&definition.name);
        if interp.warn_unused_cli && cached.as_deref() != interp.cache.value(&definition.name) {
            vars.insert(definition.name.clone(), false);
        }
    }
    for pattern in &options.undefine {
        let regex = glob_to_regex(pattern);
        let names: Vec<String> = interp
            .cache
            .entries
            .iter()
            .filter(|(name, entry)| {
                entry.kind != CacheType::Static
                    && entry.kind != CacheType::Internal
                    && regex.find(name).is_some()
            })
            .map(|(name, _)| name.clone())
            .collect();
        for name in names {
            interp.cache.remove(&name);
        }
    }
    // cmake::ProcessCacheArg's unused-variable watch for a value it set.
    let watch = |interp: &mut Interp, name: &str, cached: Option<String>| {
        let mut vars = interp.cli_vars.borrow_mut();
        vars.remove(name);
        if interp.warn_unused_cli && cached.as_deref() != interp.cache.value(name) {
            vars.insert(name.to_owned(), false);
        }
    };
    let cached_value = |interp: &Interp, name: &str| {
        interp
            .cache
            .get(name)
            .filter(|entry| entry.initialized)
            .map(|entry| entry.value.clone())
    };
    // ToolchainLambda and PrefixLambda: cmake::ProcessCacheArg with the
    // value as given (the cache converts slashes for path types).
    if let Some(toolchain) = &options.toolchain_file {
        let cached = cached_value(interp, "CMAKE_TOOLCHAIN_FILE");
        interp.cache.add(
            "CMAKE_TOOLCHAIN_FILE",
            Some(toolchain),
            Some("No help, variable specified on the command line."),
            CacheType::Filepath,
        );
        watch(interp, "CMAKE_TOOLCHAIN_FILE", cached);
    }
    if let Some(prefix) = &options.install_prefix {
        let cached = cached_value(interp, "CMAKE_INSTALL_PREFIX");
        interp.cache.add(
            "CMAKE_INSTALL_PREFIX",
            Some(prefix),
            Some("No help, variable specified on the command line."),
            CacheType::Path,
        );
        watch(interp, "CMAKE_INSTALL_PREFIX", cached);
    }
    // Later cache additions (cmake::AddCacheEntry) stop watching a key.
    interp.cache.touched.clear();
    true
}

/// `cmake::ReadListFile`: a temporary makefile whose reset state keeps
/// only CMAKE_SOURCE_DIR and CMAKE_BINARY_DIR; normal variables it sets do
/// not reach the project.
fn read_temporary_list_file(interp: &mut Interp, path: &str) {
    interp.push_scope();
    let (source, binary) = (interp.home_source.clone(), interp.home_binary.clone());
    interp.set_def("CMAKE_SOURCE_DIR", source);
    interp.set_def("CMAKE_BINARY_DIR", binary);
    if !std::path::Path::new(path).is_file() {
        system_error(Some(interp), &format!("Not a file: {path}"));
    }
    if !interp.read_list_file(path) {
        system_error(Some(interp), &format!("Error processing file: {path}"));
    }
    interp.pop_scope();
}

/// `cmake::RunCheckForUnusedVariables`.
pub fn check_for_unused_variables(interp: &mut Interp) {
    if !interp.warn_unused_cli {
        return;
    }
    // cmake::LoadCache/SaveCache unwatch the cache format entries; normal
    // mode marks the SARIF variable as used.
    const UNWATCHED: [&str; 5] = [
        "CMAKE_CACHE_MAJOR_VERSION",
        "CMAKE_CACHE_MINOR_VERSION",
        "CMAKE_CACHE_PATCH_VERSION",
        "CMAKE_CACHEFILE_DIR",
        "CMAKE_EXPORT_SARIF",
    ];
    let unused: Vec<String> = interp
        .cli_vars
        .borrow()
        .iter()
        .filter(|(name, used)| {
            !**used && !interp.cache.touched.contains(*name) && !UNWATCHED.contains(&name.as_str())
        })
        .map(|(name, _)| name.clone())
        .collect();
    if !unused.is_empty() {
        let mut message = "Manually-specified variables were not used by the project:".to_owned();
        for name in unused {
            message += &format!("\n  {name}");
        }
        interp.issue_at(MessageType::Warning, &message, &[]);
    }
}

fn util_cwd() -> String {
    std::env::current_dir()
        .map(|dir| dir.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// `cmsys::Glob::PatternToRegex` (no recursion, anchored).
pub(crate) fn glob_to_regex(pattern: &str) -> crate::cmregex::Regex {
    let mut regex = String::from("^");
    for ch in pattern.chars() {
        match ch {
            '*' => regex.push_str(".*"),
            '?' => regex.push('.'),
            '.' | '+' | '^' | '$' | '(' | ')' | '|' | '\\' => {
                regex.push('\\');
                regex.push(ch);
            }
            other => regex.push(other),
        }
    }
    regex.push('$');
    crate::cmregex::Regex::new(&regex)
        .unwrap_or_else(|_| crate::cmregex::Regex::new("^$").expect("valid"))
}

/// Builds an interpreter for configuring `options`.
/// The cmake instance's trace settings (`SetTrace*`, `SetTraceFile`).
pub fn install_trace(interp: &mut Interp, options: &ConfigureOptions) {
    interp.warn_uninitialized = options.warn_uninitialized;
    interp.warn_unused_cli = !options.no_warn_unused_cli && !options.in_try_compile;
    interp.check_system_vars = options.check_system_vars;
    if !options.trace {
        return;
    }
    let file = options.trace_redirect.as_ref().and_then(|path| {
        std::fs::OpenOptions::new()
            .append(true)
            .open(path)
            .ok()
            .map(std::cell::RefCell::new)
    });
    interp.trace = Some(std::rc::Rc::new(crate::interp::TraceConfig {
        expand: options.trace_expand,
        json: options.trace_json,
        sources: options.trace_sources.clone(),
        file,
    }));
}

/// `cmake::PrintTraceFormatVersion`.
pub fn print_trace_format_version(options: &ConfigureOptions) {
    if !options.trace || !options.trace_json {
        return;
    }
    use std::io::Write;
    let header = "{\"version\":{\"major\":1,\"minor\":2}}";
    match &options.trace_redirect {
        Some(path) => {
            if let Ok(mut file) = std::fs::OpenOptions::new().append(true).open(path) {
                let _ = writeln!(file, "{header}");
            }
        }
        None => eprintln!("{header}"),
    }
}

pub fn new_project_interp(options: &ConfigureOptions, root: &str) -> Interp {
    let mut interp = Interp::new(
        Mode::Project,
        &options.source_dir,
        &options.binary_dir,
        root,
    );
    install_trace(&mut interp, options);
    interp.in_try_compile = options.in_try_compile;
    interp.debug_trycompile = options.debug_trycompile;
    if !options.in_try_compile {
        interp.debug = options.debug.clone();
        interp.ignore_compile_warning_as_error = options.ignore_compile_warning_as_error;
        interp.ignore_link_warning_as_error = options.ignore_link_warning_as_error;
    }
    add_default_definitions(&mut interp);
    if let Some(level) = &options.log_level {
        interp.ext.misc.insert("log-level".into(), level.clone());
    }
    if options.log_context {
        interp.ext.misc.insert("log-context".into(), "1".into());
    }
    interp
}

/// Runs configure and generate. Returns the process exit code.
/// `run_configure` returns this sentinel when
/// `__CMAKE_DELETE_CACHE_CHANGE_VARS_` was set during configure, in place of
/// `cmake::HandleDeleteCacheVariables` re-entering `cmake::Configure`.
pub const RESTART_CONFIGURE: i32 = -7;

pub fn configure(options: &ConfigureOptions) -> i32 {
    let Some(root) = find_cmake_root() else {
        eprintln!("CMake Error: Could not find CMAKE_ROOT !!!");
        return 1;
    };
    // cmake::Run keeps one SARIF results log (written when Run returns)
    // across the cmake::Configure restart of HandleDeleteCacheVariables.
    let mut sarif_log = crate::sarif::ResultsLog::default();
    loop {
        let mut interp = new_project_interp(options, &root);
        interp.sarif_log = std::mem::take(&mut sarif_log);
        let code = run_configure(&mut interp, options);
        if code == RESTART_CONFIGURE {
            handle_delete_cache_variables(&interp, options);
            sarif_log = std::mem::take(&mut interp.sarif_log);
            continue;
        }
        if let Some(path) = &options.sarif_output {
            interp.sarif_log.write(path, false);
        } else if util::is_on_opt(interp.cache.value("CMAKE_EXPORT_SARIF")) {
            let path = format!("{}/.cmake/sarif/cmake.sarif", options.binary_dir);
            interp.sarif_log.write(&path, true);
        }
        if code != 0 {
            return 1;
        }
        return 0;
    }
}

/// `cmake::HandleDeleteCacheVariables`: delete the cache, restore the
/// changed compiler entries, warn, and let the caller re-run configure.
fn handle_delete_cache_variables(interp: &Interp, options: &ConfigureOptions) {
    let vars = interp
        .global_props
        .get("__CMAKE_DELETE_CACHE_CHANGE_VARS_")
        .unwrap_or_default()
        .to_owned();
    let mut saved = Vec::new();
    let mut warning = String::from(
        "You have changed variables that require your cache to be deleted.\n\
         Configure will be re-run and you may have to reset some variables.\n\
         The following variables have changed:\n",
    );
    let parts: Vec<&str> = vars.split(';').collect();
    let mut index = 0;
    while index < parts.len() {
        let key = parts[index].to_owned();
        warning.push_str(&key);
        warning.push_str("= ");
        index += 1;
        let value = if index < parts.len() {
            let value = parts[index];
            warning.push_str(value);
            warning.push('\n');
            index += 1;
            value.to_owned()
        } else {
            warning.push('\n');
            String::new()
        };
        let (kind, help) = interp
            .cache
            .entries
            .get(&key)
            .map(|entry| {
                (
                    entry.kind.as_str().to_owned(),
                    entry.properties.get("HELPSTRING").cloned(),
                )
            })
            .unwrap_or_else(|| ("UNINITIALIZED".to_owned(), None));
        saved.push((key, value, kind, help));
    }
    // cmake::DeleteCache + LoadCache: drop the cache, then restore the
    // changed entries so the next configure sees them.
    let binary = &options.binary_dir;
    let _ = std::fs::remove_file(format!("{binary}/CMakeCache.txt"));
    let _ = std::fs::remove_dir_all(format!("{binary}/CMakeFiles"));
    let _ = std::fs::create_dir_all(binary);
    let mut text = String::new();
    for (key, value, kind, help) in saved {
        text.push_str(&format!(
            "//{}\n{key}:{kind}={value}\n\n",
            help.unwrap_or_default()
        ));
    }
    let _ = std::fs::write(format!("{binary}/CMakeCache.txt"), text);
    // `cmSystemTools::Message` terminates the output with a newline.
    eprintln!("{warning}");
}

/// `cmake::Run` for NORMAL_MODE after argument parsing.
pub fn run_configure(interp: &mut Interp, options: &ConfigureOptions) -> i32 {
    let binary = options.binary_dir.clone();
    if options.fresh {
        let _ = std::fs::remove_file(format!("{binary}/CMakeCache.txt"));
        let _ = std::fs::remove_dir_all(format!("{binary}/CMakeFiles"));
    }
    let _ = std::fs::create_dir_all(&binary);
    match interp.cache.load(std::path::Path::new(&binary)) {
        Ok(_) => {}
        Err(message) => {
            system_error(Some(interp), &message);
        }
    }
    for message in std::mem::take(&mut interp.cache.load_errors) {
        system_error(Some(interp), &message);
    }
    add_cmake_paths(interp);
    if !apply_cache_args(interp, options) {
        system_error(
            Some(interp),
            "Run 'cmake --help' for all supported options.",
        );
        return -1;
    }
    for (index, script) in options.preload_scripts.iter().enumerate() {
        if let Some(arg) = options.preload_script_args.get(index) {
            interp.stdout_line(&format!("loading initial cache file {arg}"));
        }
        let path = util::collapse_full_path(script, &util_cwd());
        read_temporary_list_file(interp, &path);
    }
    if let Some(report) = &options.preset_report {
        use std::io::Write;
        let mut out = std::io::stdout().lock();
        let _ = out.write_all(report.as_bytes());
        let _ = out.flush();
    }
    // cmake::Run: MAKEFLAGS would leak into try_compile builds.
    if std::env::var_os("MAKEFLAGS").is_some() {
        // SAFETY: single-threaded configure.
        unsafe { std::env::set_var("MAKEFLAGS", "") };
    }
    // cmake::PreLoadCMakeFiles
    for dir in [interp.home_source.clone(), interp.home_binary.clone()] {
        if dir.is_empty() {
            continue;
        }
        let pre_load = format!("{dir}/PreLoad.cmake");
        if std::path::Path::new(&pre_load).exists() {
            read_temporary_list_file(interp, &pre_load);
        }
    }
    // cmake::Configure: apply `DiagLevels` (from -W flags and presets) to
    // the cache, then sync the messenger with the cache.
    // cmake::Configure
    let configure_profile = project_profile_entry(options, "configure");
    // DiagLevel: 0 = DIAG_IGNORE, 1 = DIAG_WARN, 2 = DIAG_ERROR.
    let level = |warn: Option<bool>, error: Option<bool>| -> Option<u8> {
        if error == Some(true) {
            Some(2)
        } else if warn == Some(true) {
            Some(1)
        } else if warn == Some(false) {
            Some(0)
        } else {
            None
        }
    };
    let set_internal = |interp: &mut Interp, name: &str, value: bool, help: &str| {
        let value = if value { "TRUE" } else { "FALSE" };
        interp
            .cache
            .add(name, Some(value), Some(help), CacheType::Internal);
    };
    const WARN_DEPRECATED_HELP: &str = "Whether to issue warnings for deprecated functionality.";
    const ERROR_DEPRECATED_HELP: &str =
        "Whether to issue deprecation errors for macros and functions.";
    let set_deprecated = |interp: &mut Interp, level: u8| {
        // SetSuppressDeprecatedWarnings + SetDeprecatedWarningsAsErrors
        set_internal(
            interp,
            "CMAKE_WARN_DEPRECATED",
            level != 0,
            WARN_DEPRECATED_HELP,
        );
        set_internal(
            interp,
            "CMAKE_ERROR_DEPRECATED",
            level == 2,
            ERROR_DEPRECATED_HELP,
        );
    };
    if let Some(deprecated) = level(options.warn_deprecated, options.warn_deprecated_error) {
        set_deprecated(interp, deprecated);
    }
    if let Some(dev) = level(options.warn_dev, options.warn_dev_error) {
        let set_deprecated_variables = interp.cache.value("CMAKE_WARN_DEPRECATED").is_none()
            && interp.cache.value("CMAKE_ERROR_DEPRECATED").is_none();
        // SetSuppressDevWarnings + SetDevWarningsAsErrors
        set_internal(
            interp,
            "CMAKE_SUPPRESS_DEVELOPER_WARNINGS",
            dev == 0,
            "Suppress Warnings that are meant for the author of the CMakeLists.txt files.",
        );
        set_internal(
            interp,
            "CMAKE_SUPPRESS_DEVELOPER_ERRORS",
            dev != 2,
            "Suppress errors that are meant for the author of the CMakeLists.txt files.",
        );
        if set_deprecated_variables {
            set_deprecated(interp, dev);
        }
    }
    interp.warn_deprecated = !interp
        .cache
        .value("CMAKE_WARN_DEPRECATED")
        .is_some_and(util::is_off);
    interp.deprecated_as_errors = interp
        .cache
        .value("CMAKE_ERROR_DEPRECATED")
        .is_some_and(util::is_on);
    interp.suppress_dev_warnings = interp
        .cache
        .value("CMAKE_SUPPRESS_DEVELOPER_WARNINGS")
        .is_some_and(util::is_on);
    interp.dev_warnings_as_errors = interp
        .cache
        .value("CMAKE_SUPPRESS_DEVELOPER_ERRORS")
        .is_some_and(util::is_off);
    let mut file_api = None;
    let mut instrumentation = crate::instrumentation::Instrumentation::new(&binary);
    if !options.in_try_compile {
        instrumentation.clear_generated();
        instrumentation.load();
        if instrumentation.errors {
            return 1;
        }
    }
    let clock = crate::instrumentation::Timer::start(!options.in_try_compile);
    let code = actual_configure(interp, options, &mut file_api);
    if !options.in_try_compile {
        instrumentation.load();
        instrumentation.write_snippet("configure", &options.command_args, clock, 0);
        if instrumentation.enabled {
            instrumentation.setup_launchers(interp);
        }
    }
    drop(configure_profile);
    // `cmake::Configure`: HandleDeleteCacheVariables is consulted after
    // ActualConfigure, before any generation, even when configure failed.
    if !options.in_try_compile
        && !interp.in_try_compile
        && interp
            .global_props
            .get("__CMAKE_DELETE_CACHE_CHANGE_VARS_")
            .is_some_and(|vars| !vars.is_empty())
    {
        return RESTART_CONFIGURE;
    }
    if code != 0 {
        return code;
    }
    let generate_profile = project_profile_entry(options, "generate");
    let clock = instrumentation.timer();
    let code = crate::generator::generate(interp, options);
    instrumentation.write_snippet("generate", &options.command_args, clock, code);
    if let Some(api) = &mut file_api {
        let index_for = if code != 0 {
            crate::fileapi::IndexFor::FailedGenerate
        } else {
            crate::fileapi::IndexFor::Success
        };
        api.write_replies(interp, index_for);
    }
    drop(generate_profile);
    if code != 0 {
        interp.stderr_line(
            "CMake Generate step failed.  Build files cannot be regenerated correctly.",
        );
        return code;
    }
    if !options.in_try_compile {
        instrumentation.collect("postGenerate");
        crate::commands::core::status_line(
            interp,
            &format!("Build files have been written to: {}", options.binary_dir),
        );
    }
    0
}

/// The "project" profiling entries of cmake::Configure and cmake::Generate
/// (try_compile projects run in a separate, unprofiled cmake instance).
fn project_profile_entry(options: &ConfigureOptions, name: &str) -> crate::profiling::Entry {
    if options.in_try_compile {
        return crate::profiling::Entry::none();
    }
    crate::profiling::entry("project", name, || None)
}

/// `cmake::ActualConfigure`.
fn actual_configure(
    interp: &mut Interp,
    options: &ConfigureOptions,
    file_api: &mut Option<crate::fileapi::FileApi>,
) -> i32 {
    let home = options.source_dir.clone();
    let binary = options.binary_dir.clone();
    let _ = std::fs::remove_dir_all(format!("{binary}/CMakeFiles/CMakeScratch"));
    let cached_name = interp
        .cache
        .value("CMAKE_LIST_FILE_NAME")
        .unwrap_or_default();
    let requested_name = options.project_file.as_deref().unwrap_or_default();
    if !cached_name.is_empty() && !requested_name.is_empty() && cached_name != requested_name {
        let message = format!(
            "CMakeLists filename : \"{requested_name}\"\nDoes not match the previous: \"{cached_name}\"\nEither remove the CMakeCache.txt file and CMakeFiles directory or choose a different binary directory."
        );
        system_error(Some(interp), &message);
        return -2;
    }
    let list_name = [requested_name, cached_name, "CMakeLists.txt"]
        .into_iter()
        .find(|name| !name.is_empty())
        .unwrap_or_default()
        .to_owned();
    interp.project_file.clone_from(&list_name);
    if list_name != "CMakeLists.txt" {
        interp.issue_at(
            MessageType::Warning,
            "This project has been configured with a project file other than CMakeLists.txt.  This feature is intended for temporary use during development and not for publication of a final product.",
            &[],
        );
    }
    interp.cache.add(
        "CMAKE_LIST_FILE_NAME",
        Some(&list_name),
        Some("Name of CMakeLists files to read"),
        CacheType::Internal,
    );
    // DoPreConfigureChecks
    let source_list = format!("{home}/{list_name}");
    if !std::path::Path::new(&source_list).is_file() {
        let path = std::path::Path::new(&home);
        let mut message = if path.is_dir() {
            format!("The source directory \"{home}\" does not appear to contain {list_name}.\n")
        } else if path.exists() {
            format!("The source directory \"{home}\" is a file, not a directory.\n")
        } else {
            format!("The source directory \"{home}\" does not exist.\n")
        };
        message.push_str("Specify --help for usage, or press the help button on the CMake GUI.");
        system_error(Some(interp), &message);
        return -2;
    }
    if let Some(cached) = interp
        .cache
        .value("CMAKE_HOME_DIRECTORY")
        .map(str::to_owned)
    {
        let cache_start = format!("{cached}/{list_name}");
        if !same_file(&cache_start, &source_list) {
            let message = format!(
                "The source \"{source_list}\" does not match the source \"{cache_start}\" used to generate cache.  Re-run cmake with a different source directory."
            );
            system_error(Some(interp), &message);
            return -2;
        }
    } else {
        interp.cache.add(
            "CMAKE_HOME_DIRECTORY",
            Some(&home),
            Some("Source directory with the top level CMakeLists.txt file for this project"),
            CacheType::Internal,
        );
    }
    let redirects = format!("{binary}/CMakeFiles/pkgRedirects");
    let _ = std::fs::remove_dir_all(&redirects);
    if std::fs::create_dir_all(&redirects).is_err() {
        system_error(
            Some(interp),
            &format!(
                "Unable to (re)create the private pkgRedirects directory:\n  {redirects}\nThis may be caused by not having read/write access to the build directory.\nTry specifying a location with read/write access like:\n  cmake -B build\nIf using a CMake presets file, ensure that preset parameter\n'binaryDir' expands to a writable directory.\n"
            ),
        );
        return -1;
    }
    interp.cache.add(
        "CMAKE_FIND_PACKAGE_REDIRECTS_DIR",
        Some(&redirects),
        Some("Value Computed by CMake."),
        CacheType::Static,
    );
    // Generator selection.
    let requested = options.generator.clone();
    let cached = interp.cache.value("CMAKE_GENERATOR").map(str::to_owned);
    let generator = requested
        .clone()
        .or(cached.clone())
        .unwrap_or_else(|| "Ninja".to_owned());
    if generator != "Ninja" {
        let message = format!("Could not create named generator {generator}");
        system_error(Some(interp), &message);
        return -1;
    }
    if let (Some(requested), Some(cached)) = (&requested, &cached)
        && requested != cached
    {
        let message = format!(
            "Error: generator : {requested}\nDoes not match the generator used previously: {cached}\nEither remove the CMakeCache.txt file and CMakeFiles directory or choose a different binary directory."
        );
        system_error(Some(interp), &message);
        return -2;
    }
    if cached.is_none() {
        interp.cache.add(
            "CMAKE_GENERATOR",
            Some(&generator),
            Some("Name of generator."),
            CacheType::Internal,
        );
        interp.cache.add(
            "CMAKE_EXTRA_GENERATOR",
            Some(""),
            Some("Name of external makefile project generator."),
            CacheType::Internal,
        );
        if interp.cache.value("CMAKE_TOOLCHAIN_FILE").is_none()
            && let Ok(toolchain) = std::env::var("CMAKE_TOOLCHAIN_FILE")
            && !toolchain.is_empty()
        {
            interp.cache.add(
                "CMAKE_TOOLCHAIN_FILE",
                Some(&toolchain),
                Some("The CMake toolchain file"),
                CacheType::Filepath,
            );
        }
    }
    for (name, value, help) in [
        (
            "CMAKE_GENERATOR_INSTANCE",
            String::new(),
            "Generator instance identifier.",
        ),
        (
            "CMAKE_GENERATOR_PLATFORM",
            options.platform.clone().unwrap_or_default(),
            "Name of generator platform.",
        ),
        (
            "CMAKE_GENERATOR_TOOLSET",
            options.toolset.clone().unwrap_or_default(),
            "Name of generator toolset.",
        ),
    ] {
        if interp.cache.value(name).is_none() {
            interp
                .cache
                .add(name, Some(&value), Some(help), CacheType::Internal);
        }
    }
    if interp.cache.value("CMAKE_TEST_LAUNCHER").is_none()
        && let Ok(launcher) = std::env::var("CMAKE_TEST_LAUNCHER")
        && !launcher.is_empty()
    {
        interp.cache.add(
            "CMAKE_TEST_LAUNCHER",
            Some(&launcher),
            Some("Test launcher to run tests executable."),
            CacheType::String,
        );
    }
    if interp
        .cache
        .value("CMAKE_CROSSCOMPILING_EMULATOR")
        .is_none()
        && let Ok(emulator) = std::env::var("CMAKE_CROSSCOMPILING_EMULATOR")
        && !emulator.is_empty()
    {
        interp.cache.add(
            "CMAKE_CROSSCOMPILING_EMULATOR",
            Some(&emulator),
            Some("Emulator to run executables and tests when cross compiling."),
            CacheType::String,
        );
    }
    // cmake::ActualConfigure: the file API reads queries before configuring.
    let mut api = crate::fileapi::FileApi::new(&binary);
    api.read_queries();
    *file_api = Some(api);
    let start = Instant::now();
    configure_root(interp, &source_list);
    check_reserved_global_targets(interp);
    if let Some(api) = file_api {
        api.add_project_queries(interp);
    }
    let elapsed = start.elapsed().as_secs_f64();
    if !options.in_try_compile {
        let message = if interp.error {
            "Configuring incomplete, errors occurred!".to_owned()
        } else {
            format!("Configuring done ({elapsed:.1}s)")
        };
        crate::commands::core::status_line(interp, &message);
    }
    if interp.is_on("CTEST_USE_LAUNCHERS")
        && interp.global_props.get("RULE_LAUNCH_COMPILE").is_none()
    {
        interp.issue_at(
            MessageType::FatalError,
            "CTEST_USE_LAUNCHERS is enabled, but the RULE_LAUNCH_COMPILE global property is not defined.\nDid you forget to include(CTest) in the toplevel CMakeLists.txt ?",
            &[],
        );
    }
    match crate::commands::file::save_verification_script(interp, &binary) {
        Ok(verify) => interp.ext.glob_verify = verify,
        Err(message) => system_error(Some(interp), &message),
    }
    // `cmCacheManager::OutputNewlineTruncationWarning` issues the
    // messenger warning while saving the cache.
    let newline_keys: Vec<String> = interp
        .cache
        .entries
        .iter()
        .filter(|(_, entry)| entry.initialized && entry.value.contains('\n'))
        .map(|(key, _)| key.clone())
        .collect();
    for key in newline_keys {
        interp.issue(
            MessageType::Warning,
            &format!("Value of {key} contained a newline; truncating"),
        );
    }
    let version = crate::interp::VERSION;
    let command = interp.cmake_command.clone();
    if let Err(error) = interp
        .cache
        .save(std::path::Path::new(&binary), &command, version)
    {
        system_error(
            Some(interp),
            &format!("Unable to open cache file for save. {binary}/CMakeCache.txt: {error}"),
        );
    }
    if interp.error {
        if let Some(api) = file_api {
            api.write_replies(interp, crate::fileapi::IndexFor::FailedConfigure);
        }
        return -1;
    }
    0
}

fn same_file(lhs: &str, rhs: &str) -> bool {
    match (std::fs::canonicalize(lhs), std::fs::canonicalize(rhs)) {
        (Ok(a), Ok(b)) => a == b,
        _ => lhs == rhs,
    }
}

/// `cmGlobalGenerator::CheckReservedTargetName` as run by
/// `AddGlobalTarget_{Package,PackageSource,Test}` during
/// `cmGlobalGenerator::Configure`: a target already using a reserved name
/// aborts the run with the error reported at the target's backtrace.
fn check_reserved_global_targets(interp: &mut Interp) {
    fn reserved(interp: &mut Interp, names: &[&str], reason: &str) {
        for name in names {
            if let Some(key) = crate::state::find_global_target_key(interp, name) {
                let backtrace = interp.ext.targets[&key].backtrace.clone();
                interp.issue_at(
                    MessageType::FatalError,
                    &format!("The target name \"{name}\" is reserved {reason}."),
                    &backtrace,
                );
                return;
            }
            // `cmGlobalGenerator::FindTarget` also finds ALIAS names.
            if let Some((_, _, backtrace)) = interp
                .ext
                .alias_backtraces
                .iter()
                .find(|(_, alias, _)| alias == name)
            {
                let backtrace = backtrace.clone();
                interp.issue_at(
                    MessageType::FatalError,
                    &format!("The target name \"{name}\" is reserved {reason}."),
                    &backtrace,
                );
                return;
            }
        }
    }
    let root_binary = interp.makefiles[0].binary_dir.clone();
    if std::path::Path::new(&format!("{root_binary}/CPackConfig.cmake")).exists() {
        reserved(
            interp,
            &["package", "PACKAGE"],
            "when CPack packaging is enabled",
        );
    }
    if std::path::Path::new(&format!("{root_binary}/CPackSourceConfig.cmake")).exists() {
        reserved(
            interp,
            &["package_source"],
            "when CPack source packaging is enabled",
        );
    }
    if interp.is_on("CMAKE_TESTING_ENABLED") {
        reserved(
            interp,
            &["test", "RUN_TESTS"],
            "when CTest testing is enabled",
        );
    }
}

/// `cmGlobalGenerator::Configure` + `cmMakefile::Configure` for the root.
fn configure_root(interp: &mut Interp, list_file: &str) {
    interp.makefiles[0].backtrace[0].file = std::rc::Rc::from(list_file);
    let binary = interp.home_binary.clone();
    let source = interp.home_source.clone();
    interp.set_def("CMAKE_SOURCE_DIR", source.clone());
    interp.set_def("CMAKE_BINARY_DIR", binary.clone());
    interp.set_def("CMAKE_CURRENT_SOURCE_DIR", source);
    interp.set_def("CMAKE_CURRENT_BINARY_DIR", binary.clone());
    let _ = std::fs::create_dir_all(format!("{binary}/CMakeFiles"));
    interp.run_root_list_file(list_file);
    interp.record_directory_vars();
    if interp.root_check_cmp0000 && !interp.fatal {
        let (major, minor, _) = crate::interp::VERSION;
        let text = format!(
            "No cmake_minimum_required command is present.  A line of code such as\n  cmake_minimum_required(VERSION {major}.{minor})\nshould be added at the top of the file.  The version specified may be lower if you wish to support older CMake versions for this project.  For more information run \"cmake --help-policy CMP0000\"."
        );
        let frame = Frame {
            file: std::rc::Rc::from(list_file),
            line: 0,
            name: None,
            deferred: false,
        };
        interp.issue_at(MessageType::FatalError, &text, &[frame]);
        interp.set_fatal();
    }
    reserve_global_target_codegen(interp);
    let count = interp.makefiles.len().to_string();
    interp.cache.add(
        "CMAKE_NUMBER_OF_MAKEFILES",
        Some(&count),
        Some("number of local generators"),
        CacheType::Internal,
    );
}

/// Parses try_compile CMAKE_FLAGS-style `-D` arguments into definitions.
fn parse_cache_args(args: &[String]) -> Vec<CacheDefinition> {
    let mut out = Vec::new();
    let mut index = 0;
    while index < args.len() {
        let arg = &args[index];
        if let Some(rest) = arg.strip_prefix("-D") {
            let entry = if rest.is_empty() {
                index += 1;
                args.get(index).cloned().unwrap_or_default()
            } else {
                rest.to_owned()
            };
            if let Some((name, value, kind)) = crate::cache::parse_entry(&entry) {
                out.push(CacheDefinition {
                    name,
                    value,
                    kind: (kind != CacheType::Uninitialized).then_some(kind),
                });
            }
        }
        index += 1;
    }
    out
}

/// `cmMakefile::TryCompile` + `cmGlobalGenerator::TryCompile`: configures,
/// generates and builds a test project in-process. Returns the build tool's
/// exit code (or 1 on configure/generate failure) and the captured output.
pub fn try_compile_project(
    outer: &mut Interp,
    srcdir: &str,
    bindir: &str,
    target_name: &str,
    fast: bool,
    cmake_args: Option<&[String]>,
) -> (i32, String) {
    let _ = fast;
    let _profiling = crate::profiling::suspend();
    let _ = std::fs::create_dir_all(bindir);
    let previous_cwd = std::env::current_dir().ok();
    if std::env::set_current_dir(bindir).is_err() {
        let text = format!("Failed to change working directory to {bindir}");
        outer.issue(MessageType::FatalError, &text);
        outer.set_fatal();
        return (1, String::new());
    }
    let options = ConfigureOptions {
        source_dir: srcdir.to_owned(),
        binary_dir: bindir.to_owned(),
        in_try_compile: true,
        debug_trycompile: outer.debug_trycompile,
        definitions: cmake_args.map(parse_cache_args).unwrap_or_default(),
        ..ConfigureOptions::default()
    };
    let root = outer.cmake_root.clone();
    let mut inner = new_project_interp(&options, &root);
    inner.recursion_depth_base = outer.recursion_depth_now();
    let _ = inner.cache.load(std::path::Path::new(bindir));
    add_cmake_paths(&mut inner);
    if let Some(config) = outer
        .get_def("CMAKE_TRY_COMPILE_CONFIGURATION")
        .map(str::to_owned)
    {
        inner.cache.add(
            "CMAKE_BUILD_TYPE",
            Some(&config),
            Some("Build configuration"),
            CacheType::String,
        );
    }
    if let Some(depth) = outer
        .get_def("CMAKE_MAXIMUM_RECURSION_DEPTH")
        .map(str::to_owned)
    {
        inner.cache.add(
            "CMAKE_MAXIMUM_RECURSION_DEPTH",
            Some(&depth),
            Some("Maximum recursion depth"),
            CacheType::String,
        );
    }
    for definition in &options.definitions {
        inner.cache.add(
            &definition.name,
            Some(&definition.value),
            Some("No help, variable specified on the command line."),
            definition.kind.unwrap_or(CacheType::Uninitialized),
        );
    }
    // EnableLanguagesFromGenerator.
    let configured = outer
        .langs
        .configured_files_path
        .clone()
        .unwrap_or_else(|| format!("{}/CMakeFiles", outer.home_binary));
    inner.langs = outer.langs.clone();
    inner.langs.configured_files_path = Some(configured);
    inner.langs.in_progress.clear();
    if let Some(make) = outer.cache.value("CMAKE_MAKE_PROGRAM").map(str::to_owned) {
        inner.cache.add(
            "CMAKE_MAKE_PROGRAM",
            Some(&make),
            Some("make program"),
            CacheType::Filepath,
        );
    }
    let suppress = if outer.is_on("CMAKE_SUPPRESS_DEVELOPER_WARNINGS") {
        "TRUE"
    } else {
        "FALSE"
    };
    inner.cache.add(
        "CMAKE_SUPPRESS_DEVELOPER_WARNINGS",
        Some(suppress),
        Some(""),
        CacheType::Internal,
    );
    inner.suppress_dev_warnings = outer.suppress_dev_warnings;
    // cmake::SetTraceRedirect: the inner instance traces into the parent's
    // output with the parent's settings.
    inner.trace = outer.trace.clone();
    let restore = |previous: &Option<std::path::PathBuf>| {
        if let Some(previous) = previous {
            let _ = std::env::set_current_dir(previous);
        }
    };
    // The try_compile cmake instance runs the file API like any other.
    let mut file_api = None;
    if actual_configure(&mut inner, &options, &mut file_api) != 0 {
        outer.issue(
            MessageType::FatalError,
            "Failed to configure test project build system.",
        );
        outer.set_fatal();
        restore(&previous_cwd);
        return (1, String::new());
    }
    let generated = crate::generator::generate(&mut inner, &options);
    if let Some(api) = &mut file_api {
        let index_for = if generated != 0 {
            crate::fileapi::IndexFor::FailedGenerate
        } else {
            crate::fileapi::IndexFor::Success
        };
        api.write_replies(&inner, index_for);
    }
    if generated != 0 {
        outer.issue(
            MessageType::FatalError,
            "Failed to generate test project build system.",
        );
        outer.set_fatal();
        restore(&previous_cwd);
        return (1, String::new());
    }
    // cmGlobalGenerator::Build
    let mut output = format!("Change Dir: '{bindir}'\n");
    let make = inner.get_safe("CMAKE_MAKE_PROGRAM");
    let mut command = vec![make, "-v".to_owned()];
    if !target_name.is_empty() {
        command.push(target_name.to_owned());
    }
    let printable: Vec<String> = command
        .iter()
        .map(|arg| crate::outconv::escape_for_shell(arg, crate::outconv::FLAG_IS_UNIX))
        .collect();
    output.push_str(&format!(
        "\nRun Build Command(s): {}\n",
        printable.join(" ")
    ));
    let result = run_merged(&command, bindir);
    let code = match result {
        Ok((code, text)) => {
            output.push_str(&text);
            code
        }
        Err(error) => {
            let message = format!(
                "Generator: build tool execution failed, command was: {}",
                command.join(" ")
            );
            system_error(Some(outer), &message);
            output.push_str(&format!(
                "{error}\nGenerator: build tool execution failed, command was: {}\n",
                printable.join(" ")
            ));
            restore(&previous_cwd);
            return (1, output);
        }
    };
    output.push('\n');
    restore(&previous_cwd);
    (code, output)
}

/// Runs a command with stdout and stderr merged in order
/// (`cmSystemTools::RunSingleCommand` with both outputs to one buffer).
pub fn run_merged(command: &[String], cwd: &str) -> std::io::Result<(i32, String)> {
    use std::os::unix::io::FromRawFd;
    let (reader, writer) = os_pipe()?;
    // SAFETY: duplicating a valid fd.
    let writer_dup = unsafe { libc::dup(writer) };
    if writer_dup < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: fds come from pipe()/dup() above and are owned here.
    let (stdout, stderr) = unsafe {
        (
            std::process::Stdio::from_raw_fd(writer),
            std::process::Stdio::from_raw_fd(writer_dup),
        )
    };
    let mut child = std::process::Command::new(&command[0])
        .args(&command[1..])
        .current_dir(cwd)
        .stdin(std::process::Stdio::null())
        .stdout(stdout)
        .stderr(stderr)
        .spawn()?;
    // SAFETY: reader fd from pipe().
    let mut file = unsafe { std::fs::File::from_raw_fd(reader) };
    let mut buffer = Vec::new();
    use std::io::Read;
    file.read_to_end(&mut buffer)?;
    let status = child.wait()?;
    let code = status.code().unwrap_or(1);
    Ok((code, String::from_utf8_lossy(&buffer).into_owned()))
}

fn os_pipe() -> std::io::Result<(i32, i32)> {
    let mut fds = [0i32; 2];
    // SAFETY: valid out-pointer for two fds.
    if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok((fds[0], fds[1]))
}

/// `cmGlobalGenerator::ReserveGlobalTargetCodegen`: CMP0171 as set at the
/// end of the top-level CMakeLists.txt enables the per-directory `codegen`
/// targets, which reserves the target name.
fn reserve_global_target_codegen(interp: &mut Interp) {
    use crate::policy::PolicyStatus;
    let status = interp.policy_status_at(0, "CMP0171");
    if status == PolicyStatus::New {
        interp
            .ext
            .misc
            .insert("allow-global-target-codegen".to_owned(), "1".to_owned());
    }
    let Some(key) = crate::state::find_global_target_key(interp, "codegen") else {
        return;
    };
    let backtrace = interp.ext.targets[&key].backtrace.clone();
    let policy = crate::policy::index_of("CMP0171").expect("CMP0171");
    match status {
        PolicyStatus::Warn => interp.issue_at(
            MessageType::AuthorWarning,
            &format!(
                "{}\nThe target name \"codegen\" is reserved.",
                crate::policy::policy_warning(policy)
            ),
            &backtrace,
        ),
        PolicyStatus::Old => {}
        PolicyStatus::New => {
            interp.issue_at(
                MessageType::FatalError,
                "The target name \"codegen\" is reserved.",
                &backtrace,
            );
            interp.set_fatal();
        }
    }
}
