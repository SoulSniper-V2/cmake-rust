//! Portable commands exposed by `cmake -E` (cmcmd.cxx).

mod archive;
pub(crate) mod autogen;
mod cocompile;
mod depends;
mod depfile;
mod dyndep;
mod fortran;
mod msvc;

use std::collections::BTreeMap;
use std::ffi::CString;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, Read};
use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};
use std::path::Path;
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use md5::Md5;
use sha1::Sha1;
use sha2::{Sha224, Sha256, Sha384, Sha512};

use crate::cli::{Values, matches_value, read_value};
use crate::{interp, util};

static MESSAGE_CALLBACK_ACTIVE: AtomicBool = AtomicBool::new(false);

/// Mark that cmakemain has installed its cmSystemTools message callback for
/// this command's cmake instance. `-E` never does so and keeps raw stderr.
pub(crate) fn enable_message_callback() {
    MESSAGE_CALLBACK_ACTIVE.store(true, Ordering::Relaxed);
}

pub(crate) fn error(message: impl std::fmt::Display) {
    let text = format!("CMake Error: {message}");
    if MESSAGE_CALLBACK_ACTIVE.load(Ordering::Relaxed) {
        crate::stdio::print_err_then_plain(crate::stdio::TermAttr::Normal.bit(), &text, "\n");
    } else {
        eprintln!("{text}");
    }
}

pub(crate) fn process_error(err: &io::Error) -> String {
    os_error(err).to_ascii_lowercase()
}

pub(crate) fn os_error(err: &io::Error) -> String {
    let text = err.to_string();
    text.split(" (os error ").next().unwrap_or(&text).to_owned()
}

fn usage(program: &str) {
    eprint!(
        "CMake Error: cmake version {}\nUsage: {program} -E <command> [arguments...]\n{}",
        interp::VERSION_STRING,
        include_str!("cmcmd/e-usage.txt")
    );
}

