//! Public command dispatch and project/script argument handling.

use std::os::unix::fs::FileTypeExt;
use std::path::Path;
use std::process::Command;

use crate::cache::{self, Cache, CacheType};
use crate::configure::{self, CacheDefinition, ConfigureOptions};
use crate::interp::{self, Interp, Mode};
use crate::{cmcmd, util};

const GENERATORS: &str = include_str!("cmcmd/generators.txt");
const HELP: &str = include_str!("cmcmd/help.txt");
const HELP_HINT: &str = "Run 'cmake --help' for all supported options.";
const IMPLICIT_DIRECTORIES: &str = "No source or binary directory provided. Both will be assumed to be the same as the current working directory, but note that this warning will become a fatal error in future CMake releases.";

pub fn main() -> i32 {
    let argv: Vec<String> = std::env::args_os()
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect();
    let Ok(current_dir) = std::env::current_dir() else {
        eprintln!("Current working directory cannot be established.");
        return 1;
    };
    let cwd = std::env::var("PWD")
        .ok()
        .filter(|pwd| {
            Path::new(pwd).is_absolute()
                && std::fs::canonicalize(pwd).ok().as_ref() == Some(&current_dir)
        })
        .unwrap_or_else(|| current_dir.to_string_lossy().into_owned());
    let args = &argv[1..];
    match args.first().map(String::as_str) {
        Some("-E") => cmcmd::dispatch(&argv[0], &args[1..]),
        Some("--build") => build(&args[1..], &cwd),
        Some("--install") => install(&argv[0], &args[1..], &cwd),
        Some("--workflow") => crate::presets::cli::workflow(&args[1..], &cwd),
        Some("--print-config-dir") => {
            if let Ok(dir) = std::env::var("CMAKE_CONFIG_DIR") {
                println!("{dir}");
                return 0;
            }
            let config = std::env::var("XDG_CONFIG_HOME").ok().or_else(|| {
                let home = std::env::var("HOME").ok()?;
                let suffix = if cfg!(target_os = "macos") {
                    "Library/Application Support"
                } else {
                    ".config"
                };
                Some(format!("{home}/{suffix}"))
            });
            if let Some(config) = config {
                let name = if cfg!(target_os = "macos") {
                    "CMake"
                } else {
                    "cmake"
                };
                println!("{config}/{name}");
            } else {
                println!();
            }
            0
        }
        _ => documentation(args).unwrap_or_else(|| project_or_script(&argv, &cwd)),
    }
}

fn documentation(args: &[String]) -> Option<i32> {
    if args.is_empty() {
        print!("{}", include_str!("cmcmd/usage.txt"));
        return Some(0);
    }
    let mut handled = false;
    let mut printed = false;
    let mut index = 0;
    while index < args.len() {
        let arg = &args[index];
        let mut destination = None;
        let output = match arg.as_str() {
            "--" => break,
            "--version" | "-version" | "/V" => {
                destination = args.get(index + 1).filter(|arg| !is_flag(arg));
                format!(
                    "cmake version {}\n\nCMake suite maintained and supported by Kitware (kitware.com/cmake).\n",
                    interp::VERSION_STRING
                )
            }
            "--help-command-list" => {
                destination = args.get(index + 1).filter(|arg| !is_flag(arg));
                include_str!("cmcmd/command-list.txt").to_owned()
            }
            "--help" | "-help" | "-h" | "-H" | "-usage" | "/?" => {
                if let Some(keyword) = args
                    .get(index + 1)
                    .filter(|arg| !arg.is_empty() && !is_flag(arg))
                {
                    eprintln!(
                        "Argument \"{keyword}\" to --help is not a CMake manual.  Use --help-manual-list to see all manuals."
                    );
                    return Some(1);
                }
                generator_documentation(HELP)
            }
            other if other.starts_with("--help-") => {
                cmcmd::error(format!("{other} is not supported"));
                return Some(1);
            }
            _ => {
                index += 1;
                continue;
            }
        };
        handled = true;
        if let Some(path) = destination {
            if let Err(error) = std::fs::write(path, output) {
                cmcmd::error(format!(
                    "Could not open file for write in copy operation {path}: {error}"
                ));
                return Some(1);
            }
            index += 1;
        } else {
            if printed {
                print!("\n\n");
            }
            print!("{output}");
            printed = true;
        }
        index += 1;
    }
    handled.then_some(0)
}

fn mark_generator(text: &str, generator: &str) -> String {
    const SECTION: &str = "\nGenerators\n";
    let Some((head, list)) = text.split_once(SECTION) else {
        return text.to_owned();
    };
    if generator.is_empty() {
        return text.to_owned();
    }
    let standard = format!("  {generator}");
    let marked = format!("* {generator}");
    let list = list
        .split_inclusive('\n')
        .map(|line| match line.strip_prefix(&standard) {
            Some(rest) if rest.trim_start().starts_with('=') => format!("{marked}{rest}"),
            _ => line.to_owned(),
        })
        .collect::<String>();
    format!("{head}{SECTION}{list}")
}

fn generator_documentation(text: &str) -> String {
    let generator = std::env::var("CMAKE_GENERATOR").unwrap_or_default();
    if generator.is_empty() {
        return text.to_owned();
    }
    if known_generator(&generator) {
        return mark_generator(
            &text.replace("* Unix Makefiles", "  Unix Makefiles"),
            &generator,
        );
    }
    cmcmd::error(
        "CMAKE_GENERATOR was set but the specified generator doesn't exist. Using CMake default.",
    );
    text.to_owned()
}

fn warn_orphan_generator_settings() {
    if std::env::var_os("CMAKE_GENERATOR").is_none() {
        for variable in [
            "CMAKE_GENERATOR_INSTANCE",
            "CMAKE_GENERATOR_PLATFORM",
            "CMAKE_GENERATOR_TOOLSET",
        ] {
            if std::env::var_os(variable).is_some() {
                eprintln!(
                    "Warning: Environment variable {variable} will be ignored, because CMAKE_GENERATOR is not set."
                );
            }
        }
    }
}

pub(crate) fn known_generator(name: &str) -> bool {
    GENERATORS.lines().any(|line| {
        line.get(2..)
            .and_then(|tail| tail.split_once(" = "))
            .is_some_and(|(generator, _)| generator.trim_end() == name)
    })
}

