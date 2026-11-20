//! cmFileCommand and its filesystem subcommands.

mod copy;
mod glob;
mod lock;
mod rpath;
mod strings;

pub use glob::GlobDependency;
pub use lock::LockPool;

use std::ffi::CString;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, Read, Seek, SeekFrom};
use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};
use std::path::Path;


use crate::interp::{CmdErr, CmdResult, Interp, err};
use crate::messages::MessageType;
use crate::policy::{PolicyStatus, policy_warning};
use crate::util::is_full_path;

use super::configure_file::{configure_text, newline_style, write_configured};
use super::legacy_fs::{
    can_write, collapse, collapse_from_cwd, fatal, mkdir, mode, raw_error, real_path, source_path,
    split_search_path, system_error, unix_slashes, write,
};
use super::path::{append, generic, missing_values, relative};

pub fn register(interp: &mut Interp) {
    interp.register("file", command);
}

fn command(interp: &mut Interp, args: &[String]) -> CmdResult {
    let Some(action) = args.first().map(String::as_str) else {
        return err("given no arguments, but it requires at least a sub-command.");
    };
    match action {
        "READ" => read(interp, args),
        "STRINGS" => strings::run(interp, args),
        "WRITE" | "APPEND" => write_file(interp, args),
        "TOUCH" | "TOUCH_NOCREATE" => touch(interp, args),
        "GLOB" | "GLOB_RECURSE" => glob::run(interp, args),
        "MAKE_DIRECTORY" => make_directory(interp, &args[1..]),
        "REMOVE" | "REMOVE_RECURSE" => remove(interp, args),
        "RENAME" | "COPY_FILE" => rename_or_copy(interp, args),
        "COPY" | "INSTALL" => copy::run(interp, args),
        "SIZE" | "READ_SYMLINK" => size_or_read_symlink(interp, args),
        "CREATE_LINK" => create_link(interp, args),
        "CHMOD" | "CHMOD_RECURSE" => chmod(interp, args),
        "REAL_PATH" => realpath(interp, args),
        "RELATIVE_PATH" | "TO_CMAKE_PATH" | "TO_NATIVE_PATH" => convert_path(interp, args),
        "MD5" | "SHA1" | "SHA224" | "SHA256" | "SHA384" | "SHA512" | "SHA3_224" | "SHA3_256"
        | "SHA3_384" | "SHA3_512" => hash(interp, args),
        "TIMESTAMP" => timestamp(interp, args),
        "LOCK" => lock::run(interp, args),
        "CONFIGURE" => configure(interp, args),
        "DIFFERENT" => different(interp, &args[1..]),
        "RPATH_CHANGE" | "CHRPATH" | "RPATH_SET" | "RPATH_CHECK" | "RPATH_REMOVE"
        | "READ_ELF" => rpath::run(interp, args),
        "GENERATE"
        | "DOWNLOAD"
        | "UPLOAD"
        | "ARCHIVE_CREATE"
        | "ARCHIVE_EXTRACT"
        | "GET_RUNTIME_DEPENDENCIES"
        | "READ_MACHO" => {
            // SetError prefixes the command name. The unsupported command is
            // named explicitly in the diagnostic.
            Err(fatal(interp, &format!("file({action}) not yet supported")))
        }
        _ => err(format!("does not recognize sub-command {action}")),
    }
}

fn err_two(action: &str) -> CmdResult {
    err(format!(
        "{action} must be called with at least two additional arguments"
    ))
}

fn err_name_var(action: &str) -> CmdResult {
    err(format!("{action} requires a file name and output variable"))
}

fn write_file(interp: &mut Interp, args: &[String]) -> CmdResult {
    if args.len() < 2 {
        return err(format!(
            "{} must be called with at least one additional argument.",
            args[0]
        ));
    }
    let path = source_path(interp, &args[1]);
    if !can_write(interp, &path) {
        interp.set_fatal();
        return err(format!(
            "attempted to write a file: {path} into a source directory."
        ));
    }
    if let Err((action, error)) = write(&path, args[2..].concat().as_bytes(), args[0] == "APPEND") {
        let message = if action == "open" {
            "failed to open for writing"
        } else {
            "write failed"
        };
        return err(format!("{message} ({}):\n  {path}", system_error(&error)));
    }
    Ok(())
}

