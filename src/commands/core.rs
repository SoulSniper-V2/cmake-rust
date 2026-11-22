//! Core language commands: set, unset, option, message, include,
//! include_guard, cmake_policy, cmake_minimum_required, mark_as_advanced.

use crate::cache::CacheType;
use crate::interp::{CmdErr, CmdResult, Interp, err};
use crate::messages::MessageType;

// Existing configure and generator callers import message helpers from here.
#[allow(unused_imports)]
pub use super::message::{LogLevel, current_log_level, parse_log_level, status_line};
use crate::policy::{self, PolicyStatus};
use crate::policy_table::POLICIES;
use crate::util;

pub fn register(interp: &mut Interp) {
    interp.register("set", set);
    interp.register("unset", unset);
    interp.register("option", option);
    interp.register("include", include);
    interp.register("include_guard", include_guard);
    interp.register("cmake_policy", cmake_policy);
    interp.register("cmake_minimum_required", cmake_minimum_required);
    interp.register("mark_as_advanced", mark_as_advanced);
}

fn set(interp: &mut Interp, args: &[String]) -> CmdResult {
    if args.is_empty() {
        return err("called with incorrect number of arguments");
    }
    let variable = &args[0];
    if variable.starts_with("ENV{") && variable.len() > 5 {
        let name = &variable[4..variable.len() - 1];
        let current = std::env::var_os(name);
        if args.len() > 1 && !args[1].is_empty() {
            if current.as_deref() != Some(std::ffi::OsStr::new(&args[1])) {
                // SAFETY: single-threaded interpreter.
                unsafe { std::env::set_var(name, &args[1]) };
            }
            if args.len() > 2 {
                let text = format!(
                    "Only the first value argument is used when setting an environment variable.  Argument '{}' and later are unused.",
                    args[2]
                );
                interp.issue(MessageType::AuthorWarning, &text);
            }
            return Ok(());
        }
        if current.is_some() {
            // SAFETY: single-threaded interpreter.
            unsafe { std::env::remove_var(name) };
        }
        return Ok(());
    }
    if args.len() == 1 {
        interp.unset_def(variable);
        return Ok(());
    }
    if args.len() == 2 && args[1] == "PARENT_SCOPE" {
        interp.raise_scope(variable, None);
        return Ok(());
    }
    let mut cache = false;
    let mut force = false;
    let mut parent_scope = false;
    let mut ignore_last = 0usize;
    if args.len() > 1 && args.last().unwrap() == "PARENT_SCOPE" {
        parent_scope = true;
        ignore_last += 1;
    } else {
        if args.len() > 4 && args.last().unwrap() == "FORCE" {
            force = true;
            ignore_last += 1;
        }
        if args.len() > 3 && args[args.len() - 3 - usize::from(force)] == "CACHE" {
            cache = true;
            ignore_last += 3;
        }
    }
    let value = util::join_list(&args[1..args.len() - ignore_last]);
    if parent_scope {
        interp.raise_scope(variable, Some(&value));
        return Ok(());
    }
    if args.last().unwrap() == "CACHE" {
        return err("given invalid arguments for CACHE mode: missing type and docstring");
    }
    if args.len() > 1 && args[args.len() - 2] == "CACHE" {
        return err("given invalid arguments for CACHE mode: missing type or docstring");
    }
    if force && !cache {
        return err("given invalid arguments: FORCE specified without CACHE");
    }
    let mut kind = CacheType::String;
    let mut doc = String::new();
    if cache {
        let start = args.len() - 3 - usize::from(force);
        match CacheType::parse_strict(&args[start + 1]) {
            Some(parsed) => kind = parsed,
            None => {
                let text = format!(
                    "implicitly converting '{}' to 'STRING' type.",
                    args[start + 1]
                );
                interp.issue(MessageType::AuthorWarning, &text);
            }
        }
        doc = args[start + 2].clone();
    }
    if let Some(existing) = interp.cache.get(variable)
        && existing.kind != CacheType::Uninitialized
        && cache
        && kind != CacheType::Internal
        && !force
    {
        return Ok(());
    }
    if cache {
        interp.add_cache_def(variable, Some(&value), Some(&doc), kind, force);
    } else {
        interp.set_def(variable.clone(), value);
    }
    Ok(())
}