pub(crate) fn is_flag(arg: &str) -> bool {
    arg.starts_with('-') && !arg.as_bytes().get(1).is_some_and(u8::is_ascii_digit)
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Values {
    One,
    Optional,
    Many,
}

/// The value-bearing forms of cmCommandLineArgument. The caller chooses
/// whether an attached value requires an '=' or space separator.
pub(crate) fn matches_value(arg: &str, name: &str, attached: bool) -> bool {
    arg.strip_prefix(name)
        .is_some_and(|rest| attached || rest.is_empty() || rest.starts_with(['=', ' ']))
}

pub(crate) fn read_value(
    args: &[String],
    index: &mut usize,
    name: &str,
    values: Values,
    error: &str,
) -> Result<String, ()> {
    let mut rest = &args[*index][name.len()..];
    if !rest.is_empty() {
        if let Some(value) = rest.strip_prefix('=') {
            rest = value;
            if rest.is_empty() {
                cmcmd::error(error);
                return Err(());
            }
        }
        return Ok(rest.strip_prefix(' ').unwrap_or(rest).to_owned());
    }
    let start = *index + 1;
    if args.get(start).is_none_or(|value| is_flag(value)) {
        if values == Values::Optional {
            return Ok(String::new());
        }
        cmcmd::error(error);
        return Err(());
    }
    *index = start;
    if values == Values::Many {
        while args.get(*index + 1).is_some_and(|value| !is_flag(value)) {
            *index += 1;
        }
    }
    Ok(args[start..=*index].join(";"))
}

fn cli_failure() -> i32 {
    cmcmd::error(HELP_HINT);
    1
}

#[derive(Default)]
struct CacheView {
    all: bool,
    help: bool,
    filter: Option<crate::cmregex::Regex>,
}

impl CacheView {
    fn print(&self, cache: &Cache) {
        println!("-- Cache values");
        for (key, entry) in &cache.entries {
            if matches!(
                entry.kind,
                CacheType::Internal | CacheType::Static | CacheType::Uninitialized
            ) || (!self.all && entry.properties.contains_key("ADVANCED"))
                || self
                    .filter
                    .as_ref()
                    .is_some_and(|regex| regex.find(key).is_none())
            {
                continue;
            }
            if self.help {
                println!(
                    "// {}",
                    entry
                        .properties
                        .get("HELPSTRING")
                        .map_or("", String::as_str)
                );
            }
            println!("{key}:{}={}", entry.kind.as_str(), entry.value);
            if self.help {
                println!();
            }
        }
    }
}

fn warning(message: &str) {
    eprintln!(
        "{}",
        crate::messages::render(crate::messages::MessageType::Warning, message, &[], None)
    );
}

fn set_source(options: &mut ConfigureOptions, source: String, script_mode: bool) {
    if !script_mode && !options.source_dir.is_empty() && options.source_dir != source {
        extra_path(&options.source_dir);
    }
    options.source_dir = source;
}

/// `cmSystemTools::ToNormalizedPathOnDisk` against the logical working
/// directory `cwd`.
fn normalized_path(value: &str, cwd: &str) -> String {
    let value = crate::pathresolver::resolve_tilde_path(value);
    let joined = if value.starts_with('/') { value } else { format!("{cwd}/{value}") };
    crate::pathresolver::to_normalized_path_on_disk(&joined)
}

/// `cmTokenize(value, ',')` in Legacy mode: no tokens yields one empty one.
fn tokenize_commas(value: &str) -> Vec<&str> {
    let tokens: Vec<&str> = value.split(',').filter(|v| !v.is_empty()).collect();
    if tokens.is_empty() { vec![""] } else { tokens }
}

fn extra_path(path: &str) {
    warning(&format!(
        "Ignoring extra path from command line:\n \"{path}\""
    ));
}

fn positional(options: &mut ConfigureOptions, arg: &str, cwd: &str, script_mode: bool) -> bool {
    let full = util::collapse_full_path(arg, cwd);
    let path = Path::new(&full);
    let is_dir = path.is_dir();
    let exists = path.exists();
    let is_source = is_dir && path.join("CMakeLists.txt").is_file();
    let leaf = util::filename_name(&full).to_ascii_lowercase();
    let parent = util::filename_path(&full);
    let cache_path = if is_dir && path.join("CMakeCache.txt").is_file() {
        Some(full.as_str())
    } else if exists && leaf == "cmakecache.txt" && !is_dir {
        Some(parent.as_str())
    } else {
        None
    };
    if let Some(dir) = cache_path {
        let mut cache = Cache::default();
        match cache.load(Path::new(dir)) {
            Ok(true) => {
                options.cache_loaded = true;
                if let Some(source) = cache.value("CMAKE_HOME_DIRECTORY") {
                    options.source_dir = source.to_owned();
                    options.binary_dir = dir.to_owned();
                    return true;
                }
            }
            Err(error) => cmcmd::error(error),
            _ => {}
        }
    }

    let list_path = if is_source {
        &full
    } else if (!exists && leaf == "cmakecache.txt") || (!is_dir && leaf == "cmakelists.txt") {
        &parent
    } else if !exists {
        &full
    } else {
        ""
    };
    let no_source = options.source_dir.is_empty();
    let no_build = options.binary_dir.is_empty();
    let used =
        list_path == options.source_dir || list_path == options.binary_dir || is_source || no_build;
    if is_source {
        set_source(options, full.clone(), script_mode);
        if no_build {
            options.binary_dir = cwd.to_owned();
        }
    } else if !list_path.is_empty() {
        if no_build && no_source {
            options.source_dir = list_path.to_owned();
            options.binary_dir = cwd.to_owned();
        } else if no_build {
            options.binary_dir = list_path.to_owned();
        }
    } else {
        if no_source {
            options.source_dir = full.clone();
        }
        if no_build {
            if !no_source && is_dir && cache_path.is_none() {
                options.binary_dir = full;
            } else {
                options.binary_dir = cwd.to_owned();
                return false;
            }
        }
    }
    used
}

const VALUE_OPTIONS: &[(&str, &str, bool)] = &[
    ("-S", "No source directory specified for -S", true),
    ("-H", "No source directory specified for -H", true),
    ("-B", "No build directory specified for -B", true),
    ("-C", "-C must be followed by a file name.", true),
    ("-D", "-D must be followed with VAR=VALUE.", true),
    ("-U", "-U must be followed with VAR.", true),
    ("-W", "-W must be followed with [no-]<name>.", true),
    ("-A", "No platform specified for -A", true),
    ("-T", "No toolset specified for -T", true),
    ("-G", "No generator specified for -G", true),
    ("--toolchain", "No file specified for --toolchain", false),
    (
        "--install-prefix",
        "No install directory specified for --install-prefix",
        false,
    ),
    ("--graphviz", "No file specified for --graphviz", false),
    (
        "--log-level",
        "Invalid level specified for --log-level",
        false,
    ),
    (
        "--loglevel",
        "Invalid level specified for --loglevel",
        false,
    ),
    (
        "--trace-format",
        "Invalid format specified for --trace-format",
        false,
    ),
    (
        "--trace-source",
        "No file specified for --trace-source",
        false,
    ),
    (
        "--trace-redirect",
        "No file specified for --trace-redirect",
        false,
    ),
    ("--preset", "No preset specified for --preset", false),
    (
        "--debugger-pipe",
        "No path specified for --debugger-pipe",
        false,
    ),
    (
        "--debugger-dap-log",
        "No file specified for --debugger-dap-log",
        false,
    ),
    (
        "--project-file",
        "No filename specified for --project-file",
        false,
    ),
    (
        "--profiling-output",
        "No path specified for --profiling-output",
        false,
    ),
    (
        "--profiling-format",
        "No format specified for --profiling-format",
        false,
    ),
    (
        "--sarif-output",
        "No file specified for --sarif-output",
        false,
    ),
    (
        "--debug-find-pkg",
        "Provide a package argument for --debug-find-pkg",
        false,
    ),
    (
        "--debug-find-var",
        "Invalid value used with --debug-find-var",
        false,
    ),
];

struct CacheArg<'a> {
    name: &'a str,
    value: String,
}

