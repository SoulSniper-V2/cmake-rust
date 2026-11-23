//! Portable commands exposed by `cmake -E` (cmcmd.cxx).

mod archive;
mod cocompile;
mod depends;
mod depfile;
mod dyndep;
mod fortran;

use std::collections::BTreeMap;
use std::ffi::CString;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, Read};
use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};
use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

use md5::Md5;
use sha1::Sha1;
use sha2::{Sha224, Sha256, Sha384, Sha512};

use crate::cli::{Values, matches_value, read_value};
use crate::{interp, util};

pub(crate) fn error(message: impl std::fmt::Display) {
    eprintln!("CMake Error: {message}");
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
            println!("{}", args.join(" "));
            0
        }
        "echo_append" => {
            print!("{}", args.join(" "));
            0
        }
        "true" => 0,
        "false" => 1,
        "server" => {
            error("CMake server mode has been removed in favor of the file-api.");
            1
        }
        "environment" => {
            for (name, val) in std::env::vars_os() {
                println!("{}={}", name.to_string_lossy(), val.to_string_lossy());
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
        "__run_co_compile" => cocompile::run_co_compile(args),
        "cmake_llvm_rc" => cocompile::run_llvm_rc(args),
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
        "tar" if args.len() >= 2 => archive::tar(args),
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
