//! cmake-rust: an independent implementation of CMake in Rust.

mod cache;
mod cmregex;
mod commands;
mod configure;
mod configure_log;
mod condition;
mod flow;
mod generator;
mod languages;
mod fsutil;
mod interp;
mod lexer;
mod messages;
mod outconv;
mod policy;
mod policy_table;
mod props;
mod state;
mod util;

use std::process::ExitCode;

use cache::CacheType;
use interp::{Interp, Mode};

/// Locates the CMake data root (Modules/, Templates/).
fn find_cmake_root() -> Option<String> {
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
        interp::VERSION.0,
        interp::VERSION.1
    ));
    if candidate.join("Modules").is_dir() {
        return Some(candidate.to_string_lossy().into_owned());
    }
    None
}

fn sibling_tool(interp: &Interp, name: &str) -> String {
    let directory = util::filename_path(&interp.cmake_command);
    format!("{directory}/{name}")
}

/// Variables every interpreter instance starts with (cmake::AddCMakePaths,
/// cmMakefile defaults, cmStateSnapshot::SetDefaultDefinitions).
fn add_default_definitions(interp: &mut Interp, argv: &[String]) {
    let (major, minor, patch) = interp::VERSION;
    interp.set_def("CMAKE_MAJOR_VERSION", major.to_string());
    interp.set_def("CMAKE_MINOR_VERSION", minor.to_string());
    interp.set_def("CMAKE_PATCH_VERSION", patch.to_string());
    interp.set_def("CMAKE_TWEAK_VERSION", "0");
    interp.set_def("CMAKE_VERSION", interp::VERSION_STRING);
    interp.set_def("CMAKE_FILES_DIRECTORY", "/CMakeFiles");
    interp.set_def("CMAKE_HOST_EXECUTABLE_SUFFIX", "");
    if cfg!(target_os = "macos") {
        interp.set_def("CMAKE_HOST_APPLE", "1");
        interp.set_def("CMAKE_HOST_SYSTEM_NAME", "Darwin");
        interp.set_def("APPLE", "1");
    } else {
        interp.set_def("CMAKE_HOST_LINUX", "1");
        interp.set_def("CMAKE_HOST_SYSTEM_NAME", "Linux");
        interp.set_def("LINUX", "1");
    }
    interp.set_def("CMAKE_HOST_UNIX", "1");
    interp.set_def("UNIX", "1");
    let command = interp.cmake_command.clone();
    let cpack = sibling_tool(interp, "cpack");
    let ctest = sibling_tool(interp, "ctest");
    let root = interp.cmake_root.clone();
    interp.cache.add(
        "CMAKE_COMMAND",
        Some(&command),
        Some("Path to CMake executable."),
        CacheType::Internal,
    );
    interp.cache.add(
        "CMAKE_CPACK_COMMAND",
        Some(&cpack),
        Some("Path to cpack program executable."),
        CacheType::Internal,
    );
    interp.cache.add(
        "CMAKE_CTEST_COMMAND",
        Some(&ctest),
        Some("Path to ctest program executable."),
        CacheType::Internal,
    );
    interp.cache.add(
        "CMAKE_ROOT",
        Some(&root),
        Some("Path to CMake installation."),
        CacheType::Internal,
    );
    let _ = argv;
}

fn usage() -> String {
    "Usage\n\n  cmake [options] <path-to-source>\n  cmake [options] <path-to-existing-build>\n  cmake [options] -S <path-to-source> -B <path-to-build>\n\nSpecify a source directory to (re-)generate a build system for it in the\ncurrent working directory.  Specify an existing build directory to\nre-generate its build system.\n\nRun 'cmake --help' for more information.\n".to_owned()
}

