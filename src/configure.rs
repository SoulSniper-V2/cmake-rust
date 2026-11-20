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
    pub fresh: bool,
    pub log_level: Option<String>,
    pub log_context: bool,
    pub warn_dev: Option<bool>,
    pub warn_dev_error: Option<bool>,
    pub warn_deprecated: Option<bool>,
    pub warn_deprecated_error: Option<bool>,
    pub warn_uninitialized: bool,
    pub debug_trycompile: bool,
    pub trace: bool,
    /// Original command-line arguments.
    pub argv: Vec<String>,
    /// `--explain <target> <flag>` request (cmake-rust extension).
    pub explain: Option<(String, String)>,
    /// Configure for a try_compile project (no progress output).
    pub in_try_compile: bool,
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
        ("CMAKE_COMMAND", command.clone(), "Path to CMake executable."),
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
        interp.cache.add(name, Some(&value), Some(help), CacheType::Internal);
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
        if cfg!(target_os = "macos") { "Darwin" } else { "Linux" },
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
        interp.cache.add(
            &definition.name,
            Some(&definition.value),
            Some("No help, variable specified on the command line."),
            definition.kind.unwrap_or(CacheType::Uninitialized),
        );
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
    if let Some(toolchain) = &options.toolchain_file {
        let path = util::collapse_full_path(toolchain, &util_cwd());
        interp.cache.add(
            "CMAKE_TOOLCHAIN_FILE",
            Some(&path),
            Some("The CMake toolchain file"),
            CacheType::Filepath,
        );
    }
    if let Some(prefix) = &options.install_prefix {
        let path = util::collapse_full_path(prefix, &util_cwd());
        interp.cache.add(
            "CMAKE_INSTALL_PREFIX",
            Some(&path),
            Some("Install path prefix, prepended onto install directories."),
            CacheType::Path,
        );
    }
    true
}

