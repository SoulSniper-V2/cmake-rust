//! Filename components and the legacy filesystem commands.

use std::ffi::{CStr, CString};
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use crate::cache::CacheType;
use crate::interp::{CmdErr, CmdResult, Interp, Mode, err};
use crate::messages::MessageType;
use crate::util::{expand_list, is_full_path, is_notfound, join_list};

pub fn register(interp: &mut Interp) {
    interp.register("get_filename_component", get_filename_component);
    interp.register("make_directory", make_directory);
    interp.register("remove", remove);
    interp.register("write_file", write_file);
    interp.register("aux_source_directory", aux_source_directory);
}

pub(super) fn fatal(interp: &mut Interp, message: &str) -> CmdErr {
    interp.issue(MessageType::FatalError, message);
    interp.set_fatal();
    CmdErr::Reported
}

pub(super) fn raw_error(interp: &mut Interp, message: &str) {
    interp.system_message(&format!("CMake Error: {message}"));
    interp.error = true;
}

pub(super) fn source_path(interp: &Interp, path: &str) -> String {
    if is_full_path(path) {
        path.to_owned()
    } else {
        format!("{}/{path}", interp.current_source_dir())
    }
}

pub(super) fn can_write(interp: &Interp, path: &str) -> bool {
    if !interp.is_on("CMAKE_DISABLE_SOURCE_CHANGES") {
        return true;
    }
    let source = Path::new(&interp.home_source);
    let binary = Path::new(&interp.home_binary);
    if source == binary
        || source
            .canonicalize()
            .ok()
            .zip(binary.canonicalize().ok())
            .is_some_and(|(a, b)| a == b)
    {
        return !interp.is_on("CMAKE_DISABLE_IN_SOURCE_BUILD");
    }
    let path = Path::new(path);
    !path.starts_with(source) || path == source || path.starts_with(binary)
}

pub(super) fn system_error(error: &io::Error) -> String {
    let text = error.to_string();
    match error.raw_os_error() {
        Some(code) => text
            .strip_suffix(&format!(" (os error {code})"))
            .unwrap_or(&text)
            .to_owned(),
        None => text,
    }
}

pub(super) fn mode(permission: &str) -> Option<u32> {
    Some(match permission {
        "OWNER_READ" => 0o400,
        "OWNER_WRITE" => 0o200,
        "OWNER_EXECUTE" => 0o100,
        "GROUP_READ" => 0o040,
        "GROUP_WRITE" => 0o020,
        "GROUP_EXECUTE" => 0o010,
        "WORLD_READ" => 0o004,
        "WORLD_WRITE" => 0o002,
        "WORLD_EXECUTE" => 0o001,
        "SETUID" => 0o4000,
        "SETGID" => 0o2000,
        _ => return None,
    })
}

pub(super) fn mkdir(path: &str) -> io::Result<()> {
    if path.is_empty() {
        return Ok(());
    }
    if fs::metadata(path).is_ok_and(|m| !m.is_dir()) {
        return Err(io::Error::from_raw_os_error(libc::EEXIST));
    }
    fs::create_dir_all(path)
}

pub(super) fn write(
    path: &str,
    data: &[u8],
    append: bool,
) -> Result<(), (&'static str, io::Error)> {
    let _ = mkdir(&filename_directory(path));
    let mode = fs::metadata(path).ok().map(|m| m.permissions().mode());
    if let Some(mode) = mode.filter(|mode| mode & 0o200 == 0) {
        let _ = fs::set_permissions(path, fs::Permissions::from_mode(mode | 0o220));
    }
    let result = OpenOptions::new()
        .write(true)
        .create(true)
        .append(append)
        .truncate(!append)
        .open(path)
        .map_err(|e| ("open", e))
        .and_then(|mut out| out.write_all(data).map_err(|e| ("write", e)));
    if let Some(mode) = mode.filter(|mode| mode & 0o200 == 0) {
        let _ = fs::set_permissions(path, fs::Permissions::from_mode(mode));
    }
    result
}

