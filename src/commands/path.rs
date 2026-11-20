//! POSIX cmCMakePath and cmCMakePathCommand.

use crate::interp::{CmdErr, CmdResult, Interp, err};
use crate::util::{expand_list, join_list};

pub fn register(interp: &mut Interp) {
    interp.register("cmake_path", command);
}

fn components(path: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    if path.starts_with('/') {
        parts.push("/");
    }
    parts.extend(path.split('/').filter(|part| !part.is_empty()));
    if path.ends_with('/') && !path.trim_matches('/').is_empty() {
        parts.push("");
    }
    parts
}

pub fn equal(lhs: &str, rhs: &str) -> bool {
    components(lhs) == components(rhs)
}

pub(super) fn generic(path: &str) -> String {
    let parts = components(path);
    if path.starts_with('/') {
        format!("/{}", parts[1..].join("/"))
    } else {
        parts.join("/")
    }
}

pub(super) fn append(path: &str, part: &str) -> String {
    if part.starts_with('/') || path.is_empty() {
        part.to_owned()
    } else if path.ends_with('/') {
        format!("{path}{part}")
    } else {
        format!("{path}/{part}")
    }
}

pub(super) fn normal(path: &str) -> String {
    if path.is_empty() || path.trim_matches('/').is_empty() {
        return generic(path);
    }
    let absolute = path.starts_with('/');
    let mut parts = Vec::new();
    let mut trailing = false;
    for part in path.split('/').skip(usize::from(absolute)) {
        trailing = matches!(part, "" | ".");
        match part {
            "" | "." => {}
            ".." if parts.last().is_some_and(|last| *last != "..") => {
                parts.pop();
                trailing = true;
            }
            ".." if absolute => {}
            _ => parts.push(part),
        }
    }
    let mut result = if absolute {
        String::from("/")
    } else {
        String::new()
    };
    result.push_str(&parts.join("/"));
    if result.is_empty() {
        result.push('.');
    } else if trailing && !parts.is_empty() && parts.last() != Some(&"..") {
        result.push('/');
    }
    result
}

pub(super) fn relative(path: &str, base: &str) -> String {
    if path.starts_with('/') != base.starts_with('/') {
        return String::new();
    }
    let path = components(path);
    let base = components(base);
    let common = path.iter().zip(&base).take_while(|(a, b)| a == b).count();
    let mut steps = 0isize;
    for part in &base[common..] {
        match *part {
            ".." => steps -= 1,
            "." | "" => {}
            _ => steps += 1,
        }
    }
    if steps < 0 {
        return String::new();
    }
    let mut result = vec![".."; steps as usize];
    result.extend_from_slice(&path[common..]);
    if result.is_empty() || result == [""] {
        ".".into()
    } else {
        result.join("/")
    }
}

fn filename(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or_default()
}

fn parent(path: &str) -> &str {
    let trimmed = path.trim_end_matches('/');
    if trimmed.is_empty() {
        return path;
    }
    if trimmed != path {
        return trimmed;
    }
    match path.rfind('/') {
        Some(i) => {
            let dir = path[..i].trim_end_matches('/');
            if dir.is_empty() { "/" } else { dir }
        }
        None => "",
    }
}

fn extension_index(name: &str, last: bool) -> usize {
    if matches!(name, "" | "." | "..") {
        return name.len();
    }
    let start = usize::from(name.starts_with('.'));
    let family = &name[start..];
    let dot = if last {
        family.rfind('.')
    } else {
        family.find('.')
    };
    dot.map_or(name.len(), |i| i + start)
}

fn component<'a>(path: &'a str, item: &str, last: bool) -> Option<&'a str> {
    let root = if path.starts_with('/') { "/" } else { "" };
    let name = filename(path);
    let index = extension_index(name, last);
    Some(match item {
        "ROOT_NAME" => "",
        "ROOT_DIRECTORY" | "ROOT_PATH" => root,
        "FILENAME" => name,
        "EXTENSION" => &name[index..],
        "STEM" => &name[..index],
        "RELATIVE_PART" => path.trim_start_matches('/'),
        "PARENT_PATH" => parent(path),
        _ => return None,
    })
}