fn run() -> i32 {
    let argv: Vec<String> = std::env::args().collect();
    if argv.len() >= 2 && (argv[1] == "--version" || argv[1] == "-version" || argv[1] == "/V") {
        println!(
            "cmake version {}\n\nCMake suite maintained and supported by Kitware (kitware.com/cmake).",
            interp::VERSION_STRING
        );
        return 0;
    }
    if argv.len() == 1 {
        print!("{}", usage());
        return 1;
    }
    let Some(root) = find_cmake_root() else {
        eprintln!(
            "CMake Error: Could not find CMAKE_ROOT !!!\nCMake has most likely not been installed correctly.\nModules directory not found in\n{}",
            std::env::current_exe()
                .map(|path| path.display().to_string())
                .unwrap_or_default()
        );
        return 1;
    };
    // Script mode: process -D and -P in order.
    let cwd = std::env::current_dir()
        .map(|dir| dir.to_string_lossy().into_owned())
        .unwrap_or_default();
    let cwd = std::env::var("PWD")
        .ok()
        .filter(|pwd| {
            std::fs::canonicalize(pwd).ok() == std::fs::canonicalize(&cwd).ok()
        })
        .unwrap_or(cwd);
    let script_index = argv.iter().position(|arg| arg == "-P");
    if let Some(index) = script_index {
        let Some(script) = argv.get(index + 1) else {
            eprintln!("CMake Error: No script specified for argument -P");
            return 1;
        };
        let mut interp = Interp::new(Mode::Script, &cwd, &cwd, &root);
        add_default_definitions(&mut interp, &argv);
        interp.set_def("CMAKE_ARGC", argv.len().to_string());
        for (position, arg) in argv.iter().enumerate() {
            let value = if position == 0 {
                interp.cmake_command.clone()
            } else {
                arg.clone()
            };
            interp.set_def(format!("CMAKE_ARGV{position}"), value);
        }
        interp.set_def("CMAKE_SOURCE_DIR", cwd.clone());
        interp.set_def("CMAKE_BINARY_DIR", cwd.clone());
        interp.set_def("CMAKE_CURRENT_SOURCE_DIR", cwd.clone());
        interp.set_def("CMAKE_CURRENT_BINARY_DIR", cwd.clone());
        // -D definitions before -P become cache entries.
        let mut position = 1;
        while position < index {
            let arg = &argv[position];
            if let Some(definition) = arg.strip_prefix("-D") {
                let definition = if definition.is_empty() {
                    position += 1;
                    argv.get(position).cloned().unwrap_or_default()
                } else {
                    definition.to_owned()
                };
                if let Some((key, value, kind)) = cache::parse_entry(&definition) {
                    interp.cache.add(
                        &key,
                        Some(&value),
                        Some("No help, variable specified on the command line."),
                        kind,
                    );
                }
            }
            position += 1;
        }
        let path = util::collapse_full_path(script, &cwd);
        interp.set_def("CMAKE_SCRIPT_MODE_FILE", path.clone());
        if !std::path::Path::new(&path).is_file() {
            eprintln!("CMake Error: Not a file: {path}");
            eprintln!("CMake Error: Error processing file: {path}");
            return 1;
        }
        interp.read_list_file(&path);
        if let Some(code) = interp.exit_code {
            return code;
        }
        if interp.error || interp.fatal {
            return 1;
        }
        return 0;
    }
    // Minimal configure-mode argument handling (the full CLI port replaces this).
    let mut options = configure::ConfigureOptions::default();
    let mut position = 1;
    while position < argv.len() {
        let arg = &argv[position];
        let mut take = |flag: &str| -> Option<String> {
            if arg == flag {
                position += 1;
                argv.get(position).cloned()
            } else {
                arg.strip_prefix(flag).map(str::to_owned)
            }
        };
        if let Some(value) = take("-S") {
            options.source_dir = util::collapse_full_path(&value, &cwd);
        } else if let Some(value) = take("-B") {
            options.binary_dir = util::collapse_full_path(&value, &cwd);
        } else if let Some(value) = take("-G") {
            options.generator = Some(value);
        } else if let Some(value) = take("-D") {
            if let Some((name, value, kind)) = cache::parse_entry(&value) {
                options.definitions.push(configure::CacheDefinition {
                    name,
                    value,
                    kind: (kind != CacheType::Uninitialized).then_some(kind),
                });
            }
        } else if arg == "--fresh" {
            options.fresh = true;
        } else if arg == "--debug-trycompile" {
            options.debug_trycompile = true;
        }
        position += 1;
    }
    if options.binary_dir.is_empty() {
        options.binary_dir = cwd.clone();
    }
    configure::configure(&options)
}

fn main() -> ExitCode {
    let code = run();
    ExitCode::from(u8::try_from(code & 0xff).unwrap_or(1))
}