fn unset(interp: &mut Interp, args: &[String]) -> CmdResult {
    if args.is_empty() || args.len() > 2 {
        return err("called with incorrect number of arguments");
    }
    let variable = &args[0];
    if variable.starts_with("ENV{") && variable.len() > 5 {
        let name = &variable[4..variable.len() - 1];
        // SAFETY: single-threaded interpreter.
        unsafe { std::env::remove_var(name) };
        return Ok(());
    }
    if args.len() == 1 {
        interp.unset_def(variable);
        return Ok(());
    }
    match args[1].as_str() {
        "CACHE" => {
            interp.cache.remove(variable);
            Ok(())
        }
        "PARENT_SCOPE" => {
            interp.raise_scope(variable, None);
            Ok(())
        }
        _ => err("called with an invalid second argument"),
    }
}

/// `cmOptionCommand`.
fn option(interp: &mut Interp, args: &[String]) -> CmdResult {
    let args_ok = args.len() >= 2 && args.len() <= 3;
    if !args_ok {
        if args.len() > 3 {
            let text = format!(
                "called with incorrect number of arguments: {}",
                args.join(" ")
            );
            return err(text);
        }
        return err("called with incorrect number of arguments");
    }
    let name = &args[0];
    let status = interp.policy_status("CMP0077");
    // `cmOptionCommand`: WARN warns only when a normal variable existed
    // before and is cleared by the cache entry creation below.
    let exists_before = interp.get_normal_def(name).is_some();
    let check_and_warn = status == PolicyStatus::Warn && exists_before;
    // CMP0077 NEW: a normal variable of the same name wins.
    if status == PolicyStatus::New && exists_before {
        return Ok(());
    }
    if let Some(existing) = interp.cache.get(name)
        && existing.kind != CacheType::Uninitialized
    {
        if let Some(entry) = interp.cache.entries.get_mut(name) {
            entry
                .properties
                .insert("HELPSTRING".into(), args[1].clone());
        }
        return Ok(());
    }
    let initial = if args.len() == 3 {
        args[2].clone()
    } else {
        interp
            .cache
            .get(name)
            .map(|entry| entry.value.clone())
            .unwrap_or_else(|| "OFF".to_owned())
    };
    let value = if util::is_on(&initial) { "ON" } else { "OFF" };
    interp.add_cache_def(name, Some(value), Some(&args[1]), CacheType::Bool, false);
    if status != PolicyStatus::New && interp.policy_status("CMP0126") == PolicyStatus::New {
        interp.unset_def(name);
    }
    if check_and_warn && interp.get_normal_def(name).is_none() {
        let text = format!(
            "{}\nFor compatibility with older versions of CMake, option is clearing the normal variable '{name}'.",
            policy::policy_warning(77)
        );
        interp.issue(MessageType::AuthorWarning, &text);
    }
    Ok(())
}

/// Resolves a module name to a file in CMAKE_MODULE_PATH or the CMake root.
pub fn find_module(interp: &Interp, name: &str) -> Option<String> {
    let file = format!("{name}.cmake");
    if let Some(paths) = interp.get_def("CMAKE_MODULE_PATH") {
        for directory in util::expand_list(paths, false) {
            let candidate = format!("{}/{file}", directory.trim_end_matches('/'));
            if std::path::Path::new(&candidate).is_file() {
                return Some(util::collapse_full_path(&candidate, ""));
            }
        }
    }
    let candidate = format!("{}/Modules/{file}", interp.cmake_root);
    if std::path::Path::new(&candidate).is_file() {
        return Some(candidate);
    }
    None
}