pub(crate) fn dispatch(program: &str, args: &[String]) -> i32 {
    let Some((command, args)) = args.split_first() else {
        usage(program);
        return 1;
    };
    match command.as_str() {
        "capabilities" => capabilities(args),
        "echo" => {
            crate::bytes::stdout_line(&args.join(" "));
            0
        }
        "echo_append" => {
            crate::bytes::write_stdout(&args.join(" "));
            0
        }
        "cmake_echo_color" => echo_color(args),
        "true" => 0,
        "false" => 1,
        "server" => {
            error("CMake server mode has been removed in favor of the file-api.");
            1
        }
        "environment" => {
            for (name, val) in std::env::vars_os() {
                crate::bytes::stdout_line(&format!(
                    "{}={}",
                    crate::bytes::decode_os(&name),
                    crate::bytes::decode_os(&val)
                ));
            }
            0
        }
        "env" => run_env(args),
        "cat" if !args.is_empty() => cat(args),
        "compare_files" if args.len() == 2 => i32::from(!same_contents(&args[0], &args[1], false)),
        "compare_files" if args.len() == 3 && args[0] == "--ignore-eol" => {
            i32::from(!same_contents(&args[1], &args[2], true))
        }
        "compare_files" if args.len() == 3 => {
            usage(program);
            2
        }
        "copy" | "copy_if_different" | "copy_directory" | "copy_directory_if_different"
            if args.len() >= 2 =>
        {
            copy(command, args)
        }
        "rename" if args.len() == 2 => match fs::rename(&args[0], &args[1]) {
            Ok(()) => 0,
            Err(e) => {
                eprintln!(
                    "Error renaming from \"{}\" to \"{}\": {}",
                    args[0],
                    args[1],
                    os_error(&e)
                );
                1
            }
        },
        "make_directory" if !args.is_empty() => make_directories(args),
        "remove" if !args.is_empty() => remove(args),
        "remove_directory" if !args.is_empty() => {
            let mut result = true;
            for directory in args.iter().filter(|dir| Path::new(dir).is_dir()) {
                result &= remove_directory(directory, true);
            }
            i32::from(!result)
        }
        "rm" if !args.is_empty() => rm(args),
        "touch" | "touch_nocreate" if !args.is_empty() => touch(command, args),
        "sleep" if !args.is_empty() => sleep(args),
        "chdir" if args.len() >= 2 => {
            if !Path::new(&args[0]).exists() {
                error(format!(
                    "Directory does not exist for chdir command: {}",
                    args[0]
                ));
                return 1;
            }
            run_command(
                Command::new(&args[1])
                    .args(&args[2..])
                    .current_dir(&args[0]),
                1,
            )
        }
        "time" if !args.is_empty() => {
            let start = Instant::now();
            let code = run_command(Command::new(&args[0]).args(&args[1..]), 0);
            println!("Elapsed time (seconds): {}", start.elapsed().as_secs_f64());
            code
        }
        "md5sum" if !args.is_empty() => hash_sum::<Md5>(args),
        "sha1sum" if !args.is_empty() => hash_sum::<Sha1>(args),
        "sha224sum" if !args.is_empty() => hash_sum::<Sha224>(args),
        "sha256sum" if !args.is_empty() => hash_sum::<Sha256>(args),
        "sha384sum" if !args.is_empty() => hash_sum::<Sha384>(args),
        "sha512sum" if !args.is_empty() => hash_sum::<Sha512>(args),
        "create_symlink" | "create_hardlink" if args.len() == 2 => {
            create_link(command == "create_hardlink", &args[0], &args[1])
        }
        "cmake_symlink_library" if args.len() == 3 => {
            let names: Vec<_> = args
                .iter()
                .map(|name| crate::cache::convert_to_unix_slashes(name))
                .collect();
            let first = symlink_internal(command, &names[0], &names[1]);
            first | symlink_internal(command, &names[1], &names[2])
        }
        "cmake_symlink_executable" if args.len() == 2 => {
            symlink_internal(command, &args[0], &args[1])
        }
        "cmake_transform_depfile" if args.len() == 8 => depfile::transform(args),
        "__create_def" => msvc::create_def(args),
        "vs_link_exe" => msvc::visual_studio_link(args, 1),
        "vs_link_dll" => msvc::visual_studio_link(args, 2),
        "cmake_cl_compile_depends" => msvc::cl_compile_depends(args),
        "__run_co_compile" => cocompile::run_co_compile(args),
        "cmake_llvm_rc" => cocompile::run_llvm_rc(args),
        "cmake_module_compile_db" => crate::build_database::module_compile_db(args),
        "cmake_ninja_depends" | "cmake_ninja_dyndep" => match dyndep::run(command, args) {
            Ok(()) => 0,
            Err(message) => {
                error(message);
                1
            }
        },
        // cmake_depends <generator> <dirs...> <dep-info>: only Makefile
        // generators scan (cmLocalUnixMakefileGenerator3::UpdateDependencies);
        // other generators' UpdateDependencies succeeds trivially and an
        // unknown generator name fails without a message.
        "cmake_depends" if args.len() >= 4 => depends::depends(args),
        "cmake_autogen" if args.len() >= 2 => i32::from(!autogen::moc_uic(
            &args[0],
            &args[1],
            args.get(2).map_or("", String::as_str),
        )),
        "cmake_autorcc" if !args.is_empty() => i32::from(!autogen::rcc(
            &args[0],
            args.get(1).map_or("", String::as_str),
            args.get(2).map_or("", String::as_str),
        )),
        "tar" if args.len() >= 2 => archive::tar(args),
        "cmake_progress_start" if args.len() == 2 => progress_start(&args[0], &args[1]),
        // Superseded by `cmake_echo_color --progress-*`; kept so stale
        // makefiles keep working.
        "cmake_progress_report" if !args.is_empty() => 0,
        "cmake_link_script" if !args.is_empty() => link_script(args),
        "cmake_unimplemented_variable" => {
            let mut message = "Feature not implemented for this platform.".to_owned();
            if let [variable] = args {
                message.push_str(&format!("  Variable {variable} is not set."));
            }
            eprintln!("{message}");
            1
        }
        // copy_if_newer is absent from the pinned 4.1.0 cmcmd dispatcher.
        _ => {
            usage(program);
            1
        }
    }
}

fn capabilities(args: &[String]) -> i32 {
    if !args.is_empty() {
        eprintln!("-E capabilities accepts no additional arguments");
        return 1;
    }
    // cmake::ReportCapabilities uses compact, sorted JSON keys. The generator
    // array advertises the implemented Ninja generators in the factory
    // registration order (without extra generators). File API
    // versions follow the 4.1 protocol. The debugger flag reflects this build
    // (cmDebugger support is compiled in).
    println!(
        "{}",
        serde_json::json!({
            "debugger": true,
            "fileApi": { "requests": [
                {"kind": "codemodel", "version": [{"major": 2, "minor": 8}]},
                {"kind": "configureLog", "version": [{"major": 1, "minor": 0}]},
                {"kind": "cache", "version": [{"major": 2, "minor": 0}]},
                {"kind": "cmakeFiles", "version": [{"major": 1, "minor": 1}]},
                {"kind": "toolchains", "version": [{"major": 1, "minor": 0}]}
            ] },
            "generators": [
                {"extraGenerators": ["Kate"], "name": "Ninja Multi-Config", "platformSupport": false, "toolsetSupport": false},
                {"extraGenerators": [
                    "CodeBlocks", "CodeLite", "Eclipse CDT4", "Kate", "Sublime Text 2"
                ], "name": "Ninja", "platformSupport": false, "toolsetSupport": false}
            ],
            "serverMode": false,
            "tls": true,
            "version": {
                "isDirty": false, "major": interp::VERSION.0,
                "minor": interp::VERSION.1, "patch": interp::VERSION.2,
                "string": interp::VERSION_STRING, "suffix": ""
            }
        })
    );
    0
}