fn read(interp: &mut Interp, args: &[String]) -> CmdResult {
    if args.len() < 3 {
        return err_two("READ");
    }
    let path = source_path(interp, &args[1]);
    let mut hex = false;
    let mut offset = 0;
    let mut limit = usize::MAX;
    let mut pending = "";
    for arg in &args[3..] {
        match arg.as_str() {
            "HEX" => {
                hex = true;
                pending = "";
            }
            "OFFSET" | "LIMIT" => pending = arg,
            _ => {
                match pending {
                    "OFFSET" => offset = arg.trim_start().parse::<i64>().unwrap_or(0),
                    "LIMIT" => limit = arg.trim_start().parse().unwrap_or(usize::MAX),
                    _ => {}
                }
                pending = "";
            }
        }
    }
    let mut file = File::open(&path).map_err(|error| {
        CmdErr::Error(format!(
            "failed to open for reading ({}):\n  {path}",
            system_error(&error)
        ))
    })?;
    let mut data = Vec::new();
    if let Ok(offset) = u64::try_from(offset) {
        if file.seek(SeekFrom::Start(offset)).is_ok() {
            if hex {
                let _ = file.take(limit as u64).read_to_end(&mut data);
            } else {
                let mut reader = BufReader::new(file);
                let mut line = Vec::new();
                while limit > 0
                    && reader
                        .read_until(b'\n', &mut line)
                        .is_ok_and(|size| size > 0)
                {
                    let newline = line.last() == Some(&b'\n');
                    if newline {
                        line.pop();
                    }
                    if line.last() == Some(&b'\r') {
                        line.pop();
                    }
                    line.truncate(limit);
                    limit -= line.len();
                    data.extend_from_slice(&line);
                    if newline {
                        limit = limit.saturating_sub(1);
                        data.push(b'\n');
                    }
                    line.clear();
                }
            }
        }
    }
    let value = if hex {
        to_hex(&data)
    } else {
        String::from_utf8_lossy(&data).into_owned()
    };
    interp.set_def(&args[2], value);
    Ok(())
}

fn to_hex(data: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(data.len() * 2);
    for byte in data {
        output.push(HEX[(byte >> 4) as usize] as char);
        output.push(HEX[(byte & 15) as usize] as char);
    }
    output
}

fn hash(interp: &mut Interp, args: &[String]) -> CmdResult {
    let [action, path, variable] = args else {
        return err_name_var(&args[0]);
    };
    let data = match fs::read(path) {
        Ok(data) => data,
        Err(error) => {
            return err(format!(
                "{action} failed to read file \"{path}\": {}",
                system_error(&error)
            ));
        }
    };
    let digest = match action.as_str() {
        "MD5" => <md5::Md5 as md5::Digest>::digest(&data).to_vec(),
        "SHA1" => <sha1::Sha1 as sha1::Digest>::digest(&data).to_vec(),
        "SHA224" => <sha2::Sha224 as sha2::Digest>::digest(&data).to_vec(),
        "SHA256" => <sha2::Sha256 as sha2::Digest>::digest(&data).to_vec(),
        "SHA384" => <sha2::Sha384 as sha2::Digest>::digest(&data).to_vec(),
        "SHA512" => <sha2::Sha512 as sha2::Digest>::digest(&data).to_vec(),
        "SHA3_224" => <sha3::Sha3_224 as sha3::Digest>::digest(&data).to_vec(),
        "SHA3_256" => <sha3::Sha3_256 as sha3::Digest>::digest(&data).to_vec(),
        "SHA3_384" => <sha3::Sha3_384 as sha3::Digest>::digest(&data).to_vec(),
        _ => <sha3::Sha3_512 as sha3::Digest>::digest(&data).to_vec(),
    };
    interp.set_def(variable, to_hex(&digest));
    Ok(())
}

