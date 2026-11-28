//! Public command dispatch and project/script argument handling.

use std::os::unix::fs::FileTypeExt;
use std::path::Path;
use std::process::Command;

use crate::cache::{self, Cache, CacheType};
use crate::configure::{self, CacheArgument, CacheDefinition, ConfigureOptions};
use crate::interp::{self, Interp, Mode};
use crate::{cmcmd, util};

const GENERATORS: &str = include_str!("cmcmd/generators.txt");
const HELP: &str = include_str!("cmcmd/help.txt");
const HELP_HINT: &str = "Run 'cmake --help' for all supported options.";
const IMPLICIT_DIRECTORIES: &str = "No source or binary directory provided. Both will be assumed to be the same as the current working directory, but note that this warning will become a fatal error in future CMake releases.";

pub fn main() -> i32 {
    let argv: Vec<String> = std::env::args_os()
        .map(|arg| crate::bytes::decode_os(&arg))
        .collect();
    let Ok(current_dir) = std::env::current_dir() else {
        eprintln!("Current working directory cannot be established.");
        return 1;
    };
    let cwd = crate::pathresolver::logical_from_pwd(
        std::env::var_os("PWD").map(|value| crate::bytes::decode_os(&value)),
        &current_dir,
    );
    let args = &argv[1..];
    match args.first().map(String::as_str) {
        Some("-E") => cmcmd::dispatch(&argv[0], &args[1..]),
        Some("--build") => build(&argv, &cwd),
        Some("--install") => install(&argv, &cwd),
        Some("--workflow") => crate::presets::cli::workflow(&args[1..], &cwd),
        Some("--open") => open_project(&args[1..], &cwd),
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

/// cmakemain's `cmDocumentation::CheckOptions` / `PrintRequestedDocumentation`.
fn documentation(args: &[String]) -> Option<i32> {
    use crate::help::{Documentation, HelpType, check_options};
    let items = check_options(args)?;
    let root = configure::find_cmake_root().unwrap_or_default();
    let docs = Documentation::new(&root);
    let mut result = true;
    let mut count = 0;
    for item in items {
        let mut out = String::new();
        let ok = match item.kind {
            HelpType::Version => {
                out = format!(
                    "cmake version {}\n\nCMake suite maintained and supported by Kitware (kitware.com/cmake).\n",
                    interp::VERSION_STRING
                );
                true
            }
            HelpType::Usage => {
                out = include_str!("cmcmd/usage.txt").to_owned();
                true
            }
            HelpType::Help => {
                out = generator_documentation(HELP);
                true
            }
            _ => docs.print(&item, &mut out),
        };
        if item.filename.is_empty() {
            count += 1;
            if count > 1 {
                print!("\n\n");
            }
            print!("{out}");
        } else if std::fs::write(&item.filename, out).is_err() {
            result = false;
        }
        result &= ok;
    }
    Some(i32::from(!result))
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

/// cmake::LoadEnvironmentPresets: `CMAKE_GENERATOR` names the default
/// generator, and only then do `CMAKE_GENERATOR_{INSTANCE,PLATFORM,TOOLSET}`
/// seed the generator settings.
fn load_environment_presets(options: &mut ConfigureOptions) {
    let generator = std::env::var("CMAKE_GENERATOR").ok();
    for (variable, slot) in [
        (
            "CMAKE_GENERATOR_INSTANCE",
            &mut options.environment_instance,
        ),
        (
            "CMAKE_GENERATOR_PLATFORM",
            &mut options.environment_platform,
        ),
        ("CMAKE_GENERATOR_TOOLSET", &mut options.environment_toolset),
    ] {
        let Ok(value) = std::env::var(variable) else {
            continue;
        };
        if generator.is_some() {
            *slot = value;
        } else {
            eprintln!(
                "Warning: Environment variable {variable} will be ignored, because CMAKE_GENERATOR is not set."
            );
        }
    }
    options.environment_generator = generator;
}

pub(crate) fn known_generator(name: &str) -> bool {
    if crate::generator::extra::split_generator_name(name).is_some() {
        return true;
    }
    GENERATORS.lines().any(|line| {
        if !(line.starts_with("  ") || line.starts_with("* ")) || line.starts_with("   ") {
            return false;
        }
        let tail = &line[2..];
        let generator = tail.split_once('=').map_or(tail, |(head, _)| head);
        generator.trim_end() == name
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
    let rendered =
        crate::messages::render(crate::messages::MessageType::Warning, message, &[], None);
    crate::messages::print_diagnostic(crate::messages::MessageType::Warning, &rendered);
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
    let joined = if value.starts_with('/') {
        value
    } else {
        format!("{cwd}/{value}")
    };
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
    // cmake::SetDirectoriesFromFile: an existing path is resolved with
    // ToNormalizedPathOnDisk (a symlink before ".." is followed); a missing
    // one is collapsed lexically.
    let given = Path::new(arg);
    let full = if given.exists() {
        normalized_path(arg, cwd)
    } else {
        util::collapse_full_path(arg, cwd)
    };
    let path = Path::new(&full);
    let is_dir = path.is_dir();
    let exists = path.exists();
    let is_source = is_dir && Path::new(&options.project_list_file(&full)).is_file();
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

/// cmakemain's `do_open` and `cmake::Open`.  Only an extra generator can
/// open a project (cmExtraSublimeTextGenerator::Open runs
/// `CMAKE_SUBLIMETEXT_EXECUTABLE --project <bindir>/<name>.sublime-project`);
/// every other generator reports failure without a message.
fn open_project(args: &[String], cwd: &str) -> i32 {
    let mut dir = String::new();
    for (index, arg) in args.iter().enumerate() {
        if index == 0 {
            dir = normalized_path(arg, cwd);
        } else {
            eprintln!("Unknown argument {arg}");
            dir.clear();
        }
    }
    if dir.is_empty() {
        eprintln!("Usage: cmake --open <dir>");
        return 1;
    }
    let path = Path::new(&dir);
    if !path.is_dir() {
        eprintln!("Error: {dir} is not a directory");
        return 1;
    }
    // cmake::FindCacheFile.
    let cache_dir = if !path.join("CMakeCache.txt").exists() && path.join("CMakeFiles").exists() {
        path.ancestors()
            .find(|candidate| candidate.join("CMakeCache.txt").exists())
            .unwrap_or(path)
    } else {
        path
    };
    let mut cache = Cache::default();
    if !matches!(cache.load(cache_dir), Ok(true)) {
        eprintln!("Error: not a CMake build directory (missing CMakeCache.txt)");
        return 1;
    }
    let Some(generator) = cache.value("CMAKE_GENERATOR") else {
        eprintln!("Error: could not find CMAKE_GENERATOR in Cache");
        return 1;
    };
    let extra = cache.value("CMAKE_EXTRA_GENERATOR").unwrap_or("");
    let full_name = crate::generator::extra::create_full_generator_name(generator, extra);
    if !known_generator(&full_name) {
        eprintln!("Error: could not create CMAKE_GENERATOR \"{full_name}\"");
        return 1;
    }
    let Some(project) = cache.value("CMAKE_PROJECT_NAME") else {
        eprintln!("Error: could not find CMAKE_PROJECT_NAME in Cache");
        return 1;
    };
    if extra != "Sublime Text 2" {
        return 1;
    }
    let Some(sublime) = cache
        .value("CMAKE_SUBLIMETEXT_EXECUTABLE")
        .filter(|value| !crate::util::is_notfound(value))
    else {
        return 1;
    };
    let file = format!("{dir}/{project}.sublime-project");
    // cmSystemTools::RunSingleCommand (output passed through).
    match std::process::Command::new(sublime)
        .args(["--project", &file])
        .status()
    {
        Ok(status) if status.success() => 0,
        _ => 1,
    }
}

/// cmakemain's argument loop when `--system-information` is present: the
/// arguments left after the cmakemain-level options are consumed
/// (`parsedArgs`), or `None` without the flag.
fn system_information_args(argv: &[String]) -> Option<Vec<String>> {
    let mut parsed = Vec::new();
    let mut sysinfo = false;
    let mut script = false;
    let mut index = 0;
    while let Some(arg) = argv.get(index) {
        if arg == "--" && script {
            parsed = argv.to_vec();
            break;
        }
        match arg.as_str() {
            "--system-information" => sysinfo = true,
            "-N" | "-LAH" | "-LA" | "-LH" | "-L" => {}
            "-LRAH" | "-LRA" | "-LRH" | "-LR" => index += 1,
            "--find-package" => parsed.push(arg.clone()),
            _ if arg.starts_with("-P") => {
                script = true;
                parsed.push("-P".to_owned());
                if arg.len() > 2 {
                    parsed.push(arg[2..].to_owned());
                } else if let Some(value) = argv.get(index + 1) {
                    parsed.push(value.clone());
                    index += 1;
                }
            }
            _ if arg.starts_with("--list-presets") => {
                parsed.push("--list-presets".to_owned());
                parsed.push(arg.strip_prefix("--list-presets=").unwrap_or("").to_owned());
            }
            _ => parsed.push(arg.clone()),
        }
        index += 1;
    }
    sysinfo.then_some(parsed)
}

/// `cmake::GetSystemInformation`: configure `Modules/SystemInformation.cmake`
/// in `<cwd>/__cmake_systeminformation` and print or keep its result file.
fn system_information(args: &[String], cwd: &str) -> i32 {
    let dest = format!("{cwd}/__cmake_systeminformation");
    let _ = std::fs::remove_dir_all(&dest);
    if std::fs::create_dir_all(&dest).is_err() {
        eprintln!("Error: --system-information must be run from a writable directory!");
        return 1;
    }
    let mut result_file = String::new();
    let mut write_to_stdout = true;
    let mut generator = None;
    // cmSystemTools::Error sets the error flag, and the internal cmake::Run
    // then stops right after its argument processing.
    let mut error_occurred = false;
    let mut index = 1;
    while let Some(arg) = args.get(index) {
        if let Some(value) = arg.strip_prefix("-G") {
            let value = if value.is_empty() {
                index += 1;
                let Some(value) = args.get(index) else {
                    cmcmd::error("No generator specified for -G");
                    print_generators();
                    return -1;
                };
                value.as_str()
            } else {
                value
            };
            if known_generator(value) {
                generator = Some(value.to_owned());
            } else {
                cmcmd::error(format!("Could not create named generator {value}"));
                print_generators();
                error_occurred = true;
            }
        } else {
            if !Path::new(arg).is_absolute() {
                result_file = format!("{cwd}/");
            }
            result_file.push_str(arg);
            write_to_stdout = false;
        }
        index += 1;
    }
    let Some(root) = configure::find_cmake_root() else {
        eprintln!("CMake Error: Could not find CMAKE_ROOT !!!");
        return 1;
    };
    let in_file = format!("{root}/Modules/SystemInformation.cmake");
    let out_file = format!("{dest}/CMakeLists.txt");
    if std::fs::copy(&in_file, &out_file).is_err() {
        eprintln!("Error copying file \"{in_file}\" to \"{out_file}\".");
        return 1;
    }
    if result_file.is_empty() {
        result_file = format!("{cwd}/__cmake_systeminformation/results.txt");
    }
    // cmWorkingDirectory: the internal configure runs inside `dest`.
    let previous = std::env::current_dir().ok();
    if let Err(error) = std::env::set_current_dir(&dest) {
        eprintln!(
            "Failed to change working directory to {dest} : {}",
            cmcmd::os_error(&error)
        );
        return 1;
    }
    let mut internal = vec![
        args[0].clone(),
        dest.clone(),
        format!("-DRESULT_FILE={result_file}"),
    ];
    if let Some(generator) = generator {
        // The generator chosen here is the one the internal run uses.
        internal.extend(["-G".to_owned(), generator]);
    }
    let result = if error_occurred {
        -1
    } else {
        configure_args_with(&internal, &dest, true).unwrap_or(1)
    };
    if let Some(previous) = previous {
        let _ = std::env::set_current_dir(previous);
    }
    if result != 0 {
        eprintln!("Error: --system-information failed on internal CMake!");
        return result;
    }
    if write_to_stdout && let Ok(contents) = std::fs::read(&result_file) {
        use std::io::Write as _;
        let mut stdout = std::io::stdout();
        let _ = stdout.write_all(&contents);
        let _ = stdout.flush();
    }
    let _ = std::fs::remove_dir_all(&dest);
    0
}

fn project_or_script(argv: &[String], cwd: &str) -> i32 {
    if let Some(pos) = argv.iter().position(|arg| arg == "--check-build-system") {
        return check_build_system(argv, cwd, pos);
    }
    cmcmd::enable_message_callback();
    configure_args(argv, cwd).unwrap_or(1)
}

fn configure_args(argv: &[String], cwd: &str) -> Result<i32, ()> {
    configure_args_with(argv, cwd, false)
}

/// `cmake::Run` on a `cmake` instance; `no_progress` when it has no progress
/// callback (cmake::GetSystemInformation's internal run).
fn configure_args_with(argv: &[String], cwd: &str, no_progress: bool) -> Result<i32, ()> {
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
    if let Some(parsed) = system_information_args(argv) {
        return Ok(system_information(&parsed, cwd));
    }
    let script_mode = args
        .iter()
        .take_while(|arg| arg.as_str() != "--")
        .any(|arg| arg.starts_with("-P"));
    let mut options = ConfigureOptions {
        command_args: argv.to_vec(),
        no_progress,
        ..ConfigureOptions::default()
    };
    if !script_mode {
        load_environment_presets(&mut options);
    }
    let mut view = None::<CacheView>;
    let mut view_only = false;
    let mut regenerate = false;
    let mut cache_args = Vec::new();
    let mut unknown = None;
    let mut extra = None;
    let mut find_package_mode = false;
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
                "--sarif-output" => options.sarif_output = Some(normalized_path(&value, cwd)),
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
                // cmake::DebuggerPipe is stored as given (no normalization).
                "--debugger-pipe" => options.debugger_pipe = value,
                "--debugger-dap-log" => {
                    options.debugger_dap_log = normalized_path(&value, cwd);
                }
                "-B" => have_b_arg = true,
                _ => {}
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
                "--compile-no-warning-as-error" => {
                    println!(
                        "Ignoring COMPILE_WARNING_AS_ERROR target property and CMAKE_COMPILE_WARNING_AS_ERROR variable."
                    );
                    options.ignore_compile_warning_as_error = true;
                }
                "--link-no-warning-as-error" => {
                    println!(
                        "Ignoring LINK_WARNING_AS_ERROR target property and CMAKE_LINK_WARNING_AS_ERROR variable."
                    );
                    options.ignore_link_warning_as_error = true;
                }
                "-O" | "--warn-unused-vars" => {}
                "--debugger" => {
                    println!("Running with debugger on.");
                    options.debugger_on = true;
                }
                "--find-package" => find_package_mode = true,
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
    if find_package_mode {
        return Ok(run_find_package_mode(args, &cache_args, cwd));
    }
    // Empty instance, platform and toolset if only a generator is specified.
    if options.generator.is_some() {
        options.environment_instance.clear();
        options.environment_platform.clear();
        options.environment_toolset.clear();
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
    if let Some(path) = &options.sarif_output
        && std::fs::File::create(path).is_err()
    {
        cmcmd::error(format!("Invalid SARIF output file path: {path}"));
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
    process_cache_args(&mut options, &cache_args, cwd);
    if let Some(state) = &preset_state
        && matches!(
            options.log_level.as_deref(),
            Some("verbose" | "debug" | "trace")
        )
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
        configure::process_cache_only(&mut cache, &options)
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
        "--project-file" => {
            if options.project_file.is_some() {
                return Err("Multiple --project-file options not allowed".to_owned());
            }
            options.project_file = Some(value.to_owned());
        }
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
                options
                    .trace_sources
                    .push(cache::convert_to_unix_slashes(file));
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
pub(crate) fn cache_definition_from_parts(
    name: &str,
    value: &str,
    kind: Option<CacheType>,
) -> CacheDefinition {
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

fn process_cache_args(options: &mut ConfigureOptions, args: &[CacheArg<'_>], cwd: &str) {
    let define = |name: &str, value: &str, kind| {
        CacheArgument::Define(CacheDefinition {
            name: name.to_owned(),
            value: value.to_owned(),
            kind: Some(kind),
        })
    };
    for CacheArg { name, value } in args {
        let argument = match *name {
            "-D" => match cache_definition(value) {
                Ok(definition) => CacheArgument::Define(definition),
                Err(message) => CacheArgument::Error(message),
            },
            "-C" if value.is_empty() => {
                CacheArgument::Error("No file name specified for -C".to_owned())
            }
            "-C" => CacheArgument::Preload {
                value: value.clone(),
                path: util::collapse_full_path(value, cwd),
            },
            "-U" => CacheArgument::Undefine(value.clone()),
            "-W" => match warning_option(options, value) {
                Ok(()) => continue,
                Err(message) => CacheArgument::Error(message),
            },
            "--toolchain" => define("CMAKE_TOOLCHAIN_FILE", value, CacheType::Filepath),
            "--install-prefix" if Path::new(value).is_absolute() => {
                define("CMAKE_INSTALL_PREFIX", value, CacheType::Path)
            }
            "--install-prefix" => {
                CacheArgument::Error("Absolute paths are required for --install-prefix".to_owned())
            }
            _ => continue,
        };
        // Arguments after one that does not parse are not processed.
        let failed = matches!(argument, CacheArgument::Error(_));
        options.cache_args.push(argument);
        if failed {
            break;
        }
    }
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
    make_interp(Mode::Script, argv, cwd)
}

/// The interpreter of `-P` scripts and of `--find-package` mode.
fn make_interp(mode: Mode, argv: &[String], cwd: &str) -> Option<Interp> {
    let Some(root) = configure::find_cmake_root() else {
        cmcmd::error(format!(
            "Could not find CMAKE_ROOT !!!\nCMake has most likely not been installed correctly.\nModules directory not found in\n{}",
            std::env::current_exe().unwrap_or_default().display()
        ));
        return None;
    };
    let mut interp = Interp::new(mode, cwd, cwd, &root);
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

/// `cmake::FindPackage`: run CMakeFindPackageMode.cmake in a find-package
/// mode interpreter and report the result for MODE=EXIST, COMPILE or LINK.
fn run_find_package_mode(argv: &[String], args: &[CacheArg<'_>], cwd: &str) -> i32 {
    let Some(root) = configure::find_cmake_root() else {
        cmcmd::error("Could not find CMAKE_ROOT !!!");
        return 1;
    };
    let Some(mut interp) = make_interp(Mode::FindPackage, argv, cwd) else {
        return 1;
    };
    for CacheArg { name, value } in args {
        if *name == "-D" {
            match cache_definition(value) {
                Ok(definition) => add_definition(&mut interp.cache, &definition),
                Err(error) => {
                    cmcmd::error(error);
                    return cli_failure();
                }
            }
        }
    }
    // cmMakefile::ReadListFile: errors are reported, and the result is
    // still computed from the variables the module set.
    let _ = read_script(
        &mut interp,
        &format!("{root}/Modules/CMakeFindPackageMode.cmake"),
        cwd,
        false,
    );
    let var = |name: &str| interp.get_def(name).map(str::to_owned).unwrap_or_default();
    let language = var("LANGUAGE");
    let mode = var("MODE");
    let name = var("NAME");
    let found = util::is_on(&var("PACKAGE_FOUND"));
    let quiet = util::is_on(&var("PACKAGE_QUIET"));
    if !found {
        if !quiet {
            println!("{name} not found.");
        }
        // cmake::SetArgs: the failed --find-package argument.
        cmcmd::error("Run 'cmake --help' for all supported options.");
        return 1;
    }
    match mode.as_str() {
        "EXIST" if !quiet => println!("{name} found."),
        "COMPILE" => {
            let includes = util::expand_list(&var("PACKAGE_INCLUDE_DIRS"), false);
            let include_flags = find_package_include_flags(&interp, &includes, &language);
            println!("{include_flags} {}", var("PACKAGE_DEFINITIONS"));
        }
        "LINK" => {
            let line = find_package_link_line(&mut interp, &language);
            println!("{line}");
        }
        _ => {}
    }
    // cmake::Run: any reported error fails the run.
    i32::from(interp.error || interp.fatal)
}

/// `cmake::FindPackage` MODE=LINK: the link line of a dummy executable
/// linking `PACKAGE_LIBRARIES`, from the base link line computer.
fn find_package_link_line(interp: &mut Interp, language: &str) -> String {
    let libraries = util::expand_list(
        interp.get_def("PACKAGE_LIBRARIES").unwrap_or_default(),
        false,
    );
    let build_type = interp
        .get_def("CMAKE_BUILD_TYPE")
        .unwrap_or_default()
        .to_ascii_uppercase();
    let mut target = crate::state::Target::new("dummy", crate::state::TargetKind::Executable, 0);
    target.props.set("LINKER_LANGUAGE", language.to_owned());
    // cmTarget::AddLinkLibrary with GENERAL_LibraryType.
    for library in &libraries {
        target.insert_entry("LINK_LIBRARIES", library, &[], false);
    }
    let key = crate::commands::target::register_target(interp, target);
    // The generation objects see the directory's final variables.
    interp.record_directory_vars();
    let ctx = crate::generator::ctx::GenCtx::new(interp);
    let target = &ctx.interp.ext.targets[&key];
    let line = crate::generator::link::base_link_line(&ctx, target, &build_type, language);
    // cmLocalGenerator::AppendFlags joins the non-empty fragments with
    // single spaces, keeping each fragment's own spacing.
    let append_flags = |fragments: Vec<&str>| {
        let mut flags = String::new();
        for fragment in fragments
            .into_iter()
            .filter(|fragment| !fragment.is_empty())
        {
            if !flags.is_empty() {
                flags.push(' ');
            }
            flags.push_str(fragment);
        }
        flags
    };
    let link_path = append_flags(
        line.path_fragments
            .iter()
            .map(|(text, _)| text.as_str())
            .collect(),
    );
    let link_libs = append_flags(
        line.lib_fragments
            .iter()
            .map(|(text, _)| text.as_str())
            .collect(),
    );
    format!("{}{link_path}{link_libs}", line.framework_path)
}

/// `cmLocalGenerator::GetIncludeFlags` without a target, as
/// `cmake::FindPackage` calls it, with the base generator's shell.
fn find_package_include_flags(interp: &Interp, includes: &[String], lang: &str) -> String {
    if lang.is_empty() {
        return String::new();
    }
    let shell = |path: &str| {
        crate::outconv::escape_for_shell(
            path,
            crate::outconv::FLAG_MAKE | crate::outconv::FLAG_IS_UNIX,
        )
    };
    let var = |name: &str| interp.get_def(name).map(str::to_owned);
    let include_flag = var(&format!("CMAKE_INCLUDE_FLAG_{lang}")).unwrap_or_default();
    let quote_paths = var("CMAKE_QUOTE_INCLUDE_PATHS").is_some();
    // A separator means the flag is given once, as in `-classpath a:b:c`.
    let (sep, repeat_flag) = match var(&format!("CMAKE_INCLUDE_FLAG_SEP_{lang}")) {
        Some(sep) => (sep, false),
        None => (" ".to_owned(), true),
    };
    let fw_search_flag = var(&format!("CMAKE_{lang}_FRAMEWORK_SEARCH_FLAG"));
    let apple = interp.get_def("APPLE").is_some_and(util::is_on);
    let mut flags = String::new();
    let mut flag_used = false;
    let mut emitted = std::collections::HashSet::new();
    for include in includes {
        if let Some(fw_flag) = fw_search_flag.as_deref().filter(|flag| !flag.is_empty())
            && apple
            && crate::apple::is_path_to_framework(include)
        {
            let framework_dir = util::filename_path(include);
            if emitted.insert(framework_dir.clone()) {
                flags.push_str(fw_flag);
                flags.push_str(&shell(&framework_dir));
                flags.push(' ');
            }
            continue;
        }
        if !flag_used || repeat_flag {
            flags.push_str(&include_flag);
            flag_used = true;
        }
        let path = shell(include);
        let quote = quote_paths && !path.is_empty() && !path.starts_with('"');
        if quote {
            flags.push('"');
        }
        flags.push_str(&path);
        if quote {
            flags.push('"');
        }
        flags.push_str(&sep);
    }
    // Remove trailing separators.
    if !sep.starts_with(' ') && flags.ends_with(&sep[..1]) {
        flags.pop();
        flags.push(' ');
    }
    flags.trim().to_owned()
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
    // `cmake::SetCacheArgs` seeds CMAKE_POLICY_VERSION_MINIMUM from the
    // environment before processing -D/-U/-C arguments.
    if interp.cache.value("CMAKE_POLICY_VERSION_MINIMUM").is_none()
        && let Ok(value) = std::env::var("CMAKE_POLICY_VERSION_MINIMUM")
        && !value.is_empty()
    {
        interp.cache.add(
            "CMAKE_POLICY_VERSION_MINIMUM",
            Some(&value),
            Some("Override policy version for cmake_minimum_required calls."),
            crate::cache::CacheType::String,
        );
        if let Some(entry) = interp.cache.entries.get_mut("CMAKE_POLICY_VERSION_MINIMUM") {
            entry.properties.insert("ADVANCED".into(), "1".into());
        }
    }
    let mut result = 0;
    for CacheArg { name, value } in args {
        match *name {
            "-D" => match cache_definition(value) {
                Ok(definition) => add_definition(&mut interp.cache, &definition),
                Err(error) => {
                    cmcmd::error(error);
                    result = cli_failure();
                    break;
                }
            },
            "-U" => configure::unset_cache_entries(&mut interp.cache, value),
            "-C" => {
                println!("loading initial cache file {value}");
                result = read_script(
                    &mut interp,
                    &util::collapse_full_path(value, cwd),
                    cwd,
                    false,
                );
            }
            "-P" => {
                // The -P ScriptLambda starts the debugger before the script
                // is read (the project path starts it in cmake::Run).
                if !configure::start_debugger(&mut interp, options) {
                    result = cli_failure();
                    break;
                }
                result = read_script(&mut interp, value, cwd, true);
            }
            _ => {}
        }
        if result != 0 || interp.exit_code.is_some() {
            break;
        }
    }
    // `cmake::StopDebuggerIfNeeded` reports the exit status and ends the
    // session (the handle drop joins the adapter thread).
    if let Some(session) = interp.debugger.take() {
        session.report_exit_code(i64::from(result));
    }
    if let Some(view) = view {
        view.print(&interp.cache);
    }
    if let Some(path) = &options.sarif_output {
        interp.sarif_log.write(path, false);
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

fn check_build_system_list(path: &Path, name: &str) -> Option<Vec<String>> {
    let text = std::fs::read_to_string(path).ok()?;
    let needle = format!("set({name}");
    let start = text.find(&needle)? + needle.len();
    let rest = &text[start..];
    let end = rest.find("\n  )")?;
    let mut values = Vec::new();
    for line in rest[..end].lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if let Some(inner) = line.strip_prefix('"').and_then(|v| v.strip_suffix('"')) {
            values.push(inner.replace("\\\"", "\"").replace("\\\\", "\\"));
        }
    }
    Some(values)
}

fn check_build_system_path(binary: &str, value: &str) -> std::path::PathBuf {
    let path = Path::new(value);
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        Path::new(binary).join(path)
    }
}

fn check_build_system_modified(path: &Path) -> Option<std::time::SystemTime> {
    std::fs::metadata(path).and_then(|m| m.modified()).ok()
}

fn clear_make_dependencies(binary: &str, listfile: &Path) {
    let Some(info_files) = check_build_system_list(listfile, "CMAKE_DEPEND_INFO_FILES") else {
        return;
    };
    let empty = "# Empty dependencies file\n# This may be replaced when dependencies are built.\n";
    for file in info_files {
        let file = check_build_system_path(binary, &file);
        let Some(dir) = file.parent() else { continue };
        // cmLocalUnixMakefileGenerator3::ClearDependencies: empty the
        // scanner-written make fragments, drop their internal validation
        // files, and force compiler dependency collection to run again.
        let languages =
            check_build_system_list(&file, "CMAKE_DEPENDS_LANGUAGES").unwrap_or_default();
        if !languages.is_empty() {
            let _ = crate::fsutil::write_generated(&dir.join("depend.make"), empty);
            let _ = std::fs::remove_file(dir.join("depend.internal"));
        }
        let dependency_files =
            check_build_system_list(&file, "CMAKE_DEPENDS_DEPENDENCY_FILES").unwrap_or_default();
        if !dependency_files.is_empty() {
            let _ = crate::fsutil::write_generated(&dir.join("compiler_depend.make"), empty);
            let _ = std::fs::remove_file(dir.join("compiler_depend.internal"));
            let timestamp = dir.join("compiler_depend.ts");
            if timestamp.exists() {
                let _ = std::fs::OpenOptions::new().append(true).open(&timestamp);
            } else {
                let _ = std::fs::write(&timestamp, "");
            }
            for depfile in dependency_files.iter().skip(3).step_by(4) {
                let _ = std::fs::remove_file(check_build_system_path(binary, depfile));
            }
        }
    }
}

fn check_build_system(argv: &[String], cwd: &str, pos: usize) -> i32 {
    if argv.get(pos + 1).is_none() || argv.get(pos + 2).is_none() {
        cmcmd::error("Invalid value used with --check-build-system");
        cmcmd::error("Run 'cmake --help' for all supported options.");
        return 1;
    }
    let listfile = &argv[pos + 1];
    let clear = argv[pos + 2] != "0";
    let path = Path::new(cwd).join(listfile);
    let check_file_exists = path.exists();
    if clear && check_file_exists {
        clear_make_dependencies(cwd, &path);
    }

    // cmake::CheckBuildSystem: missing generation products, missing inputs
    // or outputs, or any dependency newer than the oldest output requires a
    // configure rerun.  Entries in Makefile.cmake may be relative to the
    // top-level binary directory.
    let products = check_build_system_list(&path, "CMAKE_MAKEFILE_PRODUCTS").unwrap_or_default();
    let depends = check_build_system_list(&path, "CMAKE_MAKEFILE_DEPENDS").unwrap_or_default();
    let outputs = check_build_system_list(&path, "CMAKE_MAKEFILE_OUTPUTS").unwrap_or_default();
    let mut rerun = !check_file_exists
        || products
            .iter()
            .any(|product| !check_build_system_path(cwd, product).exists());
    let mut newest_dep: Option<(std::time::SystemTime, String)> = None;
    for dep in &depends {
        let dep_path = check_build_system_path(cwd, dep);
        let Some(time) = check_build_system_modified(&dep_path) else {
            rerun = true;
            continue;
        };
        if newest_dep
            .as_ref()
            .is_none_or(|(current, _)| time > *current)
        {
            newest_dep = Some((time, dep.clone()));
        }
    }
    let mut oldest_out: Option<(std::time::SystemTime, String)> = None;
    for output in &outputs {
        let out_path = check_build_system_path(cwd, output);
        let Some(time) = check_build_system_modified(&out_path) else {
            rerun = true;
            continue;
        };
        if oldest_out
            .as_ref()
            .is_none_or(|(current, _)| time < *current)
        {
            oldest_out = Some((time, output.clone()));
        }
    }
    if depends.is_empty() || outputs.is_empty() {
        rerun = true;
    }
    if let (Some((dep_time, _)), Some((out_time, _))) = (newest_dep, oldest_out) {
        rerun |= out_time < dep_time;
    }
    if !rerun {
        return 0;
    }

    // Re-enter the normal configure path with the check arguments removed.
    let mut configure: Vec<String> = Vec::new();
    for (index, arg) in argv.iter().enumerate() {
        if index == pos || index == pos + 1 || index == pos + 2 {
            continue;
        }
        configure.push(arg.clone());
    }
    configure_args(&configure, cwd).unwrap_or(1)
}

fn build(argv: &[String], cwd: &str) -> i32 {
    let Ok(mut options) = parse_build(&argv[2..], cwd) else {
        eprint!("{}", include_str!("cmcmd/build-usage.txt"));
        return 1;
    };
    cmcmd::enable_message_callback();
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
    if !matches!(generator, "Ninja" | "Ninja Multi-Config" | "Unix Makefiles") {
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
            let rendered = crate::messages::render(
                crate::messages::MessageType::FatalError,
                &message,
                &[],
                None,
            );
            crate::messages::print_diagnostic(crate::messages::MessageType::FatalError, &rendered);
            return 1;
        }
    }
    if cache.value("CMAKE_PROJECT_NAME").is_none() {
        eprintln!("Error: could not find CMAKE_PROJECT_NAME in Cache");
        return 1;
    }
    let mut instrumentation = crate::instrumentation::Instrumentation::new(cache_dir);
    instrumentation.load();
    if instrumentation.errors {
        return 1;
    }
    instrumentation.collect("preCMakeBuild");
    let clock = instrumentation.timer();
    let result = run_build(options, &cache, generator);
    instrumentation.write_snippet("cmakeBuild", argv, clock, result);
    instrumentation.collect("postCMakeBuild");
    result
}

fn run_build(options: BuildOptions, cache: &Cache, generator: &str) -> i32 {
    let dir = Path::new(&options.dir);
    let verbose = options.verbose || util::is_on_opt(cache.value("CMAKE_VERBOSE_MAKEFILE"));
    let make_program = cache
        .value("CMAKE_MAKE_PROGRAM")
        .filter(|program| !util::is_off(program))
        .unwrap_or("");
    let mut command = Vec::new();
    if verbose {
        println!("Change Dir: '{}'", options.dir);
    }
    if generator == "Unix Makefiles" {
        if verbose {
            let cmake = std::env::current_exe()
                .map(|path| path.to_string_lossy().into_owned())
                .unwrap_or_else(|_| "cmake".to_owned());
            command.extend([
                cmake,
                "-E".to_owned(),
                "env".to_owned(),
                "VERBOSE=1".to_owned(),
            ]);
        }
        command.extend([
            make_program.to_owned(),
            "-f".to_owned(),
            "Makefile".to_owned(),
        ]);
        if let Some(jobs) = options.jobs {
            if jobs == 0 {
                command.push("-j".to_owned());
            } else {
                command.push(format!("-j{jobs}"));
            }
        }
    } else {
        command.push(make_program.to_owned());
        if verbose {
            command.push("-v".to_owned());
        }
        if let Some(jobs) = options.jobs.filter(|jobs| *jobs > 0) {
            command.extend(["-j".to_owned(), jobs.to_string()]);
        }
        if generator == "Ninja Multi-Config" && !options.config.is_empty() {
            command.extend(["-f".to_owned(), format!("build-{}.ninja", options.config)]);
        }
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

fn install(invocation: &[String], cwd: &str) -> i32 {
    let Some((dir, args)) = invocation[2..].split_first() else {
        eprint!("{}", include_str!("cmcmd/install-usage.txt"));
        return 1;
    };
    let dir = util::collapse_full_path(dir, cwd);
    let mut variables = std::collections::BTreeMap::new();
    let mut strip = false;
    let mut verbose = false;
    let mut jobs: u32 = 0;
    let mut index = 0;
    while index < args.len() {
        let arg = &args[index];
        match arg.as_str() {
            "--strip" => strip = true,
            "-v" | "--verbose" => verbose = true,
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
                    match parse_jobs(&value, name) {
                        Ok(value) => jobs = value,
                        Err(()) => {
                            eprint!("{}", include_str!("cmcmd/install-usage.txt"));
                            return 1;
                        }
                    }
                } else {
                    variables.insert(name, value);
                }
            }
        }
        index += 1;
    }
    let mut argv = vec![invocation[0].clone()];
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
    argv.push("-P".to_owned());
    let component = variables.get("--component").cloned().unwrap_or_default();
    let config = variables.get("--config").cloned().unwrap_or_default();
    let mut instrumentation = crate::instrumentation::Instrumentation::new(&dir);
    instrumentation.load();
    let mut handler =
        crate::install_script::InstallScriptHandler::new(&dir, &component, &config, &mut argv);
    if jobs == 0 && handler.is_parallel() {
        jobs = 1;
        if let Ok(value) = std::env::var("CMAKE_INSTALL_PARALLEL_LEVEL") {
            match parse_jobs(&value, "") {
                Ok(level) if level >= 1 => jobs = level,
                _ => {
                    eprintln!(
                        "Value of CMAKE_INSTALL_PARALLEL_LEVEL environment variable must be a positive integer."
                    );
                    return 1;
                }
            }
        }
    }
    cmcmd::enable_message_callback();
    // cmInstrumentation::InstrumentCommand("cmakeInstall", ...) around
    // the install, then the postInstall hook.
    let clock = instrumentation.timer();
    let result = (|| {
        if handler.is_parallel() {
            // cmInstallScriptHandler::Install: each script runs under
            // `ctest --instrument` when instrumentation is queried.
            if instrumentation.enabled {
                handler.prefix_commands(&[
                    ctest_command(),
                    "--instrument".to_owned(),
                    "--command-type".to_owned(),
                    "install".to_owned(),
                    "--build-dir".to_owned(),
                    dir.clone(),
                    "--".to_owned(),
                ]);
            }
            return i32::from(handler.install(jobs, cwd) > 0);
        }
        let mut result = 0;
        for command in handler.commands() {
            let Some(script) = command.last() else {
                continue;
            };
            let Some(mut interp) = make_script_interp(command, cwd) else {
                return 1;
            };
            // cmake --install -v: SetDebugOutputOn(verbose).
            interp.debug.output = verbose;
            for arg in command[1..command.len() - 2]
                .iter()
                .filter_map(|arg| arg.strip_prefix("-D"))
            {
                if let Ok(definition) = cache_definition(arg) {
                    add_definition(&mut interp.cache, &definition);
                }
            }
            result = i32::from(read_script(&mut interp, script, cwd, true) != 0);
        }
        result
    })();
    instrumentation.write_snippet("cmakeInstall", invocation, clock, result);
    instrumentation.collect("postInstall");
    result
}

/// `cmSystemTools::GetCTestCommand`: the ctest next to this cmake.
fn ctest_command() -> String {
    let command = std::env::current_exe()
        .ok()
        .and_then(|path| path.canonicalize().ok())
        .map(|path| path.to_string_lossy().into_owned())
        .unwrap_or_else(|| "cmake".to_owned());
    format!("{}/ctest", util::filename_path(&command))
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

#[cfg(test)]
mod tests {
    use super::known_generator;

    #[test]
    fn accepts_extra_unix_makefiles_generators() {
        for generator in [
            "CodeBlocks - Unix Makefiles",
            "CodeLite - Unix Makefiles",
            "Eclipse CDT4 - Unix Makefiles",
            "Kate - Unix Makefiles",
            "Sublime Text 2 - Unix Makefiles",
        ] {
            assert!(known_generator(generator), "{generator} should be accepted");
        }
        assert!(!known_generator(
            "   Generates Sublime Text 2 project files."
        ));
        assert!(!known_generator("Unknown - Unix Makefiles"));
    }
}