fn run_command(command: &mut Command, on_spawn_error: i32) -> i32 {
    match command.status() {
        Ok(status) => status.code().unwrap_or(1),
        Err(err) => {
            eprintln!("{}", process_error(&err));
            on_spawn_error
        }
    }
}

/// Scan the first `%i` conversion in a progress count file. `%i` accepts an
/// optional sign, decimal input, octal input with a leading zero, and
/// hexadecimal input with a leading `0x`.
fn scan_percent_i(input: &[u8]) -> Option<i32> {
    let mut index = 0;
    while input
        .get(index)
        .is_some_and(|byte| matches!(byte, b' ' | b'\t' | b'\n' | b'\r' | 0x0b | 0x0c))
    {
        index += 1;
    }
    let mut negative = false;
    match input.get(index) {
        Some(b'-') => {
            negative = true;
            index += 1;
        }
        Some(b'+') => index += 1,
        _ => {}
    }
    let rest = &input[index..];
    if rest.first().is_none_or(|byte| !byte.is_ascii_digit()) {
        return None;
    }

    let (base, digits) = if rest.len() >= 3
        && rest[0] == b'0'
        && matches!(rest[1], b'x' | b'X')
        && rest[2].is_ascii_hexdigit()
    {
        let end = rest[2..]
            .iter()
            .position(|byte| !byte.is_ascii_hexdigit())
            .map_or(rest.len(), |pos| pos + 2);
        (16, &rest[2..end])
    } else if rest[0] == b'0' {
        let end = rest
            .iter()
            .position(|byte| !matches!(byte, b'0'..=b'7'))
            .unwrap_or(rest.len());
        (8, &rest[..end])
    } else {
        let end = rest
            .iter()
            .position(|byte| !byte.is_ascii_digit())
            .unwrap_or(rest.len());
        (10, &rest[..end])
    };
    let mut value = 0i64;
    for &byte in digits {
        let digit = match byte {
            b'0'..=b'9' => byte - b'0',
            b'a'..=b'f' => byte - b'a' + 10,
            b'A'..=b'F' => byte - b'A' + 10,
            _ => continue,
        };
        value = value.saturating_mul(base).saturating_add(i64::from(digit));
    }
    if negative {
        value = -value;
    }
    Some(value as i32)
}

/// `cmcmdProgressReport`: touch the numbered progress marker files and
/// print the percentage represented by the directory entry count.
fn progress_report(dir: &str, num: &str) {
    let dir_name = format!("{dir}/Progress");
    let count_name = format!("{dir_name}/count.txt");
    let Ok(contents) = fs::read(&count_name) else {
        return;
    };
    let count = match scan_percent_i(&contents) {
        Some(count) => count,
        None => {
            eprintln!("Could not read from progress file.");
            0
        }
    };

    for marker in num.split(',').filter(|marker| !marker.is_empty()) {
        let _ = fs::write(format!("{dir_name}/{marker}"), "empty");
    }
    let file_num = fs::read_dir(&dir_name).map_or(0, |entries| {
        entries.filter_map(Result::ok).count() as i32 + 2
    });
    if count > 0 {
        let percent = ((file_num - 3) * 100) / count;
        print!("[{percent:3}%] ");
    }
}