fn touch(interp: &mut Interp, args: &[String]) -> CmdResult {
    for arg in &args[1..] {
        let path = source_path(interp, arg);
        if !can_write(interp, &path) {
            interp.set_fatal();
            return err(format!(
                "attempted to touch a file: {path} in a source directory."
            ));
        }
        let result = if Path::new(&path).exists() {
            utime_now(&path)
        } else if args[0] == "TOUCH" {
            OpenOptions::new()
                .append(true)
                .create(true)
                .open(&path)
                .map(|_| ())
        } else {
            Ok(())
        };
        if result.is_err() {
            return err(format!("problem touching file: {path}"));
        }
    }
    Ok(())
}

fn utime_now(path: &str) -> io::Result<()> {
    let path = CString::new(path)?;
    // A null times argument asks the kernel to set both timestamps to now.
    let status = unsafe { libc::utimensat(libc::AT_FDCWD, path.as_ptr(), std::ptr::null(), 0) };
    if status == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

fn make_directory(interp: &mut Interp, args: &[String]) -> CmdResult {
    let mut result = "";
    let mut unexpected = Vec::new();
    let count = args
        .iter()
        .position(|arg| arg == "RESULT")
        .unwrap_or(args.len());
    let mut pending = false;
    for arg in &args[count..] {
        if arg == "RESULT" {
            pending = true;
        } else if pending {
            result = arg;
            pending = false;
        } else {
            unexpected.push(arg.as_str());
        }
    }
    if !unexpected.is_empty() {
        return err(format!(
            "MAKE_DIRECTORY called with unexpected\narguments:\n  {}",
            unexpected.join("\n")
        ));
    }
    for arg in &args[..count] {
        let path = source_path(interp, arg);
        if !can_write(interp, &path) {
            let msg = format!("attempted to create a directory: {path} into a source directory.");
            if result.is_empty() {
                interp.set_fatal();
                return err(msg);
            }
            interp.set_def(result, msg);
            return Ok(());
        }
        if let Err(error) = mkdir(&unix_slashes(&path)) {
            let error = system_error(&error);
            if result.is_empty() {
                return err(format!(
                    "failed to create directory:\n  {path}\nbecause: {error}"
                ));
            }
            interp.set_def(
                result,
                format!("Failed to create directory: {path} Error: {error}"),
            );
            return Ok(());
        }
    }
    if !result.is_empty() {
        interp.set_def(result, "0");
    }
    Ok(())
}

fn remove(interp: &mut Interp, args: &[String]) -> CmdResult {
    for arg in &args[1..] {
        if arg.is_empty() {
            interp.issue(
                MessageType::AuthorWarning,
                &format!("Ignoring empty file name in {}.", args[0]),
            );
            continue;
        }
        let path = source_path(interp, arg);
        let path = path.trim_end_matches('/');
        if args[0] == "REMOVE_RECURSE" && fs::symlink_metadata(path).is_ok_and(|meta| meta.is_dir())
        {
            let _ = fs::remove_dir_all(path);
        } else {
            let _ = fs::remove_file(path);
        }
    }
    Ok(())
}

fn result_or_error(interp: &mut Interp, variable: &str, result: Result<(), String>) -> CmdResult {
    if !variable.is_empty() {
        interp.set_def(variable, result.err().unwrap_or_else(|| "0".into()));
        return Ok(());
    }
    result.map_err(CmdErr::Error)
}

#[derive(Default)]
struct OperationOptions<'a> {
    result: &'a str,
    flags: Vec<&'a str>,
}

fn operation_options<'a>(
    args: &'a [String],
    flags: &[&str],
) -> Result<OperationOptions<'a>, &'a str> {
    let mut options = OperationOptions::default();
    let mut pending = false;
    for arg in args {
        if flags.contains(&arg.as_str()) {
            options.flags.push(arg);
            pending = false;
        } else if arg == "RESULT" {
            pending = true;
        } else if pending {
            options.result = arg;
            pending = false;
        } else {
            return Err(arg);
        }
    }
    Ok(options)
}

fn same_file(a: &str, b: &str) -> bool {
    fs::metadata(a)
        .ok()
        .zip(fs::metadata(b).ok())
        .is_some_and(|(a, b)| a.dev() == b.dev() && a.ino() == b.ino())
}