fn util_cwd() -> String {
    std::env::current_dir()
        .map(|dir| dir.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// `cmsys::Glob::PatternToRegex` (no recursion, anchored).
fn glob_to_regex(pattern: &str) -> crate::cmregex::Regex {
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
pub fn new_project_interp(options: &ConfigureOptions, root: &str) -> Interp {
    let mut interp = Interp::new(Mode::Project, &options.source_dir, &options.binary_dir, root);
    interp.in_try_compile = options.in_try_compile;
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
pub fn configure(options: &ConfigureOptions) -> i32 {
    let Some(root) = find_cmake_root() else {
        eprintln!("CMake Error: Could not find CMAKE_ROOT !!!");
        return 1;
    };
    let mut interp = new_project_interp(options, &root);
    let code = run_configure(&mut interp, options);
    if code != 0 {
        return 1;
    }
    0
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
    add_cmake_paths(interp);
    if !apply_cache_args(interp, options) {
        system_error(Some(interp), "Run 'cmake --help' for all supported options.");
        return -1;
    }
    for script in &options.preload_scripts {
        // `-C` scripts run in script context before configuring.
        let path = util::collapse_full_path(script, &util_cwd());
        interp.read_list_file(&path);
    }
    // Diagnostic settings from -W flags and cache.
    if let Some(dev) = options.warn_dev {
        interp.suppress_dev_warnings = !dev;
    }
    if options.warn_dev_error == Some(true) {
        interp.dev_warnings_as_errors = true;
    }
    if let Some(value) = interp.cache.value("CMAKE_WARN_DEPRECATED") {
        interp.warn_deprecated = !util::is_off(value);
    }
    if let Some(deprecated) = options.warn_deprecated {
        interp.warn_deprecated = deprecated;
    }
    if options.warn_deprecated_error == Some(true) || interp.cache.value("CMAKE_ERROR_DEPRECATED").is_some_and(util::is_on) {
        interp.deprecated_as_errors = true;
    }
    if interp.cache.value("CMAKE_SUPPRESS_DEVELOPER_WARNINGS").is_some_and(util::is_on) {
        interp.suppress_dev_warnings = true;
    }
    let code = actual_configure(interp, options);
    if code != 0 {
        return code;
    }
    let code = crate::generator::generate(interp, options);
    if code != 0 {
        interp.stdout_line_raw("CMake Generate step failed.  Build files cannot be regenerated correctly.");
        return code;
    }
    if !options.in_try_compile {
        crate::commands::core::status_line(
            interp,
            &format!("Build files have been written to: {}", options.binary_dir),
        );
    }
    0
}

/// `cmake::ActualConfigure`.
fn actual_configure(interp: &mut Interp, options: &ConfigureOptions) -> i32 {
    let home = options.source_dir.clone();
    let binary = options.binary_dir.clone();
    let _ = std::fs::remove_dir_all(format!("{binary}/CMakeFiles/CMakeScratch"));
    let list_name = interp
        .cache
        .value("CMAKE_LIST_FILE_NAME")
        .map(str::to_owned)
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "CMakeLists.txt".to_owned());
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
    if let Some(cached) = interp.cache.value("CMAKE_HOME_DIRECTORY").map(str::to_owned) {
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
    let _ = std::fs::create_dir_all(&redirects);
    interp.cache.add(
        "CMAKE_FIND_PACKAGE_REDIRECTS_DIR",
        Some(&redirects),
        Some("Value Computed by CMake."),
        CacheType::Static,
    );
    // Generator selection.
    let requested = options.generator.clone();
    let cached = interp.cache.value("CMAKE_GENERATOR").map(str::to_owned);
    let generator = requested.clone().or(cached.clone()).unwrap_or_else(|| "Ninja".to_owned());
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
        interp.cache.add("CMAKE_GENERATOR", Some(&generator), Some("Name of generator."), CacheType::Internal);
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
        ("CMAKE_GENERATOR_INSTANCE", String::new(), "Generator instance identifier."),
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
            interp.cache.add(name, Some(&value), Some(help), CacheType::Internal);
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
    if interp.cache.value("CMAKE_CROSSCOMPILING_EMULATOR").is_none()
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
    let start = Instant::now();
    configure_root(interp, &source_list);
    let elapsed = start.elapsed().as_secs_f64();
    if !options.in_try_compile {
        let message = if interp.error {
            "Configuring incomplete, errors occurred!".to_owned()
        } else {
            format!("Configuring done ({elapsed:.1}s)")
        };
        crate::commands::core::status_line(interp, &message);
    }
    if interp.is_on("CTEST_USE_LAUNCHERS") && interp.global_props.get("RULE_LAUNCH_COMPILE").is_none() {
        interp.issue_at(
            MessageType::FatalError,
            "CTEST_USE_LAUNCHERS is enabled, but the RULE_LAUNCH_COMPILE global property is not defined.\nDid you forget to include(CTest) in the toplevel CMakeLists.txt ?",
            &[],
        );
    }
    let version = crate::interp::VERSION;
    let command = interp.cmake_command.clone();
    if let Err(error) = interp
        .cache
        .save(std::path::Path::new(&binary), &command, version)
    {
        system_error(Some(interp), &format!("Unable to open cache file for save. {binary}/CMakeCache.txt: {error}"));
    }
    if interp.error {
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

/// `cmGlobalGenerator::Configure` + `cmMakefile::Configure` for the root.
fn configure_root(interp: &mut Interp, list_file: &str) {
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
        definitions: cmake_args.map(parse_cache_args).unwrap_or_default(),
        ..ConfigureOptions::default()
    };
    let root = outer.cmake_root.clone();
    let mut inner = new_project_interp(&options, &root);
    inner.recursion_depth_base = outer.recursion_depth_now();
    let _ = inner.cache.load(std::path::Path::new(bindir));
    add_cmake_paths(&mut inner);
    if let Some(config) = outer.get_def("CMAKE_TRY_COMPILE_CONFIGURATION").map(str::to_owned) {
        inner.cache.add("CMAKE_BUILD_TYPE", Some(&config), Some("Build configuration"), CacheType::String);
    }
    if let Some(depth) = outer.get_def("CMAKE_MAXIMUM_RECURSION_DEPTH").map(str::to_owned) {
        inner.cache.add("CMAKE_MAXIMUM_RECURSION_DEPTH", Some(&depth), Some("Maximum recursion depth"), CacheType::String);
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
        inner.cache.add("CMAKE_MAKE_PROGRAM", Some(&make), Some("make program"), CacheType::Filepath);
    }
    let suppress = if outer.is_on("CMAKE_SUPPRESS_DEVELOPER_WARNINGS") { "TRUE" } else { "FALSE" };
    inner.cache.add("CMAKE_SUPPRESS_DEVELOPER_WARNINGS", Some(suppress), Some(""), CacheType::Internal);
    inner.suppress_dev_warnings = outer.suppress_dev_warnings;
    let restore = |previous: &Option<std::path::PathBuf>| {
        if let Some(previous) = previous {
            let _ = std::env::set_current_dir(previous);
        }
    };
    if actual_configure(&mut inner, &options) != 0 {
        outer.issue(MessageType::FatalError, "Failed to configure test project build system.");
        outer.set_fatal();
        restore(&previous_cwd);
        return (1, String::new());
    }
    if crate::generator::generate(&mut inner, &options) != 0 {
        outer.issue(MessageType::FatalError, "Failed to generate test project build system.");
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
    output.push_str(&format!("\nRun Build Command(s): {}\n", printable.join(" ")));
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
            output.push_str(&format!("{error}\nGenerator: build tool execution failed, command was: {}\n", printable.join(" ")));
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