fn project_or_script(argv: &[String], cwd: &str) -> i32 {
    configure_args(argv, cwd).unwrap_or(1)
}

fn configure_args(argv: &[String], cwd: &str) -> Result<i32, ()> {
    let args = &argv[1..];
    if args
        .iter()
        .take_while(|arg| arg.as_str() != "--")
        .any(|arg| arg == "-i")
    {
        eprintln!(
            "The \"cmake -i\" wizard mode is no longer supported.\nUse the -D option to set cache values on the command line.\nUse cmake-gui or ccmake for an interactive dialog."
        );
        return Ok(1);
    }
    let script_mode = args
        .iter()
        .take_while(|arg| arg.as_str() != "--")
        .any(|arg| arg.starts_with("-P"));
    if !script_mode {
        warn_orphan_generator_settings();
    }
    let mut options = ConfigureOptions {
        argv: argv.to_vec(),
        ..Default::default()
    };
    let mut view = None::<CacheView>;
    let mut view_only = false;
    let mut regenerate = false;
    let mut cache_args = Vec::new();
    let mut unknown = None;
    let mut extra = None;
    let mut unsupported = None;
    let mut bad_generator = false;
    let mut trace_file_failed = false;
    let mut preset_name = String::new();
    let mut list_presets = None;
    let mut have_b_arg = false;
    let mut profiling_format = String::new();
    let mut profiling_output = String::new();
    let mut index = 0;
    while index < args.len() {
        let arg = args[index].as_str();
        if arg == "--" && script_mode {
            break;
        }
        if matches_value(arg, "-P", true) {
            let value = read_value(
                args,
                &mut index,
                "-P",
                Values::One,
                "No script specified for argument -P",
            )?;
            cache_args.push(CacheArg { name: "-P", value });
        } else if let Some(&(name, error, _)) = VALUE_OPTIONS
            .iter()
            .find(|&&(name, _, attached)| matches_value(arg, name, attached))
        {
            let values = if name == "--trace-source" {
                Values::Many
            } else {
                Values::One
            };
            let parsed = read_value(args, &mut index, name, values, error);
            let Ok(value) = parsed else {
                if name == "-G" {
                    print_generators();
                    return Ok(1);
                }
                return Ok(cli_failure());
            };
            if let Err(error) = set_option(
                &mut options,
                &mut cache_args,
                name,
                &value,
                cwd,
                script_mode,
            ) {
                cmcmd::error(error);
                if name == "-G" {
                    print_generators();
                    bad_generator = true;
                    index += 1;
                    continue;
                }
                if name == "--trace-redirect" {
                    trace_file_failed = true;
                    index += 1;
                    continue;
                }
                return Ok(cli_failure());
            }
            match name {
                "--preset" => preset_name = value.clone(),
                // cmSystemTools::ToNormalizedPathOnDisk
                "--graphviz" => options.graphviz = Some(normalized_path(&value, cwd)),
                "--profiling-format" => profiling_format = value.clone(),
                "--debug-find-pkg" => {
                    let packages = tokenize_commas(&value);
                    print!("Running with debug output on for the 'find' commands for package(s)");
                    for package in packages {
                        print!(" {package}");
                        options.debug.find_pkgs.insert(package.to_owned());
                    }
                    println!(".");
                }
                "--debug-find-var" => {
                    let vars = tokenize_commas(&value);
                    print!("Running with debug output on for the variable(s)");
                    for var in vars {
                        print!(" {var}");
                        options.debug.find_vars.insert(var.to_owned());
                    }
                    println!(".");
                }
                "--profiling-output" => profiling_output = normalized_path(&value, cwd),
                "-B" => have_b_arg = true,
                _ => {}
            }
            if matches!(
                name,
                "--project-file"
                    | "--debugger-pipe"
                    | "--debugger-dap-log"
            ) {
                unsupported = Some(name);
            }
        } else if matches_value(arg, "--list-presets", false) {
            let value = read_value(args, &mut index, "--list-presets", Values::Optional, "")?;
            match crate::presets::cli::ListPresets::parse(&value) {
                Some(list) => list_presets = Some(list),
                None => return Ok(cli_failure()),
            }
        } else if matches!(arg, "-L" | "-LA" | "-LH" | "-LAH")
            || ["-LRAH", "-LRA", "-LRH", "-LR"]
                .iter()
                .any(|name| matches_value(arg, name, false))
        {
            let name = arg.split(['=', ' ']).next().unwrap_or_default();
            let settings = view.get_or_insert_with(CacheView::default);
            settings.all |= name.contains('A');
            settings.help |= name.contains('H');
            if name.contains('R') {
                let value = read_value(
                    args,
                    &mut index,
                    name,
                    Values::One,
                    &format!("Invalid value used with {name}"),
                )?;
                settings.filter = crate::cmregex::Regex::new(&value).ok();
            }
        } else {
            match arg {
                "" => warning("Ignoring empty string (\"\") provided on the command line."),
                "-N" => view_only = true,
                "--regenerate-during-build" => regenerate = true,
                "--fresh" => options.fresh = true,
                "--log-context" => options.log_context = true,
                "--debug-trycompile" => {
                    println!("debug trycompile on");
                    options.debug_trycompile = true;
                }
                "--trace" | "--trace-expand" => {
                    let message = if arg == "--trace" {
                        "Put cmake in trace mode."
                    } else {
                        "Put cmake in trace mode, but with variables expanded."
                    };
                    println!("{message}");
                    options.trace = true;
                    options.trace_expand = arg == "--trace-expand";
                }
                "--warn-uninitialized" => {
                    println!("Warn about uninitialized values.");
                    options.warn_uninitialized = true;
                }
                "--debug-output" => {
                    println!("Running with debug output on.");
                    options.debug.output = true;
                }
                "--debug-find" => {
                    println!("Running with debug output on for the `find` commands.");
                    options.debug.find = true;
                }
                "--no-warn-unused-cli" => {
                    println!("Not searching for unused variables given on the command line.");
                    options.no_warn_unused_cli = true;
                }
                "--check-system-vars" => {
                    println!(
                        "Also check system files when warning about unused and uninitialized variables."
                    );
                    options.check_system_vars = true;
                }
                "--compile-no-warning-as-error" => println!(
                    "Ignoring COMPILE_WARNING_AS_ERROR target property and CMAKE_COMPILE_WARNING_AS_ERROR variable."
                ),
                "--link-no-warning-as-error" => println!(
                    "Ignoring LINK_WARNING_AS_ERROR target property and CMAKE_LINK_WARNING_AS_ERROR variable."
                ),
                "-O" | "--warn-unused-vars" => {}
                "--system-information" | "--find-package" | "--debugger" => {
                    unsupported = Some(arg)
                }
                "--explain" => {
                    let Some(target) = args.get(index + 1) else {
                        cmcmd::error("--explain requires a target and flag");
                        return Ok(cli_failure());
                    };
                    let Some(flag) = args.get(index + 2) else {
                        cmcmd::error("--explain requires a target and flag");
                        return Ok(cli_failure());
                    };
                    options.explain = Some((target.clone(), flag.clone()));
                    index += 2;
                }
                _ if arg.starts_with('-') => unknown = Some(arg),
                _ if !script_mode && !positional(&mut options, arg, cwd, false) => {
                    extra = Some(arg)
                }
                _ => {}
            }
        }
        index += 1;
    }
    if let Some(path) = extra {
        extra_path(path);
    }
    if let Some(arg) = unknown.filter(|_| !script_mode) {
        cmcmd::error(format!("Unknown argument {arg}"));
        return Ok(cli_failure());
    }
    if let Some(flag) = unsupported {
        if flag.starts_with("--debugger") {
            cmcmd::error(format!("CMake was not built with support for {flag}"));
            return Ok(cli_failure());
        }
        cmcmd::error(format!("{flag} is not supported"));
        return Ok(1);
    }

    // The end of cmake::SetArgs.
    if !profiling_output.is_empty() || !profiling_format.is_empty() {
        if profiling_output.is_empty() {
            cmcmd::error("--profiling-format specified but no --profiling-output!");
            return Ok(1);
        }
        if profiling_format == "google-trace" {
            if let Err(error) = crate::profiling::start(&profiling_output) {
                cmcmd::error(format!("Could not start profiling: {error}"));
                return Ok(1);
            }
        } else {
            cmcmd::error("Invalid format specified for --profiling-format");
            return Ok(1);
        }
    }

    if script_mode && options.fresh {
        cmcmd::error("--fresh allowed only when configuring a project");
        return Ok(1);
    }
    if !script_mode
        && !regenerate
        && options.source_dir.is_empty()
        && options.binary_dir.is_empty()
        && preset_name.is_empty()
        && list_presets.is_none()
    {
        warning(IMPLICIT_DIRECTORIES);
    }
    if bad_generator || trace_file_failed {
        return Ok(1);
    }
    if options.binary_dir.is_empty() {
        options.binary_dir = cwd.to_owned();
    }
    let mut preset_state = None;
    if !script_mode {
        let home = if options.source_dir.is_empty() {
            cwd.to_owned()
        } else {
            options.source_dir.clone()
        };
        match crate::presets::cli::apply_configure_preset(
            &home,
            list_presets,
            &preset_name,
            have_b_arg,
            &mut options,
        ) {
            Ok(state) => preset_state = state,
            Err(code) => return Ok(code),
        }
    }
    crate::configure::print_trace_format_version(&options);
    let mut cache = Cache::default();
    if !script_mode && (!options.fresh || regenerate) {
        if let Err(error) = cache.load(Path::new(&options.binary_dir)) {
            cmcmd::error(error);
            cmcmd::error("Error executing cmake::LoadCache(). Aborting.\n");
            return Ok(1);
        }
        if view_only {
            // Configuring reloads (and reports) the cache itself.
            for error in std::mem::take(&mut cache.load_errors) {
                cmcmd::error(error);
            }
        }
    }
    if regenerate {
        if let Some(source) = cache.value("CMAKE_HOME_DIRECTORY") {
            options.source_dir = source.to_owned();
        } else {
            cmcmd::error("Could not load CMAKE_HOME_DIRECTORY for --regenerate-during-build");
            return Ok(1);
        }
        options.generator = cache.value("CMAKE_GENERATOR").map(str::to_owned);
        options.platform = cache.value("CMAKE_GENERATOR_PLATFORM").map(str::to_owned);
        options.toolset = cache.value("CMAKE_GENERATOR_TOOLSET").map(str::to_owned);
    }
    if options.source_dir.is_empty() {
        options.source_dir = cwd.to_owned();
    }
    if script_mode {
        let mut script_argv = argv.to_vec();
        if !args.iter().any(|arg| arg == "--") {
            normalize_script_args(&mut script_argv);
        }
        return Ok(run_script_mode(
            &script_argv,
            cwd,
            &options,
            &cache_args,
            view,
        ));
    }
    if let Err(error) = process_cache_args(&mut options, &cache_args, cwd) {
        cmcmd::error(error);
        return Ok(cli_failure());
    }
    if let Some(state) = &preset_state
        && matches!(options.log_level.as_deref(), Some("verbose" | "debug" | "trace"))
    {
        // Entries the command line set or removed are no longer
        // "unprocessed" preset variables.
        let overridden = |name: &str| {
            cache_args.iter().any(|arg| match arg.name {
                "-D" => cache::parse_entry(&arg.value).is_some_and(|(var, _, _)| var == name),
                "-U" => glob_to_regex_match(&arg.value, name),
                "--install-prefix" => name == "CMAKE_INSTALL_PREFIX",
                "--toolchain" => name == "CMAKE_TOOLCHAIN_FILE",
                _ => false,
            })
        };
        // cmake::Run prints them after SetCacheArgs ran the -C scripts.
        options.preset_report = Some(state.render(&overridden));
    }
    let result = if view_only {
        let result = apply_cache_view_args(&mut cache, &cache_args, &options, cwd);
        if let Some(report) = &options.preset_report {
            print!("{report}");
        }
        result
    } else if !options.preload_scripts.is_empty()
        && options
            .preload_scripts
            .iter()
            .all(|path| !Path::new(path).is_file())
        && !validate_project_source(&options, &cache_args)
    {
        1
    } else {
        let result = configure::configure(&options);
        if view.is_some() {
            let _ = cache.load(Path::new(&options.binary_dir));
        }
        result
    };
    if let Some(view) = view {
        view.print(&cache);
    }
    Ok(result)
}