fn files_differ(a: &str, b: &str) -> bool {
    let (Ok(a), Ok(b)) = (File::open(a), File::open(b)) else {
        return true;
    };
    let mut a = BufReader::new(a);
    let mut b = BufReader::new(b);
    loop {
        match (a.fill_buf(), b.fill_buf()) {
            (Ok(left), Ok(right)) => {
                let count = left.len().min(right.len());
                if count == 0 {
                    return left.len() != right.len();
                }
                if left[..count] != right[..count] {
                    return true;
                }
                a.consume(count);
                b.consume(count);
            }
            _ => return true,
        }
    }
}

fn copy_file(source: &str, dest: &str) -> Result<(), (io::Error, &'static str)> {
    if same_file(source, dest) {
        return Ok(());
    }
    let mut source = File::open(source).map_err(|error| (error, "input"))?;
    let permissions = source.metadata().ok().map(|meta| meta.permissions());
    let mut dest = {
        let path = Path::new(dest);
        // CloneFileContent and CopyFileContentBlockwise both remove the
        // destination entry before creating the output.
        let _ = fs::remove_file(path);
        let open = || {
            OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .open(path)
        };
        open()
            .or_else(|error| {
                if error.kind() == io::ErrorKind::PermissionDenied {
                    fs::remove_file(path)?;
                    open()
                } else {
                    Err(error)
                }
            })
            .map_err(|error| (error, "output"))?
    };
    io::copy(&mut source, &mut dest).map_err(|error| (error, "output"))?;
    if let Some(permissions) = permissions {
        dest.set_permissions(permissions)
            .map_err(|error| (error, "output"))?;
    }
    Ok(())
}

fn rename_or_copy(interp: &mut Interp, args: &[String]) -> CmdResult {
    let action = &args[0];
    if args.len() < 3 {
        return err_two(action);
    }
    let source = source_path(interp, &args[1]);
    let dest = source_path(interp, &args[2]);
    let renaming = action == "RENAME";
    let flags: &[_] = if renaming {
        &["NO_REPLACE"]
    } else {
        &["INPUT_MAY_BE_RECENT", "ONLY_IF_DIFFERENT"]
    };
    let options = operation_options(&args[3..], flags)
        .map_err(|arg| CmdErr::Error(format!("{action} unknown argument:\n  {arg}")))?;
    let result = options.result;
    let error = if renaming {
        let no_replace = options.flags.contains(&"NO_REPLACE");
        let operation = if no_replace {
            fs::hard_link(&source, &dest)
        } else {
            fs::rename(&source, &dest)
        };
        operation.err().map(|error| {
            if no_replace && error.kind() == io::ErrorKind::AlreadyExists {
                if result.is_empty() {
                    "path not replaced".into()
                } else {
                    "NO_REPLACE".into()
                }
            } else {
                system_error(&error)
            }
        })
    } else {
        for (path, message) in [
            (&source, "cannot copy a directory"),
            (&dest, "cannot copy to a directory"),
        ] {
            if Path::new(path).is_dir() {
                let error = if result.is_empty() {
                    format!("COPY_FILE {message}\n  {path}")
                } else {
                    message.into()
                };
                return result_or_error(interp, result, Err(error));
            }
        }
        if options.flags.contains(&"ONLY_IF_DIFFERENT") && !files_differ(&source, &dest) {
            None
        } else {
            copy_file(&source, &dest)
                .err()
                .map(|(error, path)| format!("{} ({path})", system_error(&error)))
        }
    };
    let result_value = error.map_or(Ok(()), |error| {
        if result.is_empty() {
            let verb = if renaming { "rename" } else { "copy" };
            Err(format!(
                "{action} failed to {verb}\n  {source}\nto\n  {dest}\nbecause: {error}\n"
            ))
        } else {
            Err(error)
        }
    });
    result_or_error(interp, result, result_value)
}