fn is_prefix(prefix: &str, path: &str) -> bool {
    let prefix = components(prefix);
    let path = components(path);
    path.starts_with(&prefix)
        || prefix.last() == Some(&"")
            && path.len() >= prefix.len()
            && path.starts_with(&prefix[..prefix.len() - 1])
}

// libstdc++ std::hash<string> uses MurmurHash64A on the reference platform.
fn hash_component(component: &str) -> u64 {
    const M: u64 = 0xc6a4_a793_5bd1_e995;
    let bytes = component.as_bytes();
    let mut hash = 0xc70f_6907 ^ (bytes.len() as u64).wrapping_mul(M);
    let mut chunks = bytes.chunks_exact(8);
    for chunk in &mut chunks {
        let value = u64::from_le_bytes(chunk.try_into().unwrap()).wrapping_mul(M);
        hash = (hash ^ (value ^ (value >> 47)).wrapping_mul(M)).wrapping_mul(M);
    }
    if !chunks.remainder().is_empty() {
        let mut tail = [0; 8];
        tail[..chunks.remainder().len()].copy_from_slice(chunks.remainder());
        hash = (hash ^ u64::from_le_bytes(tail)).wrapping_mul(M);
    }
    hash = (hash ^ (hash >> 47)).wrapping_mul(M);
    hash ^ (hash >> 47)
}

fn hash_path(path: &str) -> String {
    let path = normal(path);
    let hash = components(&path).into_iter().fold(0u64, |seed, part| {
        seed ^ hash_component(part)
            .wrapping_add(0x9e37_79b9)
            .wrapping_add(seed << 6)
            .wrapping_add(seed >> 2)
    });
    format!("{hash:x}")
}

pub(super) fn missing_values(interp: &mut Interp, keys: &[&str]) -> CmdErr {
    let mut keys = keys.to_vec();
    keys.sort_unstable();
    keys.dedup();
    let message: String = keys
        .iter()
        .map(|key| format!("Error after keyword \"{key}\":\n  missing required value\n"))
        .collect();
    super::legacy_fs::fatal(interp, &message)
}

fn input<'a>(interp: &'a Interp, var: &str) -> Result<&'a str, CmdErr> {
    interp
        .get_def(var)
        .ok_or_else(|| CmdErr::Error("undefined variable for input path.".into()))
}

fn output(name: &str) -> CmdResult {
    if name.is_empty() {
        return err("Invalid name for output variable.");
    }
    Ok(())
}

fn unexpected(action: &str) -> CmdResult {
    err(format!("{action} called with unexpected arguments."))
}