pub(super) fn unix_slashes(path: &str) -> String {
    let replaced = path.replace('\\', "/");
    let double = replaced.starts_with("//") && !replaced.starts_with("///");
    let mut result = String::new();
    if double {
        result.push('/');
    }
    for ch in replaced[usize::from(double)..].chars() {
        if ch != '/' || !result.ends_with('/') || result.len() == 1 && double {
            result.push(ch);
        }
    }
    if let Some(tail) = result.strip_prefix('~') {
        let user = tail.split('/').next().unwrap_or_default();
        let home = if user.is_empty() {
            std::env::var("HOME").ok()
        } else {
            home_directory(user)
        };
        if let Some(home) = home {
            result.replace_range(..user.len() + 1, &home);
        }
    }
    let bytes = result.as_bytes();
    if result.len() > 1
        && result.ends_with('/')
        && !result.ends_with("//")
        && !(result.len() == 3 && bytes[1] == b':')
    {
        result.pop();
    }
    result
}

fn home_directory(user: &str) -> Option<String> {
    let user = CString::new(user).ok()?;
    let mut record = std::mem::MaybeUninit::<libc::passwd>::uninit();
    let mut buffer = vec![0; 16 * 1024];
    let mut result = std::ptr::null_mut();
    // All pointers live through the call. The returned strings borrow buffer.
    let status = unsafe {
        libc::getpwnam_r(
            user.as_ptr(),
            record.as_mut_ptr(),
            buffer.as_mut_ptr(),
            buffer.len(),
            &mut result,
        )
    };
    if status != 0 || result.is_null() {
        return None;
    }
    Some(
        unsafe { CStr::from_ptr((*result).pw_dir) }
            .to_string_lossy()
            .into_owned(),
    )
}

fn filename_directory(path: &str) -> String {
    let mut path = unix_slashes(path);
    match path.rfind('/') {
        Some(0) => path.truncate(1),
        Some(2) if path.as_bytes()[1] == b':' => path.truncate(3),
        Some(index) => path.truncate(index),
        None => path.clear(),
    }
    path
}

fn split_root(path: &str) -> (&str, &str) {
    if let Some(rest) = path.strip_prefix("//") {
        ("//", rest)
    } else if let Some(rest) = path.strip_prefix('/') {
        ("/", rest)
    } else if path.as_bytes().get(1) == Some(&b':') && path.is_char_boundary(2) {
        let end = if path.as_bytes().get(2) == Some(&b'/') {
            3
        } else {
            2
        };
        path.split_at(end)
    } else {
        ("", path)
    }
}

pub fn collapse(path: &str, base: &str) -> String {
    let path = path.replace('\\', "/");
    let base = base.replace('\\', "/");
    let expand_home = |path: String| {
        if path.starts_with('~') {
            unix_slashes(&path)
        } else {
            path
        }
    };
    let path = expand_home(path);
    let base = expand_home(base);
    let (root, tail) = split_root(&path);
    let (root, base) = if root.is_empty() {
        split_root(&base)
    } else {
        (root, "")
    };
    let mut parts = Vec::new();
    for part in base.split('/').chain(tail.split('/')) {
        match part {
            "" | "." => {}
            ".." if parts.last().is_some_and(|last| *last != "..") => {
                parts.pop();
            }
            ".." if !root.is_empty() => {}
            _ => parts.push(part),
        }
    }
    format!("{root}{}", parts.join("/"))
}

/// kwsys SplitString skips a separator immediately after each separator it finds.
pub(super) fn split_search_path(path: &str) -> Vec<&str> {
    if path.is_empty() {
        return Vec::new();
    }
    let mut values = Vec::new();
    let mut start = 0;
    let mut separators = path.match_indices(':').map(|(i, _)| i);
    if let Some(index) = separators.next() {
        values.push(&path[..index]);
        start = index + 1;
        for index in separators {
            if index > start {
                values.push(&path[start..index]);
                start = index + 1;
            }
        }
    }
    values.push(&path[start..]);
    values
}