/// `cmMakefile::GetModulesFile` as used by `include()`: looks for
/// `filename` (which already carries the `.cmake` suffix) in every
/// CMAKE_MODULE_PATH directory and then in `<cmake_root>/Modules`.
/// Returns the found path (empty when nothing matched) and the `system`
/// flag that `cmIncludeCommand` uses for its deprecated-module checks:
/// a CMAKE_MODULE_PATH hit is not "system" unless the calling list file
/// itself lives under `<cmake_root>/Modules`, in which case the
/// CMake-root copy wins.
fn find_modules_file(interp: &Interp, filename: &str) -> (String, bool) {
    let mut in_module_path = String::new();
    if let Some(paths) = interp.get_def("CMAKE_MODULE_PATH") {
        for directory in util::expand_list(paths, false) {
            let candidate = format!("{directory}/{filename}");
            if std::path::Path::new(&candidate).exists() {
                in_module_path = candidate;
                break;
            }
        }
    }
    let mut in_root = format!("{}/Modules/{filename}", interp.cmake_root);
    if !std::path::Path::new(&in_root).exists() {
        in_root.clear();
    }
    if !in_module_path.is_empty() && !in_root.is_empty() {
        let modules = format!("{}/Modules/", interp.cmake_root);
        match interp.get_def("CMAKE_CURRENT_LIST_FILE") {
            Some(current) if util::is_subdirectory(current, &modules) => (in_root, true),
            _ => (in_module_path, false),
        }
    } else if !in_module_path.is_empty() {
        (in_module_path, false)
    } else {
        (in_root, true)
    }
}

/// The `DeprecatedModules` table of `cmIncludeCommand`: module name to the
/// policy whose status decides whether including it is allowed.
fn deprecated_module_policy(name: &str) -> Option<&'static str> {
    let id = match name {
        "CMakeDetermineVSServicePack" => "CMP0196",
        "CMakeFindFrameworks" => "CMP0173",
        "Dart" | "FindDart" => "CMP0145",
        "Documentation" => "CMP0106",
        "FindBoost" => "CMP0167",
        "FindCABLE" => "CMP0191",
        "FindCUDA" => "CMP0146",
        "FindGCCXML" => "CMP0188",
        "FindPythonInterp" | "FindPythonLibs" => "CMP0148",
        "WriteCompilerDetectionHeader" => "CMP0120",
        _ => return None,
    };
    Some(id)
}

/// `cmIncludeCommand`.
fn include(interp: &mut Interp, args: &[String]) -> CmdResult {
    if args.is_empty() || args.len() > 4 {
        return err("called with wrong number of arguments.  include() only takes one file.");
    }
    let mut optional = false;
    let mut no_policy_scope = false;
    let mut result_var = String::new();
    let mut index = 1;
    while index < args.len() {
        if args[index] == "OPTIONAL" {
            if optional {
                return err("called with invalid arguments: OPTIONAL used twice");
            }
            optional = true;
        } else if args[index] == "RESULT_VARIABLE" {
            if !result_var.is_empty() {
                return err("called with invalid arguments: only one result variable allowed");
            }
            index += 1;
            if index < args.len() {
                result_var = args[index].clone();
            } else {
                return err("called with no value for RESULT_VARIABLE.");
            }
        } else if args[index] == "NO_POLICY_SCOPE" {
            no_policy_scope = true;
        } else if index > 1 {
            // compat.: in previous cmake versions the second parameter was
            // ignored if it wasn't "OPTIONAL"
            let text = format!("called with invalid argument: {}", args[index]);
            return err(text);
        }
        index += 1;
    }
    let mut file = args[0].clone();
    if file.is_empty() {
        interp.issue(
            MessageType::AuthorWarning,
            "include() given empty file name (ignored).",
        );
        return Ok(());
    }
    if !util::is_full_path(&file) {
        // Not a path.  Maybe a module (<name>.cmake in CMAKE_MODULE_PATH
        // or the CMake module directory).
        let (mut mfile, system) = find_modules_file(interp, &format!("{file}.cmake"));
        if system && let Some(policy_id) = deprecated_module_policy(&file) {
            match interp.policy_status(policy_id) {
                PolicyStatus::Warn => {
                    if let Some(policy_index) = policy::index_of(policy_id) {
                        let text = format!("{}\n", policy::policy_warning(policy_index));
                        interp.issue(MessageType::AuthorWarning, &text);
                    }
                }
                PolicyStatus::Old => {}
                PolicyStatus::New => mfile.clear(),
            }
        }
        if !mfile.is_empty() {
            file = mfile;
        }
    }
    let full = util::collapse_full_path(&file, &interp.current_source_dir());
    // cmGlobalGenerator::IsExportedTargetsFile (CMP0024).
    if interp.is_exported_targets_file(&full) {
        let text = format!(
            "The file\n  {full}\nwas generated by the export() command.  It may not be used as the argument to the include() command.  Use ALIAS targets instead to refer to targets by alternative names.\n"
        );
        interp.issue(MessageType::FatalError, &text);
        return Err(CmdErr::Reported);
    }
    let path = std::path::Path::new(&full);
    let missing = !path.exists();
    let directory = path.is_dir();
    if missing || directory {
        if !result_var.is_empty() {
            interp.set_def(result_var.clone(), "NOTFOUND");
        }
        if optional {
            return Ok(());
        }
        if missing {
            return err(format!("could not find requested file:\n  {file}"));
        }
        return err(format!("requested file is a directory:\n  {file}"));
    }
    let policy_depth = interp.mf().policies.len();
    let ok = interp.read_dependent_file(&full, no_policy_scope);
    // `cmMakefile::PopSnapshot` rejects policy scopes the included file
    // left open even when NO_POLICY_SCOPE skipped the include's own push.
    interp.include_policy_scope_cleanup(&full, policy_depth);
    if !result_var.is_empty() {
        interp.set_def(
            result_var.clone(),
            if ok {
                full.clone()
            } else {
                "NOTFOUND".to_owned()
            },
        );
    }
    if !optional && !ok && !interp.fatal {
        return err(format!("could not load requested file:\n  {file}"));
    }
    Ok(())
}