fn command(interp: &mut Interp, args: &[String]) -> CmdResult {
    if args.len() < 2 {
        return err("must be called with at least two arguments.");
    }
    let action = args[0].as_str();
    let variable = &args[1];
    let tail = &args[2..];
    let mut plain = Vec::new();
    let mut normalize = false;
    let mut last = false;
    match action {
        "GET" => {
            if tail.len() < 2 {
                return err("GET must be called with at least three arguments.");
            }
            let item = &tail[0];
            if component("", item, false).is_none() {
                return err(format!("GET called with an unknown action: {item}."));
            }
            for arg in &tail[1..] {
                if arg == "LAST_ONLY" && matches!(item.as_str(), "EXTENSION" | "STEM") {
                    last = true;
                } else {
                    plain.push(arg.as_str());
                }
            }
            if plain.len() != 1 {
                return unexpected(action);
            }
            output(plain[0])?;
            let value = component(input(interp, variable)?, item, last)
                .unwrap()
                .to_owned();
            interp.set_def(plain[0], value);
        }
        "COMPARE" => {
            if tail.len() != 3 {
                return err("COMPARE must be called with four arguments.");
            }
            let same = equal(variable, &tail[1]);
            let value = match tail[0].as_str() {
                "EQUAL" => same,
                "NOT_EQUAL" => !same,
                op => {
                    return err(format!(
                        "COMPARE called with an unknown comparison operator: {op}."
                    ));
                }
            };
            output(&tail[2])?;
            interp.set_def(&tail[2], bool_string(value));
        }
        "SET" | "NATIVE_PATH" | "IS_PREFIX" | "CONVERT" => {
            let (min, max, arity) = match action {
                "SET" | "NATIVE_PATH" => (1, 2, "two or three"),
                _ => (2, 3, "three or four"),
            };
            if !(min..=max).contains(&tail.len()) {
                return err(format!("{action} must be called with {arity} arguments."));
            }
            if action == "SET" && variable.is_empty() {
                return err("Invalid name for path variable.");
            }
            if action == "CONVERT" {
                let conversion = &tail[0];
                if !matches!(
                    conversion.as_str(),
                    "TO_CMAKE_PATH_LIST" | "TO_NATIVE_PATH_LIST"
                ) {
                    return err(format!(
                        "CONVERT called with an unknown action: {conversion}."
                    ));
                }
                output(&tail[1])?;
                if tail.len() == 3 && tail[2] != "NORMALIZE" {
                    return unexpected(action);
                }
                let to_cmake = conversion == "TO_CMAKE_PATH_LIST";
                let paths = if to_cmake {
                    super::legacy_fs::split_search_path(variable)
                        .into_iter()
                        .flat_map(|item| expand_list(item, false))
                        .collect()
                } else {
                    expand_list(variable, false)
                };
                let value = paths
                    .iter()
                    .map(|path| {
                        let path = if tail.len() == 3 {
                            normal(path)
                        } else {
                            path.clone()
                        };
                        if to_cmake { generic(&path) } else { path }
                    })
                    .collect::<Vec<_>>();
                let value = if to_cmake {
                    join_list(&value)
                } else {
                    value.join(":")
                };
                interp.set_def(&tail[1], value);
                return Ok(());
            }
            for arg in tail {
                if arg == "NORMALIZE" {
                    normalize = true;
                } else {
                    plain.push(arg.as_str());
                }
            }
            if plain.len() != min {
                return unexpected(action);
            }
            let norm = |path: &str| {
                if normalize {
                    normal(path)
                } else {
                    path.to_owned()
                }
            };
            match action {
                "SET" => interp.set_def(variable, generic(&norm(plain[0]))),
                "NATIVE_PATH" => {
                    output(plain[0])?;
                    let value = norm(input(interp, variable)?);
                    interp.set_def(plain[0], value);
                }
                _ => {
                    let path = norm(input(interp, variable)?);
                    output(plain[1])?;
                    interp.set_def(plain[1], bool_string(is_prefix(&path, &norm(plain[0]))));
                }
            }
        }
        "HASH" | "IS_ABSOLUTE" | "IS_RELATIVE" | "HAS_ROOT_NAME" | "HAS_ROOT_DIRECTORY"
        | "HAS_ROOT_PATH" | "HAS_FILENAME" | "HAS_EXTENSION" | "HAS_STEM" | "HAS_RELATIVE_PART"
        | "HAS_PARENT_PATH" => {
            if tail.len() != 1 {
                return err(format!("{action} must be called with two arguments."));
            }
            let path = input(interp, variable)?;
            output(&tail[0])?;
            let value = match action {
                "HASH" => hash_path(path),
                "IS_ABSOLUTE" => bool_string(path.starts_with('/')).to_owned(),
                "IS_RELATIVE" => bool_string(!path.starts_with('/')).to_owned(),
                _ => bool_string(!component(path, &action[4..], false).unwrap().is_empty())
                    .to_owned(),
            };
            interp.set_def(&tail[0], value);
        }
        "APPEND" | "APPEND_STRING" | "REMOVE_FILENAME" | "REPLACE_FILENAME"
        | "REMOVE_EXTENSION" | "REPLACE_EXTENSION" | "NORMAL_PATH" | "RELATIVE_PATH"
        | "ABSOLUTE_PATH" => {
            if action == "APPEND" && variable.is_empty() {
                return err("Invalid name for path variable.");
            }
            let mut dest = variable.as_str();
            let default_base = interp.current_source_dir();
            let mut base = default_base.as_str();
            let mut missing = Vec::new();
            let mut empty_output = false;
            let mut pending = None;
            for arg in tail {
                let keyword = match arg.as_str() {
                    "OUTPUT_VARIABLE" => Some("OUTPUT_VARIABLE"),
                    "BASE_DIRECTORY" if matches!(action, "RELATIVE_PATH" | "ABSOLUTE_PATH") => {
                        Some("BASE_DIRECTORY")
                    }
                    "LAST_ONLY" if action.ends_with("_EXTENSION") => Some("LAST_ONLY"),
                    "NORMALIZE" if action == "ABSOLUTE_PATH" => Some("NORMALIZE"),
                    _ => None,
                };
                if let Some(keyword) = keyword {
                    missing.extend(pending.take());
                    match keyword {
                        "LAST_ONLY" => last = true,
                        "NORMALIZE" => normalize = true,
                        _ => pending = Some(keyword),
                    }
                } else if let Some(keyword) = pending.take() {
                    if keyword == "OUTPUT_VARIABLE" {
                        dest = arg;
                        empty_output |= arg.is_empty();
                    } else {
                        base = arg;
                    }
                } else {
                    plain.push(arg.as_str());
                }
            }
            missing.extend(pending);
            if empty_output {
                let mut message = String::new();
                if missing.contains(&"BASE_DIRECTORY") {
                    message.push_str(
                        "Error after keyword \"BASE_DIRECTORY\":\n  missing required value\n",
                    );
                }
                message.push_str("Error after keyword \"OUTPUT_VARIABLE\":\n");
                if missing.contains(&"OUTPUT_VARIABLE") {
                    message.push_str("  missing required value\n");
                }
                message.push_str("  empty string not allowed\n");
                return Err(super::legacy_fs::fatal(interp, &message));
            }
            if !missing.is_empty() {
                return Err(missing_values(interp, &missing));
            }
            let max = match action {
                "APPEND" | "APPEND_STRING" => usize::MAX,
                "REPLACE_FILENAME" | "REPLACE_EXTENSION" => 1,
                _ => 0,
            };
            if plain.len() > max {
                return unexpected(action);
            }
            let path = if action == "APPEND" {
                interp.get_def(variable).unwrap_or_default()
            } else {
                input(interp, variable)?
            };
            let name = filename(path);
            let prefix = &path[..path.len() - name.len()];
            let replacement = plain.first().copied().unwrap_or_default();
            let value = match action {
                "APPEND" => plain
                    .iter()
                    .fold(path.to_owned(), |path, part| generic(&append(&path, part))),
                "APPEND_STRING" => format!("{path}{}", plain.concat()),
                "REMOVE_FILENAME" => prefix.to_owned(),
                "REPLACE_FILENAME" if !name.is_empty() => append(prefix, replacement),
                "REPLACE_FILENAME" => path.to_owned(),
                "REMOVE_EXTENSION" | "REPLACE_EXTENSION" => {
                    let index = extension_index(name, last);
                    let ext = if replacement.is_empty() || replacement.starts_with('.') {
                        ""
                    } else {
                        "."
                    };
                    format!("{prefix}{}{ext}{replacement}", &name[..index])
                }
                "NORMAL_PATH" => normal(path),
                "RELATIVE_PATH" => relative(path, base),
                _ => {
                    if path.starts_with('/') {
                        if normalize {
                            normal(path)
                        } else {
                            path.to_owned()
                        }
                    } else {
                        let value = generic(&append(base, path));
                        if normalize { normal(&value) } else { value }
                    }
                }
            };
            interp.set_def(dest, value);
        }
        _ => return err(format!("does not recognize sub-command {action}")),
    }
    Ok(())
}

fn bool_string(value: bool) -> &'static str {
    if value { "ON" } else { "OFF" }
}