pub(crate) fn print_generators() {
    eprint!("{}", generator_documentation(GENERATORS));
}

fn set_option<'a>(
    options: &mut ConfigureOptions,
    cache_args: &mut Vec<CacheArg<'a>>,
    name: &'a str,
    value: &str,
    cwd: &str,
    script_mode: bool,
) -> Result<(), String> {
    match name {
        "-S" | "-H" if value.is_empty() => {
            return Err(format!("No source directory specified for {name}"));
        }
        "-B" if value.is_empty() => return Err("No build directory specified for -B".to_owned()),
        // cmSystemTools::ToNormalizedPathOnDisk
        "-S" | "-H" => set_source(options, normalized_path(value, cwd), script_mode),
        "-B" => options.binary_dir = normalized_path(value, cwd),
        "-G" => {
            if !known_generator(value) {
                return Err(format!("Could not create named generator {value}"));
            }
            options.generator = Some(value.to_owned());
        }
        "-A" | "-T" => {
            let slot = if name == "-A" {
                &mut options.platform
            } else {
                &mut options.toolset
            };
            if slot.is_some() {
                return Err(format!("Multiple {name} options not allowed"));
            }
            *slot = Some(value.to_owned());
        }
        "-C" | "-D" | "-U" | "-W" | "--toolchain" | "--install-prefix" => {
            cache_args.push(CacheArg {
                name,
                value: value.to_owned(),
            })
        }
        "--log-level" | "--loglevel" => {
            let level = value.to_ascii_lowercase();
            if ![
                "error", "warning", "notice", "status", "verbose", "debug", "trace",
            ]
            .contains(&level.as_str())
            {
                return Err(format!("Invalid level specified for {name}"));
            }
            options.log_level = Some(level);
        }
        "--trace-format" => {
            println!("Put cmake in trace mode and sets the trace output format.");
            options.trace = true;
            match value.to_ascii_lowercase().as_str() {
                "human" => options.trace_json = false,
                "json-v1" => options.trace_json = true,
                _ => {
                    return Err("Invalid format specified for --trace-format. Valid formats are human, json-v1.".to_owned());
                }
            }
        }
        "--trace-source" => {
            println!(
                "Put cmake in trace mode, but output only lines of a specified file. Multiple options are allowed."
            );
            for file in value.split(';') {
                options.trace_sources.push(cache::convert_to_unix_slashes(file));
            }
            options.trace = true;
        }
        "--trace-redirect" => {
            println!(
                "Put cmake in trace mode and redirect trace output to a file instead of stderr."
            );
            let file = cache::convert_to_unix_slashes(value);
            if let Err(error) = std::fs::File::create(&file) {
                return Err(format!(
                    "Error opening trace file {file}: {}",
                    cmcmd::os_error(&error)
                ));
            } else {
                println!("Trace will be written to {file}");
                options.trace_redirect = Some(file.clone());
            }
            options.trace = true;
        }
        _ => {}
    }
    Ok(())
}