fn size_or_read_symlink(interp: &mut Interp, args: &[String]) -> CmdResult {
    let [action, path, variable] = args else {
        return err_name_var(&args[0]);
    };
    if action == "SIZE" {
        match fs::metadata(path) {
            Ok(meta) if !meta.is_dir() => interp.set_def(variable, meta.len().to_string()),
            _ => {
                return err(format!(
                    "SIZE requested of path that is not readable:\n  {path}"
                ));
            }
        }
    } else {
        match fs::read_link(path) {
            Ok(link) => interp.set_def(variable, link.to_string_lossy().into_owned()),
            Err(_) => {
                return err(format!(
                    "READ_SYMLINK requested of path that is not a symlink:\n  {path}"
                ));
            }
        }
    }
    Ok(())
}

fn create_link(interp: &mut Interp, args: &[String]) -> CmdResult {
    if args.len() < 3 {
        return err_two("CREATE_LINK");
    }
    let from = &args[1];
    let to = &args[2];
    let options = operation_options(&args[3..], &["COPY_ON_ERROR", "SYMBOLIC"])
        .map_err(|arg| CmdErr::Error(format!("unknown argument: \"{arg}\"")))?;
    let symbolic = options.flags.contains(&"SYMBOLIC");
    let result = (|| {
        if from == to {
            return Err("CREATE_LINK cannot use same file and newfile".into());
        }
        if !symbolic && !Path::new(from).exists() {
            return Err(format!("Cannot hard link '{from}' as it does not exist."));
        }
        if fs::symlink_metadata(to).is_ok() {
            fs::remove_file(to).map_err(|error| {
                format!(
                    "Failed to create link '{to}' because existing path cannot be removed: {}\n",
                    system_error(&error)
                )
            })?;
        }
        let created = if symbolic {
            symlink(from, to)
        } else {
            fs::hard_link(from, to)
        };
        match created {
            Ok(()) => Ok(()),
            Err(_) if options.flags.contains(&"COPY_ON_ERROR") => copy_file_always(from, to)
                .map_err(|error| format!("Copy failed: {}", system_error(&error))),
            Err(error) => {
                let kind = if symbolic { "symbolic link" } else { "link" };
                Err(format!(
                    "failed to create {kind} '{to}': {}",
                    system_error(&error)
                ))
            }
        }
    })();
    result_or_error(interp, options.result, result)
}

fn chmod(interp: &mut Interp, args: &[String]) -> CmdResult {
    let keys = ["PERMISSIONS", "FILE_PERMISSIONS", "DIRECTORY_PERMISSIONS"];
    let mut sets = [None, None, None];
    let mut paths = Vec::new();
    let mut current = None;
    for arg in &args[1..] {
        if let Some(index) = keys.iter().position(|key| arg == key) {
            current = Some(index);
            sets[index].get_or_insert_with(Vec::new);
        } else if let Some(index) = current {
            sets[index].as_mut().unwrap().push(arg.as_str());
        } else {
            paths.push(arg.to_owned());
        }
    }
    let count = sets.iter().flatten().count();
    if count == 0 || count == 3 {
        interp.set_fatal();
        return err(if count == 0 {
            "No permissions given"
        } else {
            "Remove either PERMISSIONS or FILE_PERMISSIONS or DIRECTORY_PERMISSIONS from the invocation"
        });
    }
    let missing: Vec<_> = sets
        .iter()
        .zip(keys)
        .filter_map(|(set, key)| set.as_ref().is_some_and(Vec::is_empty).then_some(key))
        .collect();
    if !missing.is_empty() {
        return Err(missing_values(interp, &missing));
    }
    let mut modes = [None; 3];
    for (index, values) in sets.iter().enumerate() {
        if let Some(values) = values {
            let mut bits = 0;
            for value in values {
                let Some(bit) = mode(value) else {
                    interp.set_fatal();
                    return err(format!("{value} is an invalid permission specifier"));
                };
                bits |= bit;
            }
            modes[index] = Some(bits);
        }
    }
    if args[0] == "CHMOD_RECURSE" {
        paths = paths
            .into_iter()
            .flat_map(|path| {
                let mut files = if Path::new(&path).is_dir() {
                    glob::walk(&format!("{path}/*"), true, true, true).0
                } else {
                    Vec::new()
                };
                files.push(path);
                files
            })
            .collect();
    }
    for path in paths {
        let Ok(meta) = fs::metadata(&path) else {
            interp.set_fatal();
            return err(format!("does not exist:\n  {path}"));
        };
        let bits = modes[if meta.is_dir() { 2 } else { 1 }].or(modes[0]);
        if let Some(bits) = bits {
            if fs::set_permissions(&path, fs::Permissions::from_mode(bits)).is_err() {
                interp.set_fatal();
                return err(format!("Failed to set permissions for {path}"));
            }
        }
    }
    Ok(())
}