fn include_guard(interp: &mut Interp, args: &[String]) -> CmdResult {
    if args.len() > 1 {
        return err("given an invalid number of arguments. The command takes at most 1 argument.");
    }
    let scope = args.first().map(String::as_str).unwrap_or("VARIABLE");
    let file = interp.get_safe("CMAKE_CURRENT_LIST_FILE");
    let marker = format!("__INCGUARD_{}__", md5_hex(&file));
    let guarded = match scope {
        "VARIABLE" => {
            let guarded = interp.is_def_set(&marker);
            interp.set_def(marker, "TRUE");
            guarded
        }
        "DIRECTORY" => {
            let mut directory = Some(interp.mf_index());
            let mut found = false;
            while let Some(index) = directory {
                if interp.makefiles[index].props.get(&marker).is_some() {
                    found = true;
                    break;
                }
                directory = interp.makefiles[index].parent;
            }
            interp.mf_mut().props.set(&marker, "TRUE".into());
            found
        }
        "GLOBAL" => {
            let found = interp.global_props.get(&marker).is_some();
            interp.global_props.set(&marker, "TRUE".into());
            found
        }
        other => {
            let text = format!("given an invalid scope: {other}");
            return err(text);
        }
    };
    if guarded {
        interp.request_return = true;
    }
    Ok(())
}