fn cache_definition(value: &str) -> Result<CacheDefinition, String> {
    let Some((name, value, kind)) = cache::parse_entry(value) else {
        return Err(format!(
            "Parse error in command line argument: {value}\n Should be: VAR:type=value\n"
        ));
    };
    Ok(cache_definition_from_parts(
        &name,
        &value,
        (kind != CacheType::Uninitialized).then_some(kind),
    ))
}

/// A command-line style cache definition (`cmake::ProcessCacheArg` input),
/// with PATH/FILEPATH values normalized as `-D` does.
pub(crate) fn cache_definition_from_parts(name: &str, value: &str, kind: Option<CacheType>) -> CacheDefinition {
    let mut value = value.to_owned();
    if matches!(kind, Some(CacheType::Path | CacheType::Filepath)) {
        value = util::expand_list(&value, false)
            .iter()
            .map(|path| {
                if matches!(path.as_str(), "~") || path.starts_with("~/") {
                    util::collapse_full_path(path, "")
                } else {
                    cache::convert_to_unix_slashes(path)
                }
            })
            .collect::<Vec<_>>()
            .join(";");
    }
    CacheDefinition {
        name: name.to_owned(),
        value,
        kind,
    }
}

fn warning_option(options: &mut ConfigureOptions, value: &str) -> Result<(), String> {
    let (disabled, value) = value
        .strip_prefix("no-")
        .map_or((false, value), |name| (true, name));
    let (error, name) = value
        .strip_prefix("error=")
        .map_or((false, value), |name| (true, name));
    if name.is_empty() {
        return Err("No warning name provided.".to_owned());
    }
    let (warn, fatal) = match name {
        "dev" => (&mut options.warn_dev, &mut options.warn_dev_error),
        "deprecated" => (
            &mut options.warn_deprecated,
            &mut options.warn_deprecated_error,
        ),
        _ => return Ok(()),
    };
    match (disabled, error) {
        (false, false) => {
            *warn = Some(true);
        }
        (false, true) => {
            *warn = Some(true);
            *fatal = Some(true);
        }
        (true, false) => {
            *warn = Some(false);
            *fatal = Some(false);
        }
        (true, true) if *fatal == Some(true) => *fatal = Some(false),
        _ => {}
    }
    Ok(())
}