fn realpath(interp: &mut Interp, args: &[String]) -> CmdResult {
    if args.len() < 3 {
        return err("REAL_PATH requires a path and an output variable");
    }
    let cwd = interp.current_source_dir();
    let mut base = cwd.as_str();
    let mut expand_tilde = false;
    let mut pending = false;
    let mut missing = false;
    for arg in &args[3..] {
        match arg.as_str() {
            "BASE_DIRECTORY" => {
                missing |= pending;
                pending = true;
            }
            "EXPAND_TILDE" => {
                expand_tilde = true;
                missing |= pending;
                pending = false;
            }
            _ if pending => {
                base = arg;
                pending = false;
            }
            _ => return err("REAL_PATH called with unexpected arguments"),
        }
    }
    if pending || missing {
        return Err(missing_values(interp, &["BASE_DIRECTORY"]));
    }
    let mut input = args[1].clone();
    if expand_tilde && (input == "~" || input.starts_with("~/")) {
        if let Ok(home) = std::env::var("HOME") {
            input.replace_range(..1, &home);
        }
    }
    let policy = interp.policy_status("CMP0152");
    let new = || {
        if input.starts_with('/') {
            real_path(&input)
        } else {
            real_path(&generic(&append(base, &input)))
        }
    };
    let path = if policy == PolicyStatus::New {
        new()
    } else {
        let old = real_path(&collapse(&input, base));
        if policy == PolicyStatus::Warn {
            let new = new();
            if new != old {
                interp.issue(MessageType::AuthorWarning, &format!(
                    "{}\nFrom input path:\n  {input}\nthe policy OLD behavior produces path:\n  {old}\nbut the policy NEW behavior produces path:\n  {new}\nSince the policy is not set, CMake is using the OLD behavior for compatibility.",
                    policy_warning(152)
                ));
            }
        }
        old
    };
    if !Path::new(&path).exists() {
        interp.issue(
            MessageType::AuthorWarning,
            &format!("Given path:\n  {input}\ndoes not refer to an existing path on disk."),
        );
    }
    interp.set_def(&args[2], path);
    Ok(())
}

fn sys_relative(base: &str, target: &str) -> String {
    if !is_full_path(base) || !is_full_path(target) {
        return String::new();
    }
    let base = collapse_from_cwd(base);
    let target = collapse_from_cwd(target);
    if base.starts_with("//") != target.starts_with("//") {
        return target;
    }
    let mut path = relative(&target, &base);
    if path == "." {
        path.clear();
    }
    if path.rsplit('/').all(|part| part == "..") {
        path.push('/');
    }
    path
}

fn native_path(path: &str) -> String {
    let mut out = String::new();
    let mut last = '\0';
    for (index, ch) in path.chars().enumerate() {
        if ch == '/' && last == '/' && index > 1 {
            continue;
        }
        if ch == ' ' && last != '\\' {
            out.push('\\');
        }
        out.push(ch);
        last = ch;
    }
    if out.len() > 1 && out.starts_with('"') && out.ends_with('"') {
        out.remove(0);
        out.pop();
    }
    out
}