/// `cmcmd::ExecuteEchoColor`.
fn echo_color(args: &[String]) -> i32 {
    use crate::stdio::TermAttr;

    let mut enabled = true;
    let mut attrs = TermAttr::Normal.bit();
    let mut newline = true;
    let mut progress_dir = String::new();
    for arg in args {
        if let Some(value) = arg.strip_prefix("--switch=") {
            if !value.is_empty() {
                enabled = util::is_on(value);
            }
        } else if let Some(value) = arg.strip_prefix("--progress-dir=") {
            progress_dir = value.to_owned();
        } else if let Some(value) = arg.strip_prefix("--progress-num=") {
            if !progress_dir.is_empty() {
                progress_report(&progress_dir, value);
            }
        } else if arg == "--normal" {
            attrs = TermAttr::Normal.bit();
        } else if arg == "--black" {
            attrs = TermAttr::ForegroundBlack.bit();
        } else if arg == "--red" {
            attrs = TermAttr::ForegroundRed.bit();
        } else if arg == "--green" {
            attrs = TermAttr::ForegroundGreen.bit();
        } else if arg == "--yellow" {
            attrs = TermAttr::ForegroundYellow.bit();
        } else if arg == "--blue" {
            attrs = TermAttr::ForegroundBlue.bit();
        } else if arg == "--magenta" {
            attrs = TermAttr::ForegroundMagenta.bit();
        } else if arg == "--cyan" {
            attrs = TermAttr::ForegroundCyan.bit();
        } else if arg == "--white" {
            attrs = TermAttr::ForegroundWhite.bit();
        } else if arg == "--bold" {
            attrs |= TermAttr::ForegroundBold.bit();
        } else if arg == "--no-newline" {
            newline = false;
        } else if arg == "--newline" {
            newline = true;
        } else {
            crate::stdio::print_out(
                if enabled {
                    attrs
                } else {
                    crate::stdio::NO_ATTRS
                },
                arg,
            );
            if newline {
                println!();
            }
        }
    }
    0
}

fn run_env(args: &[String]) -> i32 {
    let mut environment = BTreeMap::new();
    let mut index = 0;
    while let Some(arg) = args.get(index) {
        if arg == "--" {
            index += 1;
            break;
        }
        if let Some(name) = arg.strip_prefix("--unset=") {
            environment.insert(name.to_owned(), None);
        } else if arg == "--modify" {
            index += 1;
            let Some(op) = args.get(index) else {
                eprintln!("cmake -E env: --modify missing a parameter");
                return 1;
            };
            if let Err(message) = modify_env(&mut environment, op) {
                error(message);
                eprintln!("cmake -E env: invalid parameter to --modify: {op}");
                return 1;
            }
        } else if arg.starts_with('-') {
            eprintln!("cmake -E env: unknown option '{arg}'");
            return 1;
        } else if let Some((name, value)) = arg.split_once('=') {
            environment.insert(name.to_owned(), Some(value.to_owned()));
        } else {
            break;
        }
        index += 1;
    }

    let Some(program) = args.get(index) else {
        eprintln!("cmake -E env: no command given");
        return 1;
    };
    let mut command = Command::new(program);
    command.args(&args[index + 1..]);
    for (name, value) in environment {
        if name.is_empty() || name.contains('=') {
            continue;
        }
        if let Some(value) = value {
            command.env(name, value);
        } else {
            command.env_remove(name);
        }
    }
    run_command(&mut command, 1)
}

fn modify_env(diff: &mut BTreeMap<String, Option<String>>, spec: &str) -> Result<(), String> {
    let (name, spec_value) = spec
        .split_once('=')
        .ok_or_else(|| format!("Error: Missing `=` after the variable name in: {spec}\n"))?;
    let (op, value) = spec_value
        .split_once(':')
        .ok_or_else(|| format!("Error: Missing `:` after the operation in: {spec}\n"))?;
    match op {
        "reset" => {
            diff.remove(name);
        }
        "set" => {
            diff.insert(name.to_owned(), Some(value.to_owned()));
        }
        "unset" => {
            diff.insert(name.to_owned(), None);
        }
        _ => {
            let separator = match op {
                "string_append" | "string_prepend" => "",
                "path_list_append" | "path_list_prepend" => ":",
                "cmake_list_append" | "cmake_list_prepend" => ";",
                _ => {
                    return Err(format!(
                        "Error: Unrecognized environment manipulation argument: {op}\n"
                    ));
                }
            };
            let old = diff
                .get(name)
                .cloned()
                .flatten()
                .or_else(|| std::env::var(name).ok())
                .unwrap_or_default();
            let separator = if old.is_empty() { "" } else { separator };
            let new = if op.ends_with("_prepend") {
                format!("{value}{separator}{old}")
            } else {
                format!("{old}{separator}{value}")
            };
            diff.insert(name.to_owned(), Some(new));
        }
    }
    Ok(())
}

fn cat(args: &[String]) -> i32 {
    let mut options = true;
    let mut result = 0;
    let mut stdout = io::stdout().lock();
    for arg in args {
        if arg == "-" {
            options = false;
            if io::copy(&mut io::stdin().lock(), &mut stdout).is_err() {
                result = 1;
            }
            continue;
        }
        if options && arg == "--" {
            options = false;
            continue;
        }
        let diagnostic = if options && arg.starts_with('-') {
            "option not handled"
        } else if Path::new(arg).is_dir() {
            "is a directory (ignoring)"
        } else {
            match File::open(arg) {
                Ok(mut file) => {
                    if io::copy(&mut file, &mut stdout).is_err() {
                        result = 1;
                    }
                    continue;
                }
                Err(err) if err.kind() == io::ErrorKind::PermissionDenied => {
                    "permission denied (ignoring)"
                }
                Err(_) => "no such file or directory (ignoring)",
            }
        };
        error(format!("{arg}: {diagnostic}"));
        result = 1;
    }
    result
}