pub fn collapse_from_cwd(path: &str) -> String {
    let cwd = std::env::current_dir().unwrap_or_default();
    collapse(path, &cwd.to_string_lossy())
}

pub(super) fn real_path(path: &str) -> String {
    fs::canonicalize(path).map_or_else(
        |_| path.to_owned(),
        |path| path.to_string_lossy().into_owned(),
    )
}

fn make_directory(interp: &mut Interp, args: &[String]) -> CmdResult {
    let [path] = args else {
        return err("called with incorrect number of arguments");
    };
    if !can_write(interp, path) {
        interp.set_fatal();
        return err(format!(
            "attempted to create a directory: {path} into a source directory."
        ));
    }
    let _ = mkdir(path);
    Ok(())
}

fn remove(interp: &mut Interp, args: &[String]) -> CmdResult {
    if let Some((variable, items)) = args.split_first() {
        if let Some(value) = interp.get_def(variable) {
            let remove: Vec<_> = items
                .iter()
                .flat_map(|item| expand_list(item, false))
                .collect();
            let keep: Vec<_> = expand_list(value, false)
                .into_iter()
                .filter(|item| !remove.contains(item))
                .collect();
            interp.set_def(variable, join_list(&keep));
        }
    }
    Ok(())
}

fn write_file(interp: &mut Interp, args: &[String]) -> CmdResult {
    if args.len() < 2 {
        return err("called with incorrect number of arguments");
    }
    let path = &args[0];
    if !can_write(interp, path) {
        interp.set_fatal();
        return err(format!(
            "attempted to write a file: {path} into a source directory."
        ));
    }
    let mut append = false;
    let mut message = String::new();
    for argument in &args[1..] {
        if argument == "APPEND" {
            append = true;
        } else {
            message.push_str(argument);
        }
    }
    message.push('\n');
    if let Err(("open", _)) = write(path, &crate::bytes::encode(&message), append) {
        return err(format!(
            "Internal CMake error when trying to open file: {path} for writing."
        ));
    }
    Ok(())
}

fn aux_source_directory(interp: &mut Interp, args: &[String]) -> CmdResult {
    if interp.mode == Mode::Script {
        return err("command is not scriptable");
    }
    let [directory, variable] = args else {
        return err("called with incorrect number of arguments");
    };
    let mut sources: Vec<_> = fs::read_dir(source_path(interp, directory))
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| {
            name.rsplit_once('.').is_some_and(|(stem, ext)| {
                !stem.is_empty()
                    && matches!(
                        ext,
                        "c" | "C"
                            | "c++"
                            | "cc"
                            | "cpp"
                            | "cxx"
                            | "cu"
                            | "mpp"
                            | "m"
                            | "M"
                            | "mm"
                            | "ixx"
                            | "cppm"
                            | "ccm"
                            | "cxxm"
                            | "c++m"
                    )
            })
        })
        .map(|name| format!("{directory}/{name}"))
        .collect();
    sources.sort_unstable();
    let mut value = interp.get_def(variable).unwrap_or_default().to_owned();
    if !value.is_empty() {
        value.push(';');
    }
    value.push_str(&join_list(&sources));
    interp.set_def(variable, value);
    Ok(())
}

fn executable(path: &str) -> bool {
    if Path::new(path).is_dir() {
        return false;
    }
    CString::new(path).is_ok_and(|path| {
        // access checks the invoking user's actual access to this path.
        unsafe { libc::access(path.as_ptr(), libc::X_OK) == 0 }
    })
}

fn find_program(name: &str) -> Option<String> {
    if name.is_empty() {
        return None;
    }
    if executable(name) {
        return Some(collapse_from_cwd(name));
    }
    let paths = std::env::var("PATH").ok()?;
    split_search_path(&paths)
        .into_iter()
        .map(|dir| format!("{}/{name}", unix_slashes(dir)))
        .find(|path| executable(path))
        .map(|path| collapse_from_cwd(&path))
}