fn convert_path(interp: &mut Interp, args: &[String]) -> CmdResult {
    if args[0] == "RELATIVE_PATH" {
        let [_, variable, base, path] = args else {
            return err("RELATIVE_PATH called with incorrect number of arguments");
        };
        for (name, path) in [("directory", base), ("file", path)] {
            if !is_full_path(path) {
                return err(format!(
                    "RELATIVE_PATH must be passed a full path to the {name}: {path}"
                ));
            }
        }
        interp.set_def(variable, sys_relative(base, path));
    } else {
        let [action, path, variable] = args else {
            return err(
                "FILE([TO_CMAKE_PATH|TO_NATIVE_PATH] path result) must be called with exactly three arguments.",
            );
        };
        let convert = if action == "TO_CMAKE_PATH" {
            unix_slashes
        } else {
            native_path
        };
        let value = split_search_path(path)
            .into_iter()
            .map(convert)
            .collect::<Vec<_>>()
            .join(";");
        interp.set_def(variable, value);
    }
    Ok(())
}

fn timestamp(interp: &mut Interp, args: &[String]) -> CmdResult {
    if args.len() < 3 {
        return err("sub-command TIMESTAMP requires at least two arguments.");
    }
    if args.len() > 5 {
        return err("sub-command TIMESTAMP takes at most four arguments.");
    }
    let options = &args[3..];
    let mut format = "";
    let mut utc = false;
    if let Some(option) = options.first() {
        if option == "UTC" {
            utc = true;
        } else {
            format = option;
            if let Some(option) = options.get(1) {
                if option != "UTC" {
                    return err(format!(
                        " TIMESTAMP sub-command does not recognize option {option}."
                    ));
                }
                utc = true;
            }
        }
    }
    let path = source_path(interp, &args[1]);
    let value = fs::metadata(path).map_or_else(
        |_| String::new(),
        |meta| format_timestamp(meta.mtime(), meta.mtime_nsec() / 1_000, format, utc),
    );
    interp.set_def(&args[2], value);
    Ok(())
}

fn format_timestamp(seconds: i64, micros: i64, format: &str, utc: bool) -> String {
    let mut time = std::mem::MaybeUninit::<libc::tm>::uninit();
    let seconds = seconds as libc::time_t;
    // The reentrant conversion initializes time on success.
    let ptr = unsafe {
        if utc {
            libc::gmtime_r(&seconds, time.as_mut_ptr())
        } else {
            libc::localtime_r(&seconds, time.as_mut_ptr())
        }
    };
    if ptr.is_null() {
        return String::new();
    }
    let format = if format.is_empty() {
        if utc {
            "%Y-%m-%dT%H:%M:%SZ"
        } else {
            "%Y-%m-%dT%H:%M:%S"
        }
    } else {
        format
    };
    let mut result = String::new();
    let mut chars = format.chars();
    while let Some(ch) = chars.next() {
        if ch != '%' {
            result.push(ch);
            continue;
        }
        match chars.next() {
            Some('s') => result.push_str(&seconds.to_string()),
            Some('f') => result.push_str(&format!("{micros:06}")),
            Some(ch) if "aAbBdHIjmMSUVwyY%Zz".contains(ch) => {
                let specifier = [b'%', ch as u8, 0];
                let mut buffer = [0; 16];
                // The format is NUL terminated, the output buffer has 16 bytes,
                // and ptr points to the initialized tm for this timestamp.
                let len = unsafe {
                    libc::strftime(
                        buffer.as_mut_ptr().cast(),
                        buffer.len(),
                        specifier.as_ptr().cast(),
                        ptr,
                    )
                };
                result.push_str(&String::from_utf8_lossy(&buffer[..len]));
            }
            Some(ch) => {
                result.push('%');
                result.push(ch);
            }
            None => result.push('%'),
        }
    }
    result
}