fn same_contents(a: impl AsRef<Path>, b: impl AsRef<Path>, ignore_eol: bool) -> bool {
    let a = a.as_ref();
    let b = b.as_ref();
    if !a.is_file() || !b.is_file() {
        return false;
    }
    if !ignore_eol {
        let (Ok(mut a), Ok(mut b)) = (File::open(a), File::open(b)) else {
            return false;
        };
        let (Ok(left), Ok(right)) = (a.metadata(), b.metadata()) else {
            return false;
        };
        if left.len() != right.len() {
            return false;
        }
        let mut remaining = left.len();
        let mut left = [0; 4096];
        let mut right = [0; 4096];
        while remaining > 0 {
            let size = remaining.min(left.len() as u64) as usize;
            let left = &mut left[..size];
            let right = &mut right[..size];
            if a.read_exact(left).is_err() || b.read_exact(right).is_err() || left != right {
                return false;
            }
            remaining -= size as u64;
        }
        return true;
    }
    let (Ok(a), Ok(b)) = (File::open(a), File::open(b)) else {
        return false;
    };
    let mut a = BufReader::new(a).split(b'\n');
    let mut b = BufReader::new(b).split(b'\n');
    loop {
        match (a.next(), b.next()) {
            (None, None) => return true,
            (Some(Ok(mut a)), Some(Ok(mut b))) => {
                if a.last() == Some(&b'\r') {
                    a.pop();
                }
                if b.last() == Some(&b'\r') {
                    b.pop();
                }
                if a != b {
                    return false;
                }
            }
            _ => return false,
        }
    }
}

fn copy(command: &str, args: &[String]) -> i32 {
    let directory = command.starts_with("copy_directory");
    let different = command.ends_with("_different");
    let mut files = Vec::new();
    let mut target = None;
    let mut index = 0;
    while index < args.len() {
        if command == "copy" && matches_value(&args[index], "-t", false) {
            let Ok(path) = read_value(
                args,
                &mut index,
                "-t",
                Values::One,
                "Invalid value used with -t",
            ) else {
                return 1;
            };
            target = Some(path);
        } else {
            files.push(args[index].clone());
        }
        index += 1;
    }
    let target_is_dir = target.is_some() || files.len() > 2;
    let Some(target) = target.or_else(|| (files.len() >= 2).then(|| files.pop()).flatten()) else {
        eprintln!("Error: No files or target specified (for copy command).");
        return 1;
    };
    let dest = Path::new(&target);
    if !directory && target_is_dir && !dest.is_dir() {
        eprintln!("Error: Target (for {command} command) \"{target}\" is not a directory.");
        return 1;
    }
    let mut result = 0;
    for source in &files {
        let path = Path::new(source);
        let copied = if directory {
            copy_directory(path, dest, different)
        } else {
            copy_path(path, dest, different)
        };
        if copied.is_err() {
            if directory {
                eprintln!("Error copying directory from \"{source}\" to \"{target}\".");
            } else if different {
                eprintln!("Error copying file (if different) from \"{source}\" to \"{target}\".");
            } else {
                eprintln!("Error copying file \"{source}\" to \"{target}\".");
            }
            result = 1;
        }
    }
    result
}

fn copy_directory(source: &Path, dest: &Path, different: bool) -> io::Result<()> {
    let mut ancestors = Vec::new();
    copy_directory_inner(source, dest, different, &mut ancestors)
}

fn copy_directory_inner(
    source: &Path,
    dest: &Path,
    different: bool,
    ancestors: &mut Vec<(u64, u64)>,
) -> io::Result<()> {
    let metadata = fs::metadata(source)?;
    if !metadata.is_dir() {
        return Err(io::ErrorKind::NotADirectory.into());
    }
    let identity = (metadata.dev(), metadata.ino());
    if ancestors.contains(&identity)
        || crate::util::is_subdirectory(
            &util::collapse_full_path(&dest.to_string_lossy(), ""),
            &fs::canonicalize(source)?.to_string_lossy(),
        )
    {
        return Err(io::Error::other("recursive copy"));
    }
    if fs::metadata(dest).is_ok_and(|dest| (dest.dev(), dest.ino()) == identity) {
        return Ok(());
    }
    fs::create_dir_all(dest)?;
    ancestors.push(identity);
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        let from = entry.path();
        let to = dest.join(entry.file_name());
        if from.is_dir() {
            copy_directory_inner(&from, &to, different, ancestors)?;
        } else {
            copy_file(&from, &to, different)?;
        }
    }
    ancestors.pop();
    Ok(())
}