fn process_cache_args(
    options: &mut ConfigureOptions,
    args: &[CacheArg<'_>],
    cwd: &str,
) -> Result<(), String> {
    for CacheArg { name, value } in args {
        match *name {
            "-D" => options.definitions.push(cache_definition(value)?),
            "-C" if value.is_empty() => return Err("No file name specified for -C".to_owned()),
            "-C" => {
                options
                    .preload_scripts
                    .push(util::collapse_full_path(value, cwd));
                options.preload_script_args.push(value.clone());
            }
            "-U" => options.undefine.push(value.clone()),
            "-W" => warning_option(options, value)?,
            "--toolchain" => options.toolchain_file = Some(value.clone()),
            "--install-prefix" if Path::new(value).is_absolute() => {
                options.install_prefix = Some(value.clone())
            }
            "--install-prefix" => {
                return Err("Absolute paths are required for --install-prefix".to_owned());
            }
            _ => {}
        }
    }
    Ok(())
}

fn unset_cache_entries(cache: &mut Cache, pattern: &str) {
    use std::ffi::CString;
    let Ok(pattern) = CString::new(pattern) else {
        return;
    };
    cache.entries.retain(|key, entry| {
        if entry.kind == CacheType::Static {
            return true;
        }
        let Ok(key) = CString::new(key.as_str()) else {
            return true;
        };
        // KWSys cache globs treat backslashes literally and wildcards do
        // not cross slash boundaries.
        unsafe {
            libc::fnmatch(
                pattern.as_ptr(),
                key.as_ptr(),
                libc::FNM_NOESCAPE | libc::FNM_PATHNAME,
            ) != 0
        }
    });
}

fn validate_project_source(options: &ConfigureOptions, cache_args: &[CacheArg<'_>]) -> bool {
    let source = Path::new(&options.source_dir);
    let problem = if !source.exists() {
        Some("does not exist")
    } else if !source.is_dir() {
        Some("is a file, not a directory")
    } else if !source.join("CMakeLists.txt").exists() {
        Some("does not appear to contain CMakeLists.txt")
    } else {
        None
    };
    if let Some(problem) = problem {
        // The configure pipeline processes initial caches before its project
        // validation. Preserve the missing-preload diagnostics on this path.
        let mut preload_paths = options.preload_scripts.iter();
        for arg in cache_args.iter().filter(|arg| arg.name == "-C") {
            if let Some(file) = preload_paths.next() {
                println!("loading initial cache file {}", arg.value);
                if !Path::new(file).is_file() {
                    cmcmd::error(format!("Not a file: {file}"));
                    cmcmd::error(format!("Error processing file: {file}"));
                }
            }
        }
        cmcmd::error(format!(
            "The source directory \"{}\" {problem}.\nSpecify --help for usage, or press the help button on the CMake GUI.",
            options.source_dir
        ));
        return false;
    }
    true
}

fn normalize_script_args(argv: &mut Vec<String>) {
    let mut index = 1;
    while index < argv.len() {
        if let Some(value) = argv[index]
            .strip_prefix("-P")
            .filter(|value| !value.is_empty())
        {
            let value = value.strip_prefix('=').unwrap_or(value);
            let value = value.strip_prefix(' ').unwrap_or(value).to_owned();
            argv[index] = "-P".to_owned();
            argv.insert(index + 1, value);
            index += 1;
        } else if argv[index] == "-P" {
            index += 1;
        }
        index += 1;
    }
}

fn make_script_interp(argv: &[String], cwd: &str) -> Option<Interp> {
    let Some(root) = configure::find_cmake_root() else {
        cmcmd::error(format!(
            "Could not find CMAKE_ROOT !!!\nCMake has most likely not been installed correctly.\nModules directory not found in\n{}",
            std::env::current_exe().unwrap_or_default().display()
        ));
        return None;
    };
    let mut interp = Interp::new(Mode::Script, cwd, cwd, &root);
    configure::add_default_definitions(&mut interp);
    configure::add_cmake_paths(&mut interp);
    interp.set_def("CMAKE_ARGC", argv.len().to_string());
    for (position, arg) in argv.iter().enumerate() {
        let value = if position == 0 {
            interp.cmake_command.clone()
        } else {
            arg.clone()
        };
        interp.set_def(format!("CMAKE_ARGV{position}"), value);
    }
    for key in [
        "CMAKE_SOURCE_DIR",
        "CMAKE_BINARY_DIR",
        "CMAKE_CURRENT_SOURCE_DIR",
        "CMAKE_CURRENT_BINARY_DIR",
    ] {
        interp.set_def(key, cwd);
    }
    Some(interp)
}

fn read_script(interp: &mut Interp, script: &str, cwd: &str, script_mode: bool) -> i32 {
    if script.is_empty() {
        return 0;
    }
    let path = util::collapse_full_path(script, cwd);
    if script_mode {
        interp.set_def("CMAKE_SCRIPT_MODE_FILE", &path);
    }
    let metadata = std::fs::metadata(&path);
    if metadata
        .as_ref()
        .is_ok_and(|entry| entry.file_type().is_fifo())
    {
        eprintln!(
            "CMake Error in {script}:\n  Error while reading Byte-Order-Mark.  File not seekable?\n\n"
        );
        cmcmd::error(format!("Error processing file: {script}"));
        return 1;
    }
    if !metadata.is_ok_and(|entry| entry.is_file()) {
        cmcmd::error(format!("Not a file: {script}"));
        cmcmd::error(format!("Error processing file: {script}"));
        return 1;
    }
    if !interp.read_list_file(&path) {
        cmcmd::error(format!("Error processing file: {script}"));
    }
    interp
        .exit_code
        .unwrap_or_else(|| i32::from(interp.error || interp.fatal))
}

fn add_definition(cache: &mut Cache, definition: &CacheDefinition) {
    cache.add(
        &definition.name,
        Some(&definition.value),
        Some("No help, variable specified on the command line."),
        definition.kind.unwrap_or(CacheType::Uninitialized),
    );
}

fn apply_cache_view_args(
    cache: &mut Cache,
    args: &[CacheArg<'_>],
    options: &ConfigureOptions,
    cwd: &str,
) -> i32 {
    let mut result = 0;
    let mut definitions = options.definitions.iter();
    for CacheArg { name, value } in args {
        match *name {
            "-D" => {
                if let Some(definition) = definitions.next() {
                    add_definition(cache, definition);
                }
            }
            "-U" => unset_cache_entries(cache, value),
            "--toolchain" => cache.add(
                "CMAKE_TOOLCHAIN_FILE",
                Some(value),
                Some("No help, variable specified on the command line."),
                CacheType::Filepath,
            ),
            "--install-prefix" => cache.add(
                "CMAKE_INSTALL_PREFIX",
                Some(value),
                Some("No help, variable specified on the command line."),
                CacheType::Path,
            ),
            "-C" => {
                let file = util::collapse_full_path(value, cwd);
                println!("loading initial cache file {value}");
                let Some(root) = configure::find_cmake_root() else {
                    cmcmd::error("Could not find CMAKE_ROOT !!!");
                    return 1;
                };
                let mut interp = Interp::new(
                    Mode::Project,
                    &options.source_dir,
                    &options.binary_dir,
                    &root,
                );
                interp.cache = std::mem::take(cache);
                configure::add_default_definitions(&mut interp);
                configure::add_cmake_paths(&mut interp);
                result |= i32::from(read_script(&mut interp, &file, cwd, false) != 0);
                *cache = interp.cache;
            }
            _ => {}
        }
    }
    result
}

fn run_script_mode(
    argv: &[String],
    cwd: &str,
    options: &ConfigureOptions,
    args: &[CacheArg<'_>],
    view: Option<CacheView>,
) -> i32 {
    let Some(mut interp) = make_script_interp(argv, cwd) else {
        return 1;
    };
    crate::configure::install_trace(&mut interp, options);
    interp.debug = options.debug.clone();
    if let Some(level) = &options.log_level {
        interp
            .ext
            .misc
            .insert("log-level".to_owned(), level.clone());
    }
    if options.log_context {
        interp
            .ext
            .misc
            .insert("log-context".to_owned(), "1".to_owned());
    }
    let mut result = 0;
    for CacheArg { name, value } in args {
        match *name {
            "-D" => match cache_definition(value) {
                Ok(definition) => add_definition(&mut interp.cache, &definition),
                Err(error) => {
                    cmcmd::error(error);
                    return cli_failure();
                }
            },
            "-U" => unset_cache_entries(&mut interp.cache, value),
            "-C" => {
                println!("loading initial cache file {value}");
                result = read_script(
                    &mut interp,
                    &util::collapse_full_path(value, cwd),
                    cwd,
                    false,
                );
            }
            "-P" => result = read_script(&mut interp, value, cwd, true),
            _ => {}
        }
        if result != 0 || interp.exit_code.is_some() {
            break;
        }
    }
    if let Some(view) = view {
        view.print(&interp.cache);
    }
    result
}

#[derive(Default)]
struct BuildOptions {
    preset_name: String,
    list_presets: bool,
    dir: String,
    targets: Vec<String>,
    config: String,
    jobs: Option<u32>,
    native: Vec<String>,
    verbose: bool,
    clean_first: bool,
    preset: bool,
}

fn parse_jobs(value: &str, flag: &str) -> Result<u32, ()> {
    if value.is_empty() {
        return Ok(0);
    }
    let env = flag == "CMAKE_BUILD_PARALLEL_LEVEL";
    let parsed = value.trim_start().parse::<u64>();
    match parsed {
        Ok(0) if env => eprintln!(
            "The CMAKE_BUILD_PARALLEL_LEVEL environment variable requires a positive integer argument.\n"
        ),
        Ok(0) => eprintln!("The <jobs> value requires a positive integer argument.\n"),
        Ok(value) if value <= i32::MAX as u64 => return Ok(value as u32),
        Ok(_) if env => {
            eprintln!("The CMAKE_BUILD_PARALLEL_LEVEL environment variable is too large.\n")
        }
        Ok(_) => eprintln!("The <jobs> value is too large.\n"),
        Err(_) if env => eprintln!(
            "'CMAKE_BUILD_PARALLEL_LEVEL' environment variable\ninvalid number '{value}' given.\n"
        ),
        Err(_) => eprintln!("'{flag}' invalid number '{value}' given.\n"),
    }
    Err(())
}

fn parse_build(args: &[String], cwd: &str) -> Result<BuildOptions, ()> {
    let mut options = BuildOptions {
        verbose: std::env::var_os("VERBOSE").is_some(),
        ..Default::default()
    };
    let mut index = 0;
    while index < args.len() {
        let arg = &args[index];
        match arg.as_str() {
            "--" => {
                options.native = args[index + 1..].to_vec();
                break;
            }
            "-v" | "--verbose" => options.verbose = true,
            "--clean-first" => options.clean_first = true,
            "--list-presets" => {
                options.preset = true;
                options.list_presets = true;
            }
            "--use-stderr" => {}
            _ => {
                let names = [
                    "--preset",
                    "-j",
                    "--parallel",
                    "-t",
                    "--target",
                    "--config",
                    "--resolve-package-references",
                ];
                if let Some(name) = names
                    .into_iter()
                    .find(|name| matches_value(arg, name, matches!(*name, "-j" | "--parallel")))
                {
                    let arity = match name {
                        "-j" | "--parallel" => Values::Optional,
                        "-t" | "--target" => Values::Many,
                        _ => Values::One,
                    };
                    let value = read_value(
                        args,
                        &mut index,
                        name,
                        arity,
                        &format!("Invalid value used with {name}"),
                    )?;
                    match name {
                        "-j" | "--parallel" => options.jobs = Some(parse_jobs(&value, name)?),
                        "--config" => options.config = value,
                        "--preset" => {
                            options.preset |= !value.is_empty();
                            options.preset_name = value;
                        }
                        "-t" | "--target" if !value.is_empty() => {
                            options.targets.extend(util::expand_list(&value, false))
                        }
                        "--resolve-package-references"
                            if matches!(
                                value.to_ascii_lowercase().as_str(),
                                "on" | "off" | "only"
                            ) => {}
                        _ => return Err(()),
                    }
                } else if index == 0 {
                    options.dir = util::collapse_full_path(arg, cwd);
                } else {
                    eprintln!("Unknown argument {arg}");
                    return Err(());
                }
            }
        }
        index += 1;
    }
    if options.targets.iter().any(|target| target == "clean")
        && options.targets.iter().any(|target| target != "clean")
    {
        eprintln!("Error: Building 'clean' and other targets together is not supported.");
        return Err(());
    }
    if options.jobs.is_none() {
        if let Ok(value) = std::env::var("CMAKE_BUILD_PARALLEL_LEVEL") {
            options.jobs = Some(parse_jobs(&value, "CMAKE_BUILD_PARALLEL_LEVEL")?);
        }
    }
    if options.dir.is_empty() && !options.preset {
        return Err(());
    }
    Ok(options)
}

fn build(args: &[String], cwd: &str) -> i32 {
    let Ok(mut options) = parse_build(args, cwd) else {
        eprint!("{}", include_str!("cmcmd/build-usage.txt"));
        return 1;
    };
    if options.preset {
        let settings = crate::presets::cli::BuildSettings {
            dir: &mut options.dir,
            jobs: &mut options.jobs,
            targets: &mut options.targets,
            config: &mut options.config,
            clean_first: &mut options.clean_first,
            verbose: &mut options.verbose,
            native: &mut options.native,
        };
        if let Err(code) = crate::presets::cli::apply_build_preset(
            cwd,
            &options.preset_name,
            options.list_presets,
            settings,
        ) {
            return code;
        }
    }
    let dir = Path::new(&options.dir);
    if !dir.is_dir() {
        eprintln!("Error: {} is not a directory", options.dir);
        return 1;
    }
    let cache_dir = if !dir.join("CMakeCache.txt").exists() && dir.join("CMakeFiles").exists() {
        dir.ancestors()
            .find(|path| path.join("CMakeCache.txt").exists())
            .unwrap_or(dir)
    } else {
        dir
    };
    let mut cache = Cache::default();
    match cache.load(cache_dir) {
        Ok(true) => {
            for error in std::mem::take(&mut cache.load_errors) {
                cmcmd::error(error);
            }
        }
        Err(error) => {
            cmcmd::error(error);
            return 1;
        }
        Ok(false) => {
            eprintln!("Error: not a CMake build directory (missing CMakeCache.txt)");
            return 1;
        }
    }
    let Some(generator) = cache.value("CMAKE_GENERATOR") else {
        eprintln!("Error: could not find CMAKE_GENERATOR in Cache");
        return 1;
    };
    if !matches!(generator, "Ninja" | "Ninja Multi-Config") {
        eprintln!("Error: could not create CMAKE_GENERATOR \"{generator}\"");
        return 1;
    }
    for (key, setting) in [
        ("CMAKE_GENERATOR_INSTANCE", "instance"),
        ("CMAKE_GENERATOR_PLATFORM", "platform"),
        ("CMAKE_GENERATOR_TOOLSET", "toolset"),
    ] {
        if let Some(value) = cache.value(key).filter(|value| !value.is_empty()) {
            let message = format!(
                "Generator\n  {generator}\ndoes not support {setting} specification, but {setting}\n  {value}\nwas specified."
            );
            eprintln!(
                "{}",
                crate::messages::render(
                    crate::messages::MessageType::FatalError,
                    &message,
                    &[],
                    None
                )
            );
            return 1;
        }
    }
    if cache.value("CMAKE_PROJECT_NAME").is_none() {
        eprintln!("Error: could not find CMAKE_PROJECT_NAME in Cache");
        return 1;
    }
    let verbose = options.verbose || util::is_on_opt(cache.value("CMAKE_VERBOSE_MAKEFILE"));
    let make_program = cache
        .value("CMAKE_MAKE_PROGRAM")
        .filter(|program| !util::is_off(program))
        .unwrap_or("");
    let mut command = vec![make_program.to_owned()];
    if verbose {
        println!("Change Dir: '{}'", options.dir);
        command.push("-v".to_owned());
    }
    if let Some(jobs) = options.jobs.filter(|jobs| *jobs > 0) {
        command.extend(["-j".to_owned(), jobs.to_string()]);
    }
    if generator == "Ninja Multi-Config" && !options.config.is_empty() {
        command.extend(["-f".to_owned(), format!("build-{}.ninja", options.config)]);
    }
    if options.clean_first {
        let mut clean = command.clone();
        clean.push("clean".to_owned());
        if verbose {
            println!("\nRun Clean Command: {}", quote_command(&clean));
        }
        if Command::new(&clean[0])
            .args(&clean[1..])
            .current_dir(dir)
            .status()
            .is_err()
        {
            cmcmd::error("Generator: execution of make clean failed.");
            return 1;
        }
    }
    command.extend(options.native);
    command.extend(options.targets);
    if verbose {
        println!("\nRun Build Command(s): {}", quote_command(&command));
    }
    let result = match Command::new(&command[0])
        .args(&command[1..])
        .current_dir(dir)
        .status()
    {
        Ok(status) => status.code().unwrap_or(1),
        Err(error) => {
            eprintln!("{}", cmcmd::process_error(&error));
            cmcmd::error(format!(
                "Generator: build tool execution failed, command was: {}",
                command.join(" ")
            ));
            if verbose {
                println!(
                    "\nGenerator: build tool execution failed, command was: {}",
                    quote_command(&command)
                );
            }
            return 1;
        }
    };
    if verbose {
        println!();
    }
    result
}

fn quote_command(command: &[String]) -> String {
    command
        .iter()
        .map(|arg| {
            if arg.is_empty()
                || arg.contains(|ch: char| ch.is_whitespace() || "\\\"`$&;|()<>*?#~'".contains(ch))
            {
                let value = arg
                    .replace('\\', "\\\\")
                    .replace('"', "\\\"")
                    .replace('$', "\\$")
                    .replace('`', "\\`");
                format!("\"{value}\"")
            } else {
                arg.clone()
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn install(program: &str, args: &[String], cwd: &str) -> i32 {
    let Some((dir, args)) = args.split_first() else {
        eprint!("{}", include_str!("cmcmd/install-usage.txt"));
        return 1;
    };
    let dir = util::collapse_full_path(dir, cwd);
    let mut variables = std::collections::BTreeMap::new();
    let mut strip = false;
    let mut index = 0;
    while index < args.len() {
        let arg = &args[index];
        match arg.as_str() {
            "--strip" => strip = true,
            "-v" | "--verbose" => {}
            _ => {
                let names = [
                    "--config",
                    "--component",
                    "--default-directory-permissions",
                    "--prefix",
                    "-j",
                    "--parallel",
                ];
                let Some(name) = names
                    .into_iter()
                    .find(|name| matches_value(arg, name, false))
                else {
                    eprintln!("Unknown argument {arg}");
                    eprint!("{}", include_str!("cmcmd/install-usage.txt"));
                    return 1;
                };
                let Ok(value) = read_value(
                    args,
                    &mut index,
                    name,
                    Values::One,
                    &format!("Invalid value used with {name}"),
                ) else {
                    eprint!("{}", include_str!("cmcmd/install-usage.txt"));
                    return 1;
                };
                if matches!(name, "-j" | "--parallel") {
                    if parse_jobs(&value, name).is_err() {
                        eprint!("{}", include_str!("cmcmd/install-usage.txt"));
                        return 1;
                    }
                } else {
                    variables.insert(name, value);
                }
            }
        }
        index += 1;
    }
    let mut argv = vec![program.to_owned()];
    for (flag, key) in [
        ("--prefix", "CMAKE_INSTALL_PREFIX"),
        ("--component", "CMAKE_INSTALL_COMPONENT"),
    ] {
        if let Some(value) = variables.get(flag).filter(|value| !value.is_empty()) {
            argv.push(format!("-D{key}={value}"));
        }
    }
    if strip {
        argv.push("-DCMAKE_INSTALL_DO_STRIP=1".to_owned());
    }
    if let Some(value) = variables
        .get("--default-directory-permissions")
        .filter(|value| !value.is_empty())
    {
        let Some(permissions) = directory_permissions(value) else {
            eprintln!("--default-directory-permissions is in incorrect format");
            return 1;
        };
        argv.push(format!(
            "-DCMAKE_INSTALL_DEFAULT_DIRECTORY_PERMISSIONS={permissions}"
        ));
    }
    if let Some(value) = variables.get("--config").filter(|value| !value.is_empty()) {
        argv.push(format!("-DCMAKE_INSTALL_CONFIG_NAME={value}"));
    }
    let script = format!("{dir}/cmake_install.cmake");
    argv.extend(["-P".to_owned(), script.clone()]);
    let Some(mut interp) = make_script_interp(&argv, cwd) else {
        return 1;
    };
    for arg in argv[1..argv.len() - 2]
        .iter()
        .filter_map(|arg| arg.strip_prefix("-D"))
    {
        if let Ok(definition) = cache_definition(arg) {
            add_definition(&mut interp.cache, &definition);
        }
    }
    i32::from(read_script(&mut interp, &script, cwd, true) != 0)
}

fn directory_permissions(value: &str) -> Option<String> {
    let mut permissions = Vec::new();
    for clause in value.split(',') {
        let (who, bits) = clause.split_once('=')?;
        let who = match who {
            "u" => "OWNER",
            "g" => "GROUP",
            "o" => "WORLD",
            _ => return None,
        };
        for bit in bits.chars() {
            let bit = match bit {
                'r' => "READ",
                'w' => "WRITE",
                'x' => "EXECUTE",
                _ => return None,
            };
            let permission = format!("{who}_{bit}");
            if !permissions.contains(&permission) {
                permissions.push(permission);
            }
        }
    }
    Some(permissions.join(";"))
}

/// Whether a `-U` glob matches a cache variable name.
fn glob_to_regex_match(pattern: &str, name: &str) -> bool {
    configure::glob_to_regex(pattern).find(name).is_some()
}