fn md5_hex(text: &str) -> String {
    use md5::{Digest, Md5};
    let digest = Md5::digest(text.as_bytes());
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn set_policy_by_name(interp: &mut Interp, id: &str, status: PolicyStatus) -> bool {
    match policy::index_of(id) {
        Some(index) => interp.set_policy(index, status),
        None => {
            let text = format!("Policy \"{id}\" is not known to this version of CMake.");
            interp.issue(MessageType::FatalError, &text);
            false
        }
    }
}

fn cmake_policy(interp: &mut Interp, args: &[String]) -> CmdResult {
    if args.is_empty() {
        return err("requires at least one argument.");
    }
    match args[0].as_str() {
        "SET" => {
            if args.len() != 3 {
                return err("SET must be given exactly 2 additional arguments.");
            }
            let status = match args[2].as_str() {
                "OLD" => PolicyStatus::Old,
                "NEW" => PolicyStatus::New,
                other => {
                    return err(format!("SET given unrecognized policy status \"{other}\""));
                }
            };
            if !set_policy_by_name(interp, &args[1], status) {
                return err("SET failed to set policy.");
            }
            Ok(())
        }
        "GET" => {
            let parent_scope = args.len() == 4 && args[3] == "PARENT_SCOPE";
            if args.len() != 3 && !parent_scope {
                return err("GET must be given exactly 2 additional arguments.");
            }
            let Some(index) = policy::index_of(&args[1]) else {
                return err(format!(
                    "GET given policy \"{}\" which is not known to this version of CMake.",
                    args[1]
                ));
            };
            let status = if parent_scope {
                // Skip the top-most policy frame.
                let frames = &interp.mf().policies;
                let mut found = None;
                if POLICIES[index].removed {
                    found = Some(PolicyStatus::New);
                } else {
                    for frame in frames.iter().rev().skip(1) {
                        if let Some(status) = frame.map.get(index) {
                            found = Some(status);
                            break;
                        }
                    }
                }
                found.unwrap_or(PolicyStatus::Warn)
            } else {
                interp.policy_status(&args[1])
            };
            let value = match status {
                PolicyStatus::Old => "OLD",
                PolicyStatus::Warn => "",
                PolicyStatus::New => "NEW",
            };
            interp.set_def(args[2].clone(), value);
            Ok(())
        }
        "PUSH" => {
            if args.len() > 1 {
                return err("PUSH may not be given additional arguments.");
            }
            interp.push_policy(false, crate::policy::PolicyMap::default());
            Ok(())
        }
        "POP" => {
            if args.len() > 1 {
                return err("POP may not be given additional arguments.");
            }
            interp.pop_policy();
            Ok(())
        }
        "VERSION" => {
            if args.len() <= 1 {
                return err("VERSION not given an argument");
            }
            if args.len() >= 3 {
                return err("VERSION given too many arguments");
            }
            let (min, max) = split_version_range(&args[1])?;
            if !apply_policy_version(interp, &min, &max) {
                return Err(CmdErr::Reported);
            }
            Ok(())
        }
        "GET_WARNING" => {
            if args.len() != 3 {
                return err("GET_WARNING must be given exactly 2 additional arguments.");
            }
            let Some(index) = policy::index_of(&args[1]) else {
                return err(format!(
                    "GET_WARNING given policy \"{}\" which is not known to this version of CMake.",
                    args[1]
                ));
            };
            interp.set_def(args[2].clone(), policy::policy_warning(index));
            Ok(())
        }
        other => err(format!("given unknown first argument \"{other}\"")),
    }
}

fn split_version_range(text: &str) -> Result<(String, String), CmdErr> {
    match text.find("...") {
        Some(position) => {
            let min = &text[..position];
            let max = &text[position + 3..];
            if min.is_empty() || max.is_empty() {
                return err(format!(
                    "VERSION \"{text}\" does not have a version on both sides of \"...\"."
                ));
            }
            Ok((min.to_owned(), max.to_owned()))
        }
        None => Ok((text.to_owned(), String::new())),
    }
}

/// `sscanf("%u.%u.%u.%u")`: returns parsed components and the count.
pub fn scan_version(text: &str) -> ([u32; 4], usize) {
    let mut parts = [0u32; 4];
    let mut count = 0;
    let bytes = text.as_bytes();
    let mut index = 0;
    while count < 4 {
        // %u skips leading whitespace and accepts an optional sign.
        while index < bytes.len() && bytes[index].is_ascii_whitespace() {
            index += 1;
        }
        let start = index;
        if index < bytes.len() && (bytes[index] == b'+' || bytes[index] == b'-') {
            index += 1;
        }
        let digits = index;
        while index < bytes.len() && bytes[index].is_ascii_digit() {
            index += 1;
        }
        if index == digits {
            break;
        }
        let value: i64 = text[start..index].parse().unwrap_or(0);
        parts[count] = value as u32;
        count += 1;
        if count < 4 {
            if index < bytes.len() && bytes[index] == b'.' {
                index += 1;
            } else {
                break;
            }
        }
    }
    (parts, count)
}

const ADVICE_UPDATE_VERSION_ARGUMENT: &str = "Update the VERSION argument <min> value.  Or, use the <min>...<max> syntax to tell CMake that the project requires at least <min> but has been updated to work with policies introduced by <max> or earlier.";

/// `cmPolicies::ApplyPolicyVersion`.
pub fn apply_policy_version(interp: &mut Interp, min: &str, max: &str) -> bool {
    apply_policy_version_ex(interp, min, max, true)
}

pub fn apply_policy_version_ex(
    interp: &mut Interp,
    min: &str,
    max: &str,
    warn_compat: bool,
) -> bool {
    let (parts, count) = scan_version(min);
    if count < 2 {
        let text = format!(
            "Invalid policy version value \"{min}\".  A numeric major.minor[.patch[.tweak]] must be given."
        );
        interp.issue(MessageType::FatalError, &text);
        return false;
    }
    let (min_major, min_minor, min_patch, min_tweak) = (parts[0], parts[1], parts[2], parts[3]);
    if min_major < 2 || (min_major == 2 && min_minor < 4) {
        interp.issue(
            MessageType::FatalError,
            "Compatibility with CMake < 2.4 is not supported by CMake >= 3.0.  For compatibility with older versions please use any CMake 2.8.x release or lower.",
        );
        return false;
    }
    let current = crate::interp::VERSION;
    if (min_major, min_minor, min_patch, min_tweak) > (current.0, current.1, current.2, 0) {
        let text = format!(
            "An attempt was made to set the policy version of CMake to \"{min}\" which is greater than this version of CMake.  This is not allowed because the greater version may have new policies not known to this CMake.  You may need a newer CMake version to build this project."
        );
        interp.issue(MessageType::FatalError, &text);
        return false;
    }
    let (mut major, mut minor, mut patch) = (min_major, min_minor, min_patch);
    if !max.is_empty() {
        let (max_parts, max_count) = scan_version(max);
        if max_count < 2 {
            let text = format!(
                "Invalid policy max version value \"{max}\".  A numeric major.minor[.patch[.tweak]] must be given."
            );
            interp.issue(MessageType::FatalError, &text);
            return false;
        }
        if (min_major, min_minor, min_patch, min_tweak)
            > (max_parts[0], max_parts[1], max_parts[2], max_parts[3])
        {
            let text = format!(
                "Policy VERSION range \"{min}...{max}\" specifies a larger minimum than maximum."
            );
            interp.issue(MessageType::FatalError, &text);
            return false;
        }
        major = max_parts[0];
        minor = max_parts[1];
        patch = max_parts[2];
    }
    // CMAKE_POLICY_VERSION_MINIMUM
    if let Some(minimum) = interp
        .get_def("CMAKE_POLICY_VERSION_MINIMUM")
        .map(str::to_owned)
        && !minimum.is_empty()
    {
        let (var_parts, var_count) = scan_version(&minimum);
        if var_count < 2 {
            let text = format!(
                "Invalid CMAKE_POLICY_VERSION_MINIMUM value \"{minimum}\".  A numeric major.minor[.patch[.tweak]] must be given."
            );
            interp.issue(MessageType::FatalError, &text);
            return false;
        }
        if (var_parts[0], var_parts[1], var_parts[2]) > (major, minor, patch) {
            major = var_parts[0];
            minor = var_parts[1];
            patch = var_parts[2];
        }
    }
    if major < 3 || (major == 3 && minor < 5) {
        let text = format!(
            "Compatibility with CMake < 3.5 has been removed from CMake.\n{ADVICE_UPDATE_VERSION_ARGUMENT}\nOr, add -DCMAKE_POLICY_VERSION_MINIMUM=3.5 to try configuring anyway."
        );
        interp.issue(MessageType::FatalError, &text);
        interp.set_fatal();
        return false;
    } else if major == 3 && minor < 10 && warn_compat {
        let text = format!(
            "Compatibility with CMake < 3.10 will be removed from a future version of CMake.\n{ADVICE_UPDATE_VERSION_ARGUMENT}"
        );
        interp.issue(MessageType::DeprecationWarning, &text);
    }
    for (index, entry) in POLICIES.iter().enumerate() {
        if policy::newer_than(index, major, minor, patch) {
            if entry.removed {
                continue;
            }
            let mut status = PolicyStatus::Warn;
            let variable = format!("CMAKE_POLICY_DEFAULT_{}", entry.id);
            if let Some(value) = interp.get_def(&variable).map(str::to_owned)
                && !value.is_empty()
            {
                match value.as_str() {
                    "OLD" => status = PolicyStatus::Old,
                    "NEW" => status = PolicyStatus::New,
                    other => {
                        let text = format!(
                            "Policy {} has invalid value \"{other}\" in variable {variable}.  It must be \"OLD\", \"NEW\", or empty.",
                            entry.id
                        );
                        interp.issue(MessageType::FatalError, &text);
                        return false;
                    }
                }
            }
            if !interp.set_policy(index, status) {
                return false;
            }
        } else if !interp.set_policy(index, PolicyStatus::New) {
            return false;
        }
    }
    true
}

fn cmake_minimum_required(interp: &mut Interp, args: &[String]) -> CmdResult {
    let mut version: Option<String> = None;
    let mut doing_version = false;
    let mut unknown: Vec<String> = Vec::new();
    for arg in args {
        if arg == "VERSION" {
            doing_version = true;
        } else if arg == "FATAL_ERROR" {
            if doing_version {
                return err("called with no value for VERSION.");
            }
            doing_version = false;
        } else if doing_version {
            doing_version = false;
            version = Some(arg.clone());
        } else {
            unknown.push(arg.clone());
        }
    }
    if doing_version {
        return err("called with no value for VERSION.");
    }
    let Some(version) = version.filter(|value| !value.is_empty()) else {
        return enforce_unknown(interp, "", &unknown);
    };
    let (min, max) = split_version_range(&version)?;
    interp.set_def("CMAKE_MINIMUM_REQUIRED_VERSION", min.clone());
    let (parts, count) = scan_version(&min);
    if count < 2 {
        return err(format!("could not parse VERSION \"{min}\"."));
    }
    let current = crate::interp::VERSION;
    if (parts[0], parts[1], parts[2], parts[3]) > (current.0, current.1, current.2, 0) {
        let text = format!(
            "CMake {min} or higher is required.  You are running version {}",
            crate::interp::VERSION_STRING
        );
        interp.issue(MessageType::FatalError, &text);
        interp.set_fatal();
        return Ok(());
    }
    enforce_unknown(interp, &max, &unknown)?;
    let ok = if parts[0] < 2 || (parts[0] == 2 && parts[1] < 4) {
        interp.issue(
            MessageType::AuthorWarning,
            "Compatibility with CMake < 2.4 is not supported by CMake >= 3.0.",
        );
        apply_policy_version(interp, "2.4", &max)
    } else {
        apply_policy_version(interp, &min, &max)
    };
    let _ = ok;
    Ok(())
}

fn enforce_unknown(_interp: &mut Interp, max: &str, unknown: &[String]) -> CmdResult {
    if unknown.is_empty() {
        return Ok(());
    }
    let (parts, count) = scan_version(max);
    if count >= 2 {
        let current = crate::interp::VERSION;
        if (current.0, current.1, current.2, 0) < (parts[0], parts[1], parts[2], parts[3]) {
            return Ok(());
        }
    }
    err(format!("called with unknown argument \"{}\".", unknown[0]))
}

fn mark_as_advanced(interp: &mut Interp, args: &[String]) -> CmdResult {
    if args.is_empty() {
        return err("called with incorrect number of arguments");
    }
    let mut index = 0;
    let mut value = "1";
    let mut overwrite = false;
    if args[0] == "CLEAR" || args[0] == "FORCE" {
        overwrite = true;
        if args[0] == "CLEAR" {
            value = "0";
        }
        index = 1;
    }
    for name in &args[index..] {
        let policy = interp.policy_status("CMP0102");
        let in_cache = interp.cache.value(name).is_some();
        // `cmMarkAsAdvancedCommand`: CMP0102 WARN emits a bespoke author
        // warning (not the generic policy warning text).
        if policy == PolicyStatus::Warn
            && !in_cache
            && interp
                .policy_optional_warning(interp.get_def("CMAKE_POLICY_WARNING_CMP0102"))
        {
            interp.issue(
                MessageType::AuthorWarning,
                &format!(
                    "Policy CMP0102 is not set: The variable named \"{name}\" is not in the cache. This results in an empty cache entry which is no longer created when policy CMP0102 is set to NEW. Run \"cmake --help-policy CMP0102\" for policy details. Use the cmake_policy command to set the policy and suppress this warning."
                ),
            );
        }
        match interp.cache.entries.get_mut(name) {
            Some(entry) => {
                if entry.kind == CacheType::Uninitialized {
                    entry.kind = CacheType::String;
                }
                if overwrite || !entry.properties.contains_key("ADVANCED") {
                    entry.properties.insert("ADVANCED".into(), value.into());
                }
            }
            None => {
                if policy == PolicyStatus::New {
                    continue;
                }
                interp.cache.add(name, None, None, CacheType::Uninitialized);
                if let Some(entry) = interp.cache.entries.get_mut(name) {
                    entry.properties.insert("ADVANCED".into(), value.into());
                }
            }
        }
    }
    Ok(())
}