fn copy_path(source: &Path, dest: &Path, different: bool) -> io::Result<()> {
    let metadata = fs::metadata(source)?;
    if metadata.is_dir() && !different {
        fs::create_dir_all(dest)?;
        return fs::set_permissions(dest, fs::Permissions::from_mode(metadata.mode()));
    }
    let destination;
    let dest = if dest.is_dir() {
        destination = dest.join(source.file_name().unwrap_or_default());
        &destination
    } else {
        dest
    };
    if metadata.is_dir() {
        fs::create_dir_all(dest)?;
        fs::set_permissions(dest, fs::Permissions::from_mode(metadata.mode()))
    } else {
        copy_file(source, dest, different)
    }
}

fn copy_file(source: &Path, dest: &Path, different: bool) -> io::Result<()> {
    let meta = fs::metadata(source)?;
    if (different && same_contents(source, dest, false))
        || fs::metadata(dest).is_ok_and(|old| (meta.dev(), meta.ino()) == (old.dev(), old.ino()))
    {
        return Ok(());
    }
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::copy(source, dest)?;
    Ok(())
}

fn make_directories(args: &[String]) -> i32 {
    let mut result = 0;
    for arg in args {
        if arg.is_empty() || fs::create_dir_all(arg).is_err() {
            eprintln!("Error creating directory \"{arg}\".");
            result = 1;
        }
    }
    result
}

fn remove_directory(dir: &str, recursive: bool) -> bool {
    if Path::new(dir).is_symlink() {
        if fs::remove_file(dir).is_err() {
            eprintln!("Error removing directory symlink \"{dir}\".");
            return false;
        }
    } else if !recursive {
        eprintln!("Error removing directory \"{dir}\" without recursive option.");
        return false;
    } else if fs::remove_dir_all(dir).is_err() {
        eprintln!("Error removing directory \"{dir}\".");
        return false;
    }
    true
}

fn remove(args: &[String]) -> i32 {
    let mut force = false;
    for arg in args {
        if matches!(arg.as_str(), "-f" | "\\-f") {
            force = true;
        } else if fs::remove_file(arg).is_err() && !force && Path::new(arg).exists() {
            return 1;
        }
    }
    0
}

fn rm(args: &[String]) -> i32 {
    let mut options = true;
    let mut force = false;
    let mut recursive = false;
    let mut any_files = false;
    let mut success = true;
    for arg in args {
        if options && arg.starts_with('-') {
            if arg == "--" {
                options = false;
            }
            if arg.contains(|c| !matches!(c, '-' | 'f' | 'r' | 'R')) {
                error(format!("Unknown -E rm argument: {arg}"));
                return 1;
            }
            force |= arg.contains('f');
            recursive |= arg.contains(['r', 'R']);
        } else if !arg.is_empty() {
            any_files = true;
            let path = Path::new(arg);
            if path.is_dir() {
                success &= remove_directory(arg, recursive);
            } else if path.symlink_metadata().is_err() && !force {
                error(format!(
                    "File to remove does not exist and force is not set: {arg}"
                ));
                success = false;
            } else if fs::remove_file(path).is_err() && path.exists() {
                error(format!("File can't be removed and still exist: {arg}"));
                success = false;
            }
        }
    }
    if !any_files {
        error("Missing file/directory to remove");
    }
    i32::from(!success || !any_files)
}

fn touch(command: &str, args: &[String]) -> i32 {
    for arg in args {
        let result = if Path::new(arg).exists() {
            let path = CString::new(arg.as_str()).expect("argument cannot contain NUL");
            // SAFETY: path is a valid C string. A null times array selects
            // the current time for both timestamps.
            let status =
                unsafe { libc::utimensat(libc::AT_FDCWD, path.as_ptr(), std::ptr::null(), 0) };
            if status == 0 {
                Ok(())
            } else {
                Err(io::Error::last_os_error())
            }
        } else if command == "touch" {
            OpenOptions::new()
                .append(true)
                .create(true)
                .open(arg)
                .map(|_| ())
        } else {
            Ok(())
        };
        if result.is_err() {
            eprintln!("cmake -E {command}: failed to update \"{arg}\".");
            return 1;
        }
    }
    0
}