fn configure(interp: &mut Interp, args: &[String]) -> CmdResult {
    let mut output = None;
    let mut content = None;
    let mut at_only = false;
    let mut escape_quotes = false;
    let mut missing = Vec::new();
    let mut pending = "";
    for arg in &args[1..] {
        let key = match arg.as_str() {
            "OUTPUT" => "OUTPUT",
            "CONTENT" => "CONTENT",
            "NEWLINE_STYLE" => "NEWLINE_STYLE",
            "@ONLY" => {
                at_only = true;
                "@ONLY"
            }
            "ESCAPE_QUOTES" => {
                escape_quotes = true;
                "ESCAPE_QUOTES"
            }
            _ => "",
        };
        if !key.is_empty() {
            if matches!(pending, "OUTPUT" | "CONTENT") {
                missing.push(pending);
            }
            pending = match key {
                "@ONLY" | "ESCAPE_QUOTES" => "",
                _ => key,
            };
        } else {
            match pending {
                "OUTPUT" => output = Some(arg),
                "CONTENT" => content = Some(arg),
                "NEWLINE_STYLE" => {}
                _ => {
                    interp.set_fatal();
                    return err(format!("CONFIGURE Unrecognized argument: \"{arg}\""));
                }
            }
            pending = "";
        }
    }
    if matches!(pending, "OUTPUT" | "CONTENT") {
        missing.push(pending);
    }
    if !missing.is_empty() {
        return Err(missing_values(interp, &missing));
    }
    let (Some(output), Some(content)) = (output, content) else {
        interp.set_fatal();
        let missing = if output.is_none() {
            "OUTPUT"
        } else {
            "CONTENT"
        };
        return err(format!("CONFIGURE {missing} option is mandatory."));
    };
    let newline =
        newline_style(args).map_err(|message| CmdErr::Error(format!("CONFIGURE {message}")))?;
    let output = collapse(output, &interp.current_binary_dir());
    if let Some(ch) = output.chars().find(|&ch| ch == '<' || ch == '>') {
        return err(format!(
            "CONFIGURE called with OUTPUT containing a \"{ch}\".  This character is not allowed."
        ));
    }
    if !can_write(interp, &output) {
        raw_error(
            interp,
            &format!("Attempt to write file: {output} into a source directory."),
        );
        return Err(CmdErr::Reported);
    }
    interp.ext.configure_outputs.insert(output.clone());
    let data = configure_text(
        interp,
        content.as_bytes(),
        newline,
        false,
        at_only,
        escape_quotes,
    );
    if let Err(error) = write_configured(&output, &data) {
        raw_error(
            interp,
            &format!("Could not open file for write in copy operation {output}"),
        );
        eprintln!("System Error: {}", system_error(&error));
        return Err(CmdErr::Reported);
    }
    Ok(())
}

fn different(interp: &mut Interp, args: &[String]) -> CmdResult {
    let mut variable = None;
    let mut lhs = None;
    let mut rhs = None;
    let mut position = 0;
    for arg in args {
        if arg == "FILES" {
            position = 1;
            continue;
        }
        match position {
            0 => {
                variable = Some(arg);
                position = 3;
            }
            1 => {
                lhs = Some(arg);
                position = 2;
            }
            2 => {
                rhs = Some(arg);
                position = 3;
            }
            _ => return err(format!("DIFFERENT given unknown argument {arg}")),
        }
    }
    let Some(variable) = variable else {
        return err("DIFFERENT not given result variable name.");
    };
    let (Some(lhs), Some(rhs)) = (lhs, rhs) else {
        return err("DIFFERENT not given FILES option with two file names.");
    };
    interp.set_def(variable, if files_differ(lhs, rhs) { "1" } else { "0" });
    Ok(())
}

fn copy_times(source: &str, dest: &str) -> io::Result<()> {
    let meta = fs::metadata(source)?;
    let path = CString::new(dest)?;
    let time = libc::utimbuf {
        actime: meta.atime(),
        modtime: meta.mtime(),
    };
    // cmFileTimes stores Unix timestamps with second precision using utime.
    let status = unsafe { libc::utime(path.as_ptr(), &time) };
    if status == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

fn copy_file_always(source: &str, dest: &str) -> io::Result<()> {
    let metadata = fs::metadata(source)?;
    if metadata.is_dir() {
        mkdir(&unix_slashes(dest))?;
        fs::set_permissions(dest, metadata.permissions())
    } else {
        let dest = if Path::new(dest).is_dir() {
            format!(
                "{}/{}",
                unix_slashes(dest),
                crate::util::filename_name(source)
            )
        } else {
            dest.to_owned()
        };
        mkdir(&crate::util::filename_path(&unix_slashes(&dest)))?;
        copy_file(source, &dest).map_err(|(error, _)| error)
    }
}