fn split_program(command: &str) -> Option<(String, &str)> {
    let command = command.trim_start_matches(|c: char| c.is_ascii_whitespace());
    let mut program = String::new();
    let mut escape = false;
    let mut quote = None;
    for (index, ch) in command.char_indices() {
        match ch {
            '\'' if quote == Some('\'') => quote = None,
            _ if quote == Some('\'') => program.push(ch),
            _ if escape => {
                program.push(ch);
                escape = false;
            }
            '\\' => escape = true,
            '"' if quote == Some('"') => quote = None,
            _ if quote == Some('"') => program.push(ch),
            '"' | '\'' => quote = Some(ch),
            c if c.is_ascii_whitespace() => return Some((program, &command[index..])),
            _ => program.push(ch),
        }
    }
    (quote.is_none() && !escape).then_some((program, ""))
}

fn get_filename_component(interp: &mut Interp, args: &[String]) -> CmdResult {
    if args.len() < 3 {
        interp.set_fatal();
        return err("called with incorrect number of arguments");
    }
    let target = &args[0];
    let cache = args.len() >= 4 && args.last().is_some_and(|arg| arg == "CACHE");
    if cache
        && interp
            .get_def(target)
            .is_some_and(|value| !is_notfound(value))
    {
        return Ok(());
    }
    let mut path = args[1].clone();
    while let Some(start) = path.find("[HKEY") {
        let Some(end) = path[start..].find(']') else {
            break;
        };
        path.replace_range(start..=start + end, "/registry");
    }
    let name = path.rsplit('/').next().unwrap_or_default();
    let action = args[2].as_str();
    let mut store_args = "";
    let mut program_args = "";
    let result = match action {
        "DIRECTORY" | "PATH" => filename_directory(&path),
        "NAME" => name.to_owned(),
        "EXT" | "NAME_WE" | "LAST_EXT" | "NAME_WLE" => {
            let index = if matches!(action, "EXT" | "NAME_WE") {
                name.find('.')
            } else {
                name.rfind('.')
            };
            let index = index.unwrap_or(name.len());
            if matches!(action, "EXT" | "LAST_EXT") {
                name[index..].to_owned()
            } else {
                name[..index].to_owned()
            }
        }
        "ABSOLUTE" | "REALPATH" => {
            let cwd = interp.current_source_dir();
            let base = last_keyword_value(&args[3..], "BASE_DIR").unwrap_or(&cwd);
            let path = collapse(&path, base);
            if action == "REALPATH" {
                real_path(&path)
            } else {
                path
            }
        }
        "PROGRAM" => {
            store_args = last_keyword_value(&args[3..], "PROGRAM_ARGS").unwrap_or("");
            let direct = if path.trim().is_empty() {
                None
            } else {
                find_program(&path)
            };
            let program = direct.or_else(|| {
                let (program, rest) = split_program(&path)?;
                let result = if Path::new(&program).exists() {
                    Some(program)
                } else {
                    find_program(&program)
                };
                if result.is_some() {
                    program_args = rest;
                }
                result
            });
            program.unwrap_or_default()
        }
        _ => {
            interp.set_fatal();
            return err(format!("unknown component {action}"));
        }
    };
    if cache {
        let kind = if action == "PATH" {
            CacheType::Filepath
        } else {
            CacheType::String
        };
        if !program_args.is_empty() && !store_args.is_empty() {
            interp.add_cache_def(store_args, Some(program_args), Some(""), kind, false);
        }
        interp.add_cache_def(target, Some(&result), Some(""), kind, false);
    } else {
        if !program_args.is_empty() && !store_args.is_empty() {
            interp.set_def(store_args, program_args);
        }
        interp.set_def(target, result);
    }
    Ok(())
}

fn last_keyword_value<'a>(mut args: &'a [String], keyword: &str) -> Option<&'a str> {
    let mut value = None;
    while let Some((arg, rest)) = args.split_first() {
        args = rest;
        if arg == keyword {
            if let Some((arg, rest)) = args.split_first() {
                value = Some(arg.as_str());
                args = rest;
            }
        }
    }
    value
}