fn sleep(args: &[String]) -> i32 {
    let mut total = 0.0;
    for arg in args {
        let number = arg.strip_suffix('s').unwrap_or(arg);
        let Some(value) = util::strtod_full(number).filter(|value| *value >= 0.0) else {
            eprintln!("Unknown sleep time format \"{arg}\".");
            return 1;
        };
        total += value;
    }
    let millis = if total.is_finite() {
        (total * 1000.0) as u64 as u32
    } else {
        0
    };
    std::thread::sleep(Duration::from_millis(u64::from(millis)));
    0
}

// md-5 pins digest 0.10 while sha1/sha2/sha3 use digest 0.11, so the two
// `Digest` traits are distinct. This bridge lets one `hash_sum` drive both.
trait Hasher {
    fn new() -> Self;
    fn feed(&mut self, data: &[u8]);
    fn finish(self) -> Vec<u8>;
}

impl Hasher for Md5 {
    fn new() -> Self {
        <Self as md5::Digest>::new()
    }
    fn feed(&mut self, data: &[u8]) {
        <Self as md5::Digest>::update(self, data);
    }
    fn finish(self) -> Vec<u8> {
        <Self as md5::Digest>::finalize(self).to_vec()
    }
}

macro_rules! impl_hasher {
    ($($t:ty, $krate:ident);+ $(;)?) => {
        $(
            impl Hasher for $t {
                fn new() -> Self {
                    <Self as $krate::Digest>::new()
                }
                fn feed(&mut self, data: &[u8]) {
                    <Self as $krate::Digest>::update(self, data);
                }
                fn finish(self) -> Vec<u8> {
                    <Self as $krate::Digest>::finalize(self).to_vec()
                }
            }
        )+
    };
}

impl_hasher!(Sha1, sha1; Sha224, sha2; Sha256, sha2; Sha384, sha2; Sha512, sha2);

fn hash_sum<D: Hasher>(args: &[String]) -> i32 {
    let mut failures = 0;
    for arg in args {
        if Path::new(arg).is_dir() {
            eprintln!("Error: {arg} is a directory");
            failures += 1;
        } else {
            let mut hash = D::new();
            let hashed = File::open(arg).and_then(|mut file| {
                let mut buf = [0u8; 8192];
                loop {
                    let read = file.read(&mut buf)?;
                    if read == 0 {
                        return Ok(());
                    }
                    hash.feed(&buf[..read]);
                }
            });
            if hashed.is_err() {
                eprintln!("{arg}: No such file or directory");
                failures += 1;
            } else {
                for byte in hash.finish() {
                    print!("{byte:02x}");
                }
                println!("  {arg}");
            }
        }
    }
    failures
}

fn create_link(hard: bool, source: &str, dest: &str) -> i32 {
    let kind = if hard { "hard link" } else { "symbolic link" };
    if hard && !Path::new(source).exists() {
        eprintln!("failed to create hard link because source path '{source}' does not exist");
        return 1;
    }
    if Path::new(dest).symlink_metadata().is_ok() {
        if let Err(err) = fs::remove_file(dest) {
            eprintln!(
                "failed to create {kind} '{dest}' because existing path cannot be removed: {}",
                os_error(&err)
            );
            return 1;
        }
    }
    let result = if hard {
        fs::hard_link(source, dest)
    } else {
        symlink(source, dest)
    };
    if let Err(err) = result {
        let kind = if hard { "link" } else { kind };
        error(format!(
            "failed to create {kind} '{dest}': {}",
            os_error(&err)
        ));
        return 1;
    }
    0
}

fn symlink_internal(command: &str, source: &str, dest: &str) -> i32 {
    if source == dest {
        return 0;
    }
    let _ = fs::remove_file(dest);
    if let Err(err) = symlink(util::filename_name(source), dest) {
        error(format!(
            "failed to create symbolic link '{dest}': {}",
            os_error(&err)
        ));
        error(format!("{command}: System Error: {}", os_error(&err)));
        return 1;
    }
    0
}

/// C `atoi`: optional leading whitespace and sign, then decimal digits.
fn c_atoi(text: &str) -> i32 {
    let text = text.trim_start_matches(|c: char| c.is_ascii_whitespace() || c == '\x0b');
    let (negative, digits) = match text.as_bytes().first() {
        Some(b'-') => (true, &text[1..]),
        Some(b'+') => (false, &text[1..]),
        _ => (false, text),
    };
    let value = digits
        .bytes()
        .take_while(u8::is_ascii_digit)
        .fold(0i64, |value, digit| {
            (value * 10 + i64::from(digit - b'0')).min(i64::from(u32::MAX) + 1)
        });
    (if negative { -value } else { value }) as i32
}

/// `fscanf(file, "%i", &count)`: an integer with C base prefixes (`0x`
/// hexadecimal, `0` octal), or `None` when no integer can be read.
fn scan_c_integer(text: &str) -> Option<i32> {
    let text = text.trim_start_matches(|c: char| c.is_ascii_whitespace() || c == '\x0b');
    let (negative, rest) = match text.as_bytes().first() {
        Some(b'-') => (true, &text[1..]),
        Some(b'+') => (false, &text[1..]),
        _ => (false, text),
    };
    let (radix, digits) = if let Some(hex) = rest
        .strip_prefix("0x")
        .or_else(|| rest.strip_prefix("0X"))
        .filter(|hex| hex.starts_with(|c: char| c.is_ascii_hexdigit()))
    {
        (16, hex)
    } else if rest.starts_with('0') {
        (8, rest)
    } else {
        (10, rest)
    };
    let digits: String = digits.chars().take_while(|c| c.is_digit(radix)).collect();
    if digits.is_empty() {
        return None;
    }
    let value = u64::from_str_radix(&digits, radix).unwrap_or(u64::MAX) as i64;
    Some(
        (if negative {
            value.wrapping_neg()
        } else {
            value
        }) as i32,
    )
}

/// `cmake_progress_start <dir> <count-or-file>` (cmcmd.cxx).
fn progress_start(directory: &str, count: &str) -> i32 {
    let progress = format!("{directory}/Progress");
    let _ = std::fs::remove_dir_all(&progress);
    let count = match std::fs::read(count) {
        Ok(bytes) => scan_c_integer(&String::from_utf8_lossy(&bytes)).unwrap_or_else(|| {
            eprintln!("Could not read from count file.");
            // Upstream leaves `count` uninitialized here; treat it as zero.
            0
        }),
        Err(_) => c_atoi(count),
    };
    if count != 0 {
        let _ = std::fs::create_dir_all(&progress);
        let _ = std::fs::write(format!("{progress}/count.txt"), format!("{count}\n"));
    }
    0
}

/// `cmcmd::ExecuteLinkScript`: run each non-blank line of the script as a
/// command sharing this process's stdout and stderr, stopping at the first
/// failure.
fn link_script(args: &[String]) -> i32 {
    let verbose = args
        .get(1)
        .and_then(|arg| arg.strip_prefix("--verbose="))
        .is_some_and(|value| !crate::util::is_off(value));
    let Ok(script) = std::fs::read(&args[0]) else {
        eprintln!("Error opening link script \"{}\"", args[0]);
        return 1;
    };
    // `std::cout` echoes through C stdio, so the echo is buffered exactly
    // as glibc buffers stdout (fully when piped) relative to the children.
    let echo = Stdout::open();
    let script = String::from_utf8_lossy(&script);
    let mut lines: Vec<&str> = script.split('\n').collect();
    if script.ends_with('\n') {
        lines.pop();
    }
    let mut result = 0;
    for line in lines {
        let command = line.strip_suffix('\r').unwrap_or(line);
        if command.trim_matches([' ', '\t']).is_empty() {
            continue;
        }
        let argv = crate::util::parse_unix_command_line(command);
        if verbose {
            echo.write(&format!("{command}\n"));
        }
        let Some((program, rest)) = argv.split_first() else {
            continue;
        };
        match Command::new(program).args(rest).status() {
            Ok(status) => {
                if let Some(text) = crate::commands::process::exception_text(status) {
                    echo.flush();
                    eprint!("Error running link command: {text}");
                    result = 1;
                } else if status.code() != Some(0) {
                    result = status.code().unwrap_or(1);
                }
            }
            Err(error) => {
                echo.flush();
                eprint!(
                    "Error running link command: {}",
                    crate::commands::process::spawn_error_text(&error)
                );
                result = 2;
            }
        }
        if result != 0 {
            break;
        }
    }
    result
}

/// A C stdio stream on file descriptor 1, which glibc buffers like
/// `stdout` (line buffered on a terminal, fully buffered otherwise).
struct Stdout(*mut libc::FILE);

impl Stdout {
    fn open() -> Self {
        // SAFETY: fdopen on the always-open descriptor 1 with a valid mode.
        Self(unsafe { libc::fdopen(1, c"w".as_ptr()) })
    }

    fn write(&self, text: &str) {
        if self.0.is_null() {
            print!("{text}");
            return;
        }
        // SAFETY: the stream is open and the buffer is valid for its length.
        unsafe { libc::fwrite(text.as_ptr().cast(), 1, text.len(), self.0) };
    }

    /// `std::cerr` is tied to `std::cout`: writing an error flushes it.
    fn flush(&self) {
        if !self.0.is_null() {
            // SAFETY: flush the stream without closing descriptor 1.
            unsafe { libc::fflush(self.0) };
        }
    }
}

impl Drop for Stdout {
    fn drop(&mut self) {
        self.flush();
    }
}
