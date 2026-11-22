//! cmInstallCommand and the three legacy project install commands.
use std::collections::BTreeMap;
use std::path::Path;

use crate::install_model::*;
use crate::interp::{CmdErr, CmdResult, Interp, Mode, err};
use crate::messages::MessageType;
use crate::policy::{PolicyStatus, index_of, policy_warning};
use crate::state::{TargetKind, find_target_to_use};
use crate::util::{expand_list, is_full_path};

pub fn register(interp: &mut Interp) {
    interp.register("install", install);
    if interp.mode == Mode::Project {
        interp.register("install_files", legacy_files);
        interp.register("install_programs", legacy_programs);
        interp.register("install_targets", legacy_targets);
    }
}

// cmArgumentParser consumes a scalar once, a list up to the next keyword, and
// gathers unconsumed arguments for the caller's signature-specific diagnostic.
#[derive(Default)]
pub(crate) struct Arguments<'a> {
    pub values: BTreeMap<&'a str, Vec<String>>,
    pub unknown: Vec<String>,
}

impl Arguments<'_> {
    pub fn has(&self, key: &str) -> bool { self.values.contains_key(key) }
    pub fn get(&self, key: &str) -> &str {
        self.list(key).last().map_or("", String::as_str)
    }
    pub fn list(&self, key: &str) -> &[String] {
        self.values.get(key).map_or(&[], Vec::as_slice)
    }
}

pub(super) fn parse<'a>(
    args: &[String], singles: &[&'a str], lists: &[&'a str], flags: &[&'a str],
) -> Arguments<'a> {
    let mut result = Arguments::default();
    let mut active = None;
    for arg in args {
        if let Some(&key) = flags.iter().find(|&&key| key == arg) {
            result.values.entry(key).or_default();
            active = None;
        } else if let Some(&key) = singles.iter().find(|&&key| key == arg) {
            result.values.entry(key).or_default();
            active = Some((key, false));
        } else if let Some(&key) = lists.iter().find(|&&key| key == arg) {
            result.values.entry(key).or_default();
            active = Some((key, true));
        } else if let Some((key, multiple)) = active {
            let values = result.values.entry(key).or_default();
            if !multiple {
                values.clear();
                active = None;
            }
            values.push(arg.clone());
        } else {
            result.unknown.push(arg.clone());
        }
    }
    result
}

const SINGLES: &[&str] = &["DESTINATION", "COMPONENT", "NAMELINK_COMPONENT", "RENAME", "TYPE"];
const LISTS: &[&str] = &["CONFIGURATIONS", "PERMISSIONS"];
const FLAGS: &[&str] = &["OPTIONAL", "EXCLUDE_FROM_ALL", "NAMELINK_ONLY", "NAMELINK_SKIP"];
const FILTERS: &[&str] = &[
    "DIRECTORIES", "PRE_INCLUDE_REGEXES", "PRE_EXCLUDE_REGEXES", "POST_INCLUDE_REGEXES",
    "POST_EXCLUDE_REGEXES", "POST_INCLUDE_FILES", "POST_EXCLUDE_FILES",
];
const TARGET_GROUPS: &[&str] = &[
    "ARCHIVE", "LIBRARY", "RUNTIME", "OBJECTS", "FRAMEWORK", "BUNDLE", "INCLUDES",
    "PRIVATE_HEADER", "PUBLIC_HEADER", "RESOURCE", "FILE_SET", "CXX_MODULES_BMI",
];
const IMPORT_GROUPS: &[&str] = &["LIBRARY", "RUNTIME", "FRAMEWORK", "BUNDLE"];

fn parse_options<'a>(
    args: &[String], singles: &[&'a str], lists: &[&'a str], flags: &[&'a str],
) -> Arguments<'a> {
    parse(args, &[SINGLES, singles].concat(), &[LISTS, lists].concat(), &[FLAGS, flags].concat())
}

pub(super) fn unknown(mode: &str, argument: &str) -> CmdErr {
    CmdErr::Error(format!("{mode} given unknown argument \"{argument}\"."))
}

fn default_component(interp: &Interp) -> String {
    nonempty(interp.get_def("CMAKE_INSTALL_DEFAULT_COMPONENT_NAME").unwrap_or(""), "Unspecified").to_owned()
}

fn nonempty<'a>(value: &'a str, default: &'a str) -> &'a str {
    if value.is_empty() { default } else { value }
}

fn message_level(interp: &Interp) -> String {
    match interp.get_def("CMAKE_INSTALL_MESSAGE") {
        Some(level @ ("ALWAYS" | "LAZY" | "NEVER")) => format!("MESSAGE_{level}"),
        _ => String::new(),
    }
}

fn valid_permission(permission: &str) -> bool {
    matches!(permission, "OWNER_READ" | "OWNER_WRITE" | "OWNER_EXECUTE" | "GROUP_READ"
        | "GROUP_WRITE" | "GROUP_EXECUTE" | "WORLD_READ" | "WORLD_WRITE"
        | "WORLD_EXECUTE" | "SETUID" | "SETGID")
}

pub(crate) fn unix_slashes(value: &str) -> String {
    let path = value.replace('\\', "/");
    let mut out = String::new();
    for ch in path.chars() {
        if ch != '/' || !out.ends_with('/') || out == "/" {
            out.push(ch);
        }
    }
    while out.len() > 1 && out.ends_with('/') {
        out.pop();
    }
    out
}

fn normalize(path: &str) -> String {
    if path.is_empty() { return String::new(); }
    let absolute = path.starts_with('/');
    let mut parts = Vec::new();
    let mut directory = false;
    for part in path.split('/') {
        directory = true;
        match part {
            "" | "." => {}
            ".." if parts.last().is_some_and(|&p| p != "..") => { parts.pop(); }
            ".." if absolute => {}
            _ => { parts.push(part); directory = false; }
        }
    }
    let mut result = if absolute { "/".to_owned() } else { String::new() };
    result.push_str(&parts.join("/"));
    if result.is_empty() { return ".".into(); }
    if directory && parts.last().is_some_and(|&part| part != "..") { result.push('/'); }
    result
}

fn destination(interp: &mut Interp, text: &str, directory: bool) -> String {
    let normalized = normalize(text);
    let value = match interp.policy_status("CMP0177") {
        PolicyStatus::New if !directory && text.contains("$<") =>
            format!("$<PATH:CMAKE_PATH,NORMALIZE,{text}>"),
        PolicyStatus::New => normalized,
        status => {
            if status == PolicyStatus::Warn && !text.contains("$<") && normalized != text {
                interp.issue(MessageType::AuthorWarning, &policy_warning(index_of("CMP0177").unwrap()));
            }
            text.to_owned()
        }
    };
    if directory { value } else { unix_slashes(&value) }
}

fn options(interp: &mut Interp, args: &Arguments<'_>, generic: &InstallOptions) -> InstallOptions {
    let dest = destination(interp, args.get("DESTINATION"), false);
    let prefer = |key, fallback: &String| nonempty(args.get(key), fallback).to_owned();
    let prefer_list = |key, fallback: &Vec<String>| {
        let list = args.list(key);
        if list.is_empty() { fallback.clone() } else { list.to_vec() }
    };
    InstallOptions {
        destination: nonempty(&dest, &generic.destination).to_owned(),
        component: prefer("COMPONENT", &generic.component),
        namelink_component: args.get("NAMELINK_COMPONENT").to_owned(),
        configurations: prefer_list("CONFIGURATIONS", &generic.configurations),
        permissions: prefer_list("PERMISSIONS", &generic.permissions),
        rename: prefer("RENAME", &generic.rename),
        optional: args.has("OPTIONAL") || generic.optional,
        exclude: args.has("EXCLUDE_FROM_ALL") || generic.exclude,
        namelink_only: args.has("NAMELINK_ONLY") || generic.namelink_only,
        namelink_skip: args.has("NAMELINK_SKIP") || generic.namelink_skip,
        message: generic.message.clone(),
    }
}

fn defaults(interp: &Interp) -> InstallOptions {
    InstallOptions { component: default_component(interp), message: message_level(interp), ..Default::default() }
}

fn validate_permissions(opts: &InstallOptions) -> CmdResult {
    if opts.permissions.iter().all(|p| valid_permission(p)) { Ok(()) }
    else { err("called with incorrect arguments.") }
}

fn typed_destination(interp: &Interp, kind: &str) -> Option<String> {
    let (suffix, fallback) = match kind {
        "BIN" => ("BINDIR", "bin"),
        "SBIN" => ("SBINDIR", "sbin"),
        "LIB" => ("LIBDIR", "lib"),
        "INCLUDE" => ("INCLUDEDIR", "include"),
        "SYSCONF" => ("SYSCONFDIR", "etc"),
        "SHAREDSTATE" => ("SHAREDSTATEDIR", "com"),
        "LOCALSTATE" => ("LOCALSTATEDIR", "var"),
        "RUNSTATE" => ("RUNSTATEDIR", "run"),
        "DATA" => ("DATADIR", ""),
        "INFO" => ("INFODIR", "info"),
        "LOCALE" => ("LOCALEDIR", "locale"),
        "MAN" => ("MANDIR", "man"),
        "DOC" => ("DOCDIR", "doc"),
        "LIBEXEC" => ("LIBEXECDIR", "libexec"),
        _ => return None,
    };
    let value = interp.get_safe(&format!("CMAKE_INSTALL_{suffix}"));
    if !value.is_empty() { return Some(value); }
    let root = interp.get_safe("CMAKE_INSTALL_DATAROOTDIR");
    let data_root = nonempty(&root, "share");
    Some(match kind {
        "RUNSTATE" => format!("{}/run", typed_destination(interp, "LOCALSTATE").unwrap()),
        "DATA" => data_root.to_owned(),
        "INFO" | "LOCALE" | "MAN" | "DOC" => format!("{data_root}/{fallback}"),
        _ => fallback.to_owned(),
    })
}

fn with_destination(interp: &Interp, mut opts: InstallOptions, kind: &str) -> InstallOptions {
    if opts.destination.is_empty() {
        opts.destination = typed_destination(interp, kind).unwrap_or_default();
    }
    opts
}

fn add(interp: &mut Interp, options: InstallOptions, action: InstallAction) {
    let index = interp.mf_index();
    if let InstallAction::Target { name, imported: false, .. } | InstallAction::Bmi(name) = &action
        && let Some(key) = crate::state::find_target_key(interp, name)
        && let Some(target) = interp.ext.targets.get_mut(&key)
    {
        target.have_install_rule = true;
    }
    let preceding_children = interp.mf().children.len();
    let backtrace = interp.backtrace.clone();
    interp.ext.misc.insert("install-target-enabled".into(), "1".into());
    // cmGlobalGenerator::AddInstallComponent: a sorted set of names.
    if !options.component.is_empty() {
        let components = interp.ext.misc.entry("install-components".into()).or_default();
        let mut set: std::collections::BTreeSet<String> =
            crate::util::expand_list(components, false).into_iter().collect();
        set.insert(options.component.clone());
        *components = set.into_iter().collect::<Vec<_>>().join(";");
    }
    interp.ext.install.rules.entry(index).or_default().push(InstallRule {
        options, action, preceding_children, backtrace,
    });
}

fn install(interp: &mut Interp, args: &[String]) -> CmdResult {
    if interp.mode == Mode::Script { return err("command is not scriptable"); }
    let Some(mode) = args.first().map(String::as_str) else { return Ok(()); };
    interp.ext.install.enabled = true;
    match mode {
        "FILES" | "PROGRAMS" => files(interp, args),
        "DIRECTORY" => directory(interp, &args[1..]),
        "SCRIPT" | "CODE" => script(interp, args),
        "TARGETS" | "IMPORTED_RUNTIME_ARTIFACTS" => targets(interp, args),
        "EXPORT" => export(interp, args),
        "PACKAGE_INFO" => package_info(interp, args),
        "RUNTIME_DEPENDENCY_SET" => runtime_set(interp, args),
        "EXPORT_ANDROID_MK" => err("EXPORT_ANDROID_MK is not supported by cmake-rust"),
        _ => err(format!("does not recognize sub-command {mode}")),
    }
}

fn source_files(interp: &Interp, mode: &str, files: &[String]) -> Result<Vec<String>, CmdErr> {
    files.iter().map(|file| {
        let path = if file.starts_with("$<") || is_full_path(file) {
            file.clone()
        } else {
            format!("{}/{file}", interp.current_source_dir())
        };
        let fs_path = Path::new(&path);
        if !file.contains("$<") && !fs_path.is_symlink() && fs_path.is_dir() {
            return Err(CmdErr::Error(format!("{mode} given directory \"{file}\" to install.")));
        }
        Ok(path)
    }).collect()
}

fn copy(kind: CopyKind, paths: Vec<String>) -> InstallAction {
    InstallAction::Copy { kind, paths, dir_permissions: Vec::new(), arguments: String::new() }
}

fn files(interp: &mut Interp, args: &[String]) -> CmdResult {
    let mode = args[0].as_str();
    let parsed = parse_options(args, &[], &[mode], &[]);
    if let Some(arg) = parsed.unknown.first() { return Err(unknown(mode, arg)); }
    let kind = parsed.get("TYPE");
    if !kind.is_empty() && typed_destination(interp, kind).is_none() {
        return err(format!("{mode} given non-type \"{kind}\" with TYPE argument."));
    }
    let paths = parsed.list(mode);
    if paths.is_empty() { return Ok(()); }
    if !parsed.get("RENAME").is_empty() && paths.len() > 1 {
        return err(format!("{mode} given RENAME option with more than one file."));
    }
    let abs = source_files(interp, mode, paths)?;
    for path in paths {
        if interp.ext.install.build_exports.contains_key(path) {
            interp.issue(MessageType::FatalError, &format!(
                "The file\n  {path}\nwas generated by the export() command.  It may not be installed with the install() command.  Use the install(EXPORT) mechanism instead.  See the cmake-packages(7) manual for more."
            ));
            return Err(CmdErr::Reported);
        }
    }
    let opts = options(interp, &parsed, &defaults(interp));
    validate_permissions(&opts)?;
    if !kind.is_empty() && !opts.destination.is_empty() {
        return err(format!("{mode} given both TYPE and DESTINATION arguments. You may only specify one."));
    }
    let opts = with_destination(interp, opts, kind);
    if opts.destination.is_empty() { return err(format!("{mode} given no DESTINATION!")); }
    let kind = if mode == "PROGRAMS" { CopyKind::Program } else { CopyKind::File };
    add(interp, opts, copy(kind, abs));
    Ok(())
}

fn script(interp: &mut Interp, args: &[String]) -> CmdResult {
    let mut opts = defaults(interp);
    opts.message.clear();
    let mut all_components = false;
    let mut count = 0;
    let mut iter = args.iter().map(String::as_str).peekable();
    while let Some(mut arg) = iter.next() {
        if arg == "COMPONENT" && let Some(component) = iter.next() {
            arg = component;
            count += 1;
            opts.component = component.into();
        }
        opts.exclude |= arg == "EXCLUDE_FROM_ALL";
        all_components |= arg == "ALL_COMPONENTS";
    }
    if count > 1 {
        return err("given more than one COMPONENT for the SCRIPT or CODE signature of the INSTALL command. Use multiple INSTALL commands with one COMPONENT each.");
    }
    if all_components && count == 1 { return err("ALL_COMPONENTS and COMPONENT are mutually exclusive"); }
    let mut active = "";
    for arg in args {
        match arg.as_str() {
            "SCRIPT" => active = "SCRIPT",
            "CODE" => active = "CODE",
            "COMPONENT" => active = "",
            _ if !active.is_empty() => {
                let is_code = active == "CODE";
                let mut text = arg.clone();
                if !is_code && !text.starts_with("$<INSTALL_PREFIX>") {
                    if !is_full_path(&text) { text = format!("{}/{text}", interp.current_source_dir()); }
                    if Path::new(&text).is_dir() { return err("given a directory as value of SCRIPT argument."); }
                }
                add(interp, opts.clone(), InstallAction::Script { text, is_code, all_components });
                active = "";
            }
            _ => {}
        }
    }
    if !active.is_empty() { return err(format!("given no value for {active} argument.")); }
    Ok(())
}

fn directory(interp: &mut Interp, args: &[String]) -> CmdResult {
    let mut opts = defaults(interp);
    let mut dirs = Vec::new();
    let mut dir_permissions = Vec::new();
    let mut literals = String::new();
    let mut active = "DIRECTORY";
    let mut matches = false;
    let mut dest = None;
    let mut kind = "";
    for arg in args {
        match arg.as_str() {
            "PATTERN" | "REGEX" => { matches = true; active = arg; }
            "EXCLUDE" | "PERMISSIONS" => {
                if !matches || (arg == "EXCLUDE" && matches!(active, "PATTERN" | "REGEX")) {
                    return err(format!("DIRECTORY does not allow \"{arg}\" before a PATTERN or REGEX is given."));
                }
                literals.push(' ');
                literals.push_str(arg);
                active = if arg == "PERMISSIONS" { arg } else { "" };
            }
            "DESTINATION" | "TYPE" | "OPTIONAL" | "MESSAGE_NEVER" | "FILE_PERMISSIONS"
            | "DIRECTORY_PERMISSIONS" | "USE_SOURCE_PERMISSIONS" | "FILES_MATCHING"
            | "CONFIGURATIONS" | "COMPONENT" | "EXCLUDE_FROM_ALL" => {
                if matches { return err(format!("DIRECTORY does not allow \"{arg}\" after PATTERN or REGEX.")); }
                active = "";
                match arg.as_str() {
                    "OPTIONAL" => opts.optional = true,
                    "EXCLUDE_FROM_ALL" => opts.exclude = true,
                    "MESSAGE_NEVER" => opts.message = "MESSAGE_NEVER".into(),
                    "USE_SOURCE_PERMISSIONS" | "FILES_MATCHING" => {
                        literals.push(' ');
                        literals.push_str(arg);
                    }
                    _ => active = arg,
                }
            }
            _ => match active {
                "DIRECTORY" => {
                    let path = if arg.starts_with("$<") || is_full_path(arg) { arg.clone() }
                        else { format!("{}/{arg}", interp.current_source_dir()) };
                    if Path::new(&path).is_file() {
                        return err(format!("DIRECTORY given non-directory \"{arg}\" to install."));
                    }
                    dirs.push(path);
                }
                "DESTINATION" => { dest = Some(destination(interp, arg, true)); active = ""; }
                "TYPE" => {
                    if typed_destination(interp, arg).is_none() {
                        return err(format!("DIRECTORY given non-type \"{arg}\" with TYPE argument."));
                    }
                    kind = arg;
                    active = "";
                }
                "CONFIGURATIONS" => opts.configurations.push(arg.clone()),
                "COMPONENT" => { opts.component.clone_from(arg); active = ""; }
                "PATTERN" | "REGEX" => {
                    let regex = if active == "PATTERN" { format!("/{}$", pattern_regex(arg)) } else { arg.clone() };
                    literals.push_str(&format!(" REGEX \"{}\"", regex.replace('\\', "\\\\")));
                    active = "";
                }
                "FILE_PERMISSIONS" | "DIRECTORY_PERMISSIONS" | "PERMISSIONS" => {
                    let label = match active {
                        "FILE_PERMISSIONS" => "file ",
                        "DIRECTORY_PERMISSIONS" => "directory ",
                        _ => "",
                    };
                    if !valid_permission(arg) { return err(format!("DIRECTORY given invalid {label}permission \"{arg}\".")); }
                    match active {
                        "FILE_PERMISSIONS" => opts.permissions.push(arg.clone()),
                        "DIRECTORY_PERMISSIONS" => dir_permissions.push(arg.clone()),
                        _ => { literals.push(' '); literals.push_str(arg); }
                    }
                }
                _ => return Err(unknown("DIRECTORY", arg)),
            },
        }
    }
    if dirs.is_empty() && dest.is_some() { dirs.push(String::new()); }
    if dirs.is_empty() { return Ok(()); }
    opts.destination = match (dest, typed_destination(interp, kind)) {
        (Some(_), Some(_)) => return err("DIRECTORY given both TYPE and DESTINATION arguments. You may only specify one."),
        (Some(dest), None) | (None, Some(dest)) => dest,
        (None, None) => return err("DIRECTORY given no DESTINATION!"),
    };
    add(interp, opts, InstallAction::Copy { kind: CopyKind::Directory, paths: dirs, dir_permissions, arguments: literals });
    Ok(())
}

fn pattern_regex(pattern: &str) -> String {
    let mut regex = String::new();
    let mut chars = pattern.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '*' => regex.push_str("[^/]*"),
            '?' => regex.push_str("[^/]"),
            '[' => {
                let mut bracket = chars.clone();
                let complement = bracket.next_if(|&ch| matches!(ch, '!' | '^')).is_some();
                let leading_close = bracket.next_if_eq(&']').is_some();
                if bracket.any(|ch| ch == ']') {
                    regex.push('[');
                    if complement { chars.next(); regex.push('^'); }
                    if leading_close { chars.next(); regex.push(']'); }
                    for member in chars.by_ref().take_while(|&ch| ch != ']') {
                        if member == '\\' { regex.push('\\'); }
                        regex.push(member);
                    }
                    regex.push(']');
                } else { regex.push_str("\\["); }
            }
            _ => {
                if !ch.is_ascii_alphanumeric() { regex.push('\\'); }
                regex.push(ch);
            }
        }
    }
    regex
}

fn split_groups<'a>(
    args: &[String], group_keys: &[&'a str],
) -> (Vec<String>, Vec<(&'a str, Vec<String>)>) {
    let mut generic = Vec::new();
    let mut groups: Vec<(&str, Vec<String>)> = Vec::new();
    for arg in args {
        if let Some(&key) = group_keys.iter().find(|&&key| key == arg) {
            if key != "FILE_SET" && let Some(i) = groups.iter().position(|(k, _)| *k == key) {
                let previous = groups.remove(i);
                groups.push(previous);
            } else {
                groups.push((key, Vec::new()));
            }
        } else if let Some((_, values)) = groups.last_mut() {
            values.push(arg.clone());
        } else {
            generic.push(arg.clone());
        }
    }
    (generic, groups)
}

fn artifact_options(
    interp: &mut Interp, mode: &str, args: &[String], generic: &InstallOptions,
) -> Result<(InstallOptions, bool), CmdErr> {
    let parsed = parse_options(args, &[], &[], &[]);
    if let Some(arg) = parsed.unknown.first() { return Err(unknown(mode, arg)); }
    let opts = options(interp, &parsed, generic);
    validate_permissions(&opts)?;
    Ok((opts, !parsed.get("TYPE").is_empty()))
}

fn dependency_filters(args: &[String], mode: &str) -> Result<BTreeMap<String, Vec<String>>, CmdErr> {
    let parsed = parse(args, &[], FILTERS, &[]);
    if let Some(arg) = parsed.unknown.first() { return Err(unknown(mode, arg)); }
    Ok(parsed.values.into_iter().map(|(k, v)| (k.to_owned(), v)).collect())
}

fn targets(interp: &mut Interp, args: &[String]) -> CmdResult {
    let mode = args[0].as_str();
    let imported = mode == "IMPORTED_RUNTIME_ARTIFACTS";
    let group_keys = if imported { IMPORT_GROUPS } else { TARGET_GROUPS };
    let (general, groups) = split_groups(args, group_keys);
    let singles = if imported { &["RUNTIME_DEPENDENCY_SET"][..] } else { &["RUNTIME_DEPENDENCY_SET", "EXPORT"][..] };
    let lists = if imported { vec![mode] } else { vec![mode, "RUNTIME_DEPENDENCIES"] };
    let parsed = parse_options(&general, singles, &lists, &[]);
    if let Some(arg) = parsed.unknown.first() { return Err(unknown(mode, arg)); }
    let defaults = defaults(interp);
    let generic = options(interp, &parsed, &defaults);
    validate_permissions(&generic)?;
    let mut by_kind = BTreeMap::new();
    let mut sets = Vec::new();
    let mut includes = Vec::new();
    let mut any_type = !parsed.get("TYPE").is_empty();
    for (group, group_args) in &groups {
        if *group == "INCLUDES" {
            includes.extend(group_args.iter().skip(1).map(|p| unix_slashes(p)));
        } else if *group == "FILE_SET" {
            let Some((name, values)) = group_args.split_first() else {
                return err("TARGETS given FILE_SET option without file set name.");
            };
            if SINGLES.contains(&name.as_str()) || LISTS.contains(&name.as_str()) || FLAGS.contains(&name.as_str()) {
                return err("TARGETS given FILE_SET option without file set name.");
            }
            let (opts, uses_type) = artifact_options(interp, mode, values, &generic)?;
            sets.push((name.clone(), opts));
            any_type |= uses_type;
        } else {
            let (opts, uses_type) = artifact_options(interp, mode, group_args, &generic)?;
            any_type |= uses_type;
            by_kind.insert(*group, opts);
        }
    }
    let group = |key: &str| by_kind.get(key).unwrap_or(&generic);
    if !imported {
        let restricted = std::iter::once(&generic).chain(by_kind.iter().filter_map(|(key, value)|
            (!matches!(*key, "LIBRARY" | "ARCHIVE")).then_some(value)
        )).chain(sets.iter().map(|(_, opts)| opts));
        for (key, invalid) in [
            ("NAMELINK_ONLY", restricted.clone().any(|opts| opts.namelink_only)),
            ("NAMELINK_SKIP", restricted.clone().any(|opts| opts.namelink_skip)),
            ("NAMELINK_COMPONENT", restricted.clone().any(|opts| !opts.namelink_component.is_empty())),
        ] {
            if invalid {
                return err(format!("TARGETS given {key} option not in LIBRARY or ARCHIVE group.  The {key} option may be specified only following LIBRARY or ARCHIVE."));
            }
        }
        let library = group("LIBRARY");
        if library.namelink_only && library.namelink_skip {
            return err("TARGETS given NAMELINK_ONLY and NAMELINK_SKIP.  At most one of these two options may be specified.");
        }
        if any_type {
            return err("TARGETS given TYPE option. The TYPE option may only be specified in  install(FILES) and install(DIRECTORIES).");
        }
    }
    let implicit = parsed.has("RUNTIME_DEPENDENCIES");
    let explicit = parsed.get("RUNTIME_DEPENDENCY_SET");
    let filters = dependency_filters(parsed.list("RUNTIME_DEPENDENCIES"), mode)?;
    if implicit && !explicit.is_empty() {
        return err("TARGETS cannot have both RUNTIME_DEPENDENCIES and RUNTIME_DEPENDENCY_SET.");
    }
    if implicit || !explicit.is_empty() {
        let system = interp.get_safe("CMAKE_HOST_SYSTEM_NAME");
        let option = if implicit { "RUNTIME_DEPENDENCIES" } else { "RUNTIME_DEPENDENCY_SET" };
        if !matches!(system.as_str(), "Linux" | "Darwin" | "Windows") {
            return err(format!("{mode} {option} is not supported on system \"{system}\""));
        }
        if implicit && interp.is_on("CMAKE_CROSSCOMPILING") {
            return err("TARGETS RUNTIME_DEPENDENCIES is not supported when cross-compiling.");
        }
    }
    let names = parsed.list(mode);
    let mut dependencies = Vec::new();
    for name in names {
        if interp.ext.aliases.contains_key(name) {
            return err(format!("{mode} given target \"{name}\" which is an alias."));
        }
        let Some(target) = find_target_to_use(interp, name).filter(|t| t.imported == imported) else {
            return err(format!("{mode} given target \"{name}\" which does not exist."));
        };
        let kind = target.kind;
        let props = target.props.clone();
        let makefile = target.makefile;
        let allowed = matches!(kind, TargetKind::Executable | TargetKind::SharedLibrary | TargetKind::ModuleLibrary)
            || (!imported && matches!(kind, TargetKind::StaticLibrary | TargetKind::ObjectLibrary | TargetKind::InterfaceLibrary));
        if !allowed {
            return err(format!("{mode} given target \"{name}\" which is not an executable, library, or module."));
        }

        let (key, standard) = match kind {
            TargetKind::Executable => ("RUNTIME", "BIN"),
            TargetKind::StaticLibrary => ("ARCHIVE", "LIB"),
            TargetKind::SharedLibrary => ("LIBRARY", "LIB"),
            TargetKind::ModuleLibrary => ("LIBRARY", if imported { "LIB" } else { "" }),
            TargetKind::ObjectLibrary => ("OBJECTS", ""),
            _ => ("", ""),
        };
        let artifact_args = group(key);
        let mut artifact = with_destination(interp, artifact_args.clone(), standard);
        artifact.rename.clear();
        let specified = !artifact_args.destination.is_empty();
        let namelink_only = !imported && matches!(kind, TargetKind::SharedLibrary | TargetKind::ModuleLibrary)
            && artifact_args.namelink_only;
        if !imported && kind == TargetKind::ModuleLibrary && !specified {
            return err(format!("TARGETS given no LIBRARY DESTINATION for module target \"{name}\"."));
        }
        let export_set = parsed.get("EXPORT");
        if !export_set.is_empty() {
            for property in ["INTERFACE_HEADER_SETS", "INTERFACE_CXX_MODULE_SETS"] {
                for set in expand_list(props.get(property).unwrap_or(""), false) {
                    if !sets.iter().any(|(name, _)| *name == set) {
                        return err(format!("TARGETS target {name} is exported but not all of its interface file sets are installed"));
                    }
                }
            }
        }
        let mut export_artifact = None;
        let mut export_namelink = None;
        if !artifact.destination.is_empty() {
            if !namelink_only {
                let namelink = if !imported && (kind == TargetKind::SharedLibrary || artifact_args.namelink_skip) {
                    Namelink::Skip
                } else { Namelink::None };
                add(interp, artifact.clone(), InstallAction::Target { name: name.clone(), namelink, imported });
                export_artifact = Some(artifact.clone());
                if matches!(kind, TargetKind::Executable | TargetKind::SharedLibrary | TargetKind::ModuleLibrary) {
                    dependencies.push(name.clone());
                }
            }
            if !imported && !artifact_args.namelink_skip &&
                (kind == TargetKind::SharedLibrary || namelink_only) {
                artifact.component = nonempty(&artifact.namelink_component, &artifact.component).to_owned();
                add(interp, artifact.clone(), InstallAction::Target { name: name.clone(), namelink: Namelink::Only, imported });
                export_namelink = Some(artifact);
            }
        }
        if imported { continue; }
        if !namelink_only {
            for property in ["PRIVATE_HEADER", "PUBLIC_HEADER", "RESOURCE"] {
                let files = expand_list(props.get(property).unwrap_or(""), false);
                if files.is_empty() { continue; }
                let opts = group(property);
                if opts.destination.is_empty() && (specified || property == "RESOURCE") {
                    interp.issue(MessageType::AuthorWarning, &format!(
                        "Target {name} has {property} files but no {property} DESTINATION."
                    ));
                    continue;
                }
                let paths = source_files(interp, mode, &files)?;
                let opts = with_destination(interp, opts.clone(), "INCLUDE");
                add(interp, opts, copy(CopyKind::File, paths));
            }
            for (set, opts) in &sets {
                let opts = with_destination(interp, opts.clone(), "INCLUDE");
                add(interp, opts, InstallAction::FileSet { name: name.clone(), set: set.clone() });
            }
        }
        let mut bmi = group("CXX_MODULES_BMI").clone();
        if !bmi.destination.is_empty() {
            if makefile != interp.mf_index() {
                bmi.message.clear();
            }
            add(interp, bmi, InstallAction::Bmi(name.clone()));
        }
        if !export_set.is_empty() {
            let file_sets = sets.iter().map(|(name, opts)| {
                (name.clone(), with_destination(interp, opts.clone(), "INCLUDE").destination)
            }).collect();
            interp.ext.install.export_sets.entry(export_set.to_owned()).or_default().targets.push(
                ExportTarget { name: name.clone(), artifact: export_artifact, includes: includes.clone(), file_sets, namelink: export_namelink, namelink_only }
            );
        }
    }
    if implicit && !dependencies.is_empty() {
        let set = format!("#{}:{}", interp.mf_index(), interp.ext.install.runtime_sets.len());
        interp.ext.install.runtime_sets.insert(set.clone(), dependencies);
        let opts = with_destination(interp, group("LIBRARY").clone(), "LIB");
        add(interp, opts, InstallAction::RuntimeDependencies { set, filters });
    } else if !explicit.is_empty() {
        interp.ext.install.runtime_sets.entry(explicit.to_owned()).or_default().extend(dependencies);
    }
    Ok(())
}

fn runtime_set(interp: &mut Interp, args: &[String]) -> CmdResult {
    let system = interp.get_safe("CMAKE_HOST_SYSTEM_NAME");
    if !matches!(system.as_str(), "Linux" | "Darwin" | "Windows") {
        return err(format!("RUNTIME_DEPENDENCY_SET is not supported on system \"{system}\""));
    }
    let (general, groups) = split_groups(args, &["LIBRARY", "RUNTIME", "FRAMEWORK"]);
    let parsed = parse_options(&general, &["RUNTIME_DEPENDENCY_SET"], &[], &[]);
    let filters = dependency_filters(&parsed.unknown, "RUNTIME_DEPENDENCY_SET")?;
    let generic = options(interp, &parsed, &defaults(interp));
    validate_permissions(&generic)?;
    let mut library = generic.clone();
    for (key, group) in groups {
        let (opts, _) = artifact_options(interp, "RUNTIME_DEPENDENCY_SET", &group, &generic)?;
        if key == "LIBRARY" { library = opts; }
    }
    let set = parsed.get("RUNTIME_DEPENDENCY_SET");
    if set.is_empty() { return err("RUNTIME_DEPENDENCY_SET not given a runtime dependency set."); }
    let opts = with_destination(interp, library, "LIB");
    add(interp, opts, InstallAction::RuntimeDependencies { set: set.to_owned(), filters });
    Ok(())
}

pub(super) fn package_dependencies_enabled(interp: &Interp) -> bool {
    interp.get_def("CMAKE_EXPERIMENTAL_EXPORT_PACKAGE_DEPENDENCIES")
        == Some("1942b4fa-b2c5-4546-9385-83f254070067")
}

fn export(interp: &mut Interp, args: &[String]) -> CmdResult {
    let experimental = package_dependencies_enabled(interp);
    let flags = if experimental { &["EXPORT_LINK_INTERFACE_LIBRARIES", "EXPORT_PACKAGE_DEPENDENCIES"][..] }
        else { &["EXPORT_LINK_INTERFACE_LIBRARIES"][..] };
    let parsed = parse_options(args, &["EXPORT", "NAMESPACE", "FILE", "CXX_MODULES_DIRECTORY"], &[], flags);
    if let Some(arg) = parsed.unknown.first() { return Err(unknown("EXPORT", arg)); }
    let opts = options(interp, &parsed, &defaults(interp));
    validate_permissions(&opts)?;
    if opts.destination.is_empty() { return err("EXPORT given no DESTINATION!"); }
    let set = parsed.get("EXPORT");
    let file = parsed.get("FILE");
    let invalid_path = |path: &str| path.contains([':', '/', '\\']);
    if invalid_path(file) {
        return err(format!("EXPORT given invalid export file name \"{file}\".  The FILE argument may not contain a path.  Specify the path in the DESTINATION argument."));
    }
    if !file.is_empty() && !file.ends_with(".cmake") {
        return err(format!("EXPORT given invalid export file name \"{file}\".  The FILE argument must specify a name ending in \".cmake\"."));
    }
    if file.is_empty() && invalid_path(set) {
        return err(format!("EXPORT given export name \"{set}\".  This name cannot be safely converted to a file name.  Specify a different export name or use the FILE option to set a file name explicitly."));
    }
    let filename = if file.is_empty() { format!("{set}.cmake") } else { file.to_owned() };
    let options = ExportOptions {
        namespace: parsed.get("NAMESPACE").into(),
        modules_directory: parsed.get("CXX_MODULES_DIRECTORY").into(),
        old_link_interface: parsed.has("EXPORT_LINK_INTERFACE_LIBRARIES"),
        package_dependencies: parsed.has("EXPORT_PACKAGE_DEPENDENCIES"),
    };
    interp.ext.install.export_sets.entry(set.to_owned()).or_default();
    add(interp, opts, InstallAction::Export(InstallExport { set: set.to_owned(), filename, options }));
    Ok(())
}

/// `HandlePackageInfoMode`.
fn package_info(interp: &mut Interp, args: &[String]) -> CmdResult {
    if !super::project::export_package_info_enabled(interp) {
        return err("does not recognize sub-command PACKAGE_INFO");
    }
    let parsed = parse_options(
        args,
        &[crate::package_info::SINGLE_KEYS.as_slice(), &["EXPORT"]].concat(),
        &crate::package_info::LIST_KEYS,
        &crate::package_info::FLAG_KEYS,
    );
    if let Some(arg) = parsed.unknown.first() { return Err(unknown("PACKAGE_INFO", arg)); }
    let mut opts = options(interp, &parsed, &defaults(interp));
    validate_permissions(&opts)?;
    let set = parsed.get("EXPORT");
    if set.is_empty() { return err("PACKAGE_INFO missing EXPORT."); }
    let mut arguments = crate::package_info::PackageInfoArguments::from_args(&parsed);
    arguments.check(true)?;
    arguments.set_metadata_from_project(interp)?;
    // Get or construct the destination path.
    if opts.destination.is_empty() {
        opts.destination = if interp.get_safe("CMAKE_SYSTEM_NAME") == "Windows" {
            "cps".to_owned()
        } else {
            format!(
                "{}/cps/{}",
                typed_destination(interp, "LIB").unwrap_or_default(),
                arguments.package_dir_name()
            )
        };
    }
    interp.ext.install.export_sets.entry(set.to_owned()).or_default();
    add(interp, opts, InstallAction::PackageInfo { set: set.to_owned(), arguments });
    Ok(())
}

fn legacy_files(interp: &mut Interp, args: &[String]) -> CmdResult { legacy_copy(interp, args, false) }
fn legacy_programs(interp: &mut Interp, args: &[String]) -> CmdResult { legacy_copy(interp, args, true) }

pub(crate) fn legacy_destination(value: &str) -> String {
    let value = value.chars().skip(1).collect::<String>();
    nonempty(&unix_slashes(&value), ".").to_owned()
}

pub(crate) fn legacy_source(source: &str, binary: &str, name: &str) -> String {
    if is_full_path(name) || name.starts_with("$<") { return name.to_owned(); }
    let from_build = format!("{binary}/{name}");
    let from_source = format!("{source}/{name}");
    if Path::new(&from_build).exists() || !Path::new(&from_source).exists() { from_build } else { from_source }
}

fn legacy_copy(interp: &mut Interp, args: &[String], programs: bool) -> CmdResult {
    if args.len() < 2 { return err("called with incorrect number of arguments"); }
    interp.ext.install.enabled = true;
    let opts = InstallOptions {
        destination: legacy_destination(&args[0]),
        component: interp.get_def("CMAKE_INSTALL_DEFAULT_COMPONENT_NAME").unwrap_or("Unspecified").to_owned(),
        message: message_level(interp),
        ..Default::default()
    };
    let action = if args[1] == "FILES" && !programs {
        copy(CopyKind::File, args[2..].iter().map(|arg| {
            legacy_source(&interp.current_source_dir(), &interp.current_binary_dir(), arg)
        }).collect())
    } else {
        InstallAction::LegacyFiles { args: args[1..].to_vec(), programs }
    };
    add(interp, opts, action);
    Ok(())
}

fn legacy_targets(interp: &mut Interp, args: &[String]) -> CmdResult {
    if args.len() < 2 { return err("called with incorrect number of arguments"); }
    interp.ext.install.enabled = true;
    let mut iter = args[1..].iter();
    while let Some(name) = iter.next() {
        if name == "RUNTIME_DIRECTORY" {
            if iter.next().is_none() { return err("called with RUNTIME_DIRECTORY but no actual directory"); }
            continue;
        }
        if !interp.ext.targets.get(name).is_some_and(|t| t.makefile == interp.mf_index() && !t.imported) {
            return err(format!("Cannot find target: \"{name}\" to install."));
        }
        let opts = InstallOptions {
            component: interp.get_def("CMAKE_INSTALL_DEFAULT_COMPONENT_NAME").unwrap_or("Unspecified").to_owned(),
            destination: legacy_destination(&args[0]),
            message: message_level(interp),
            ..Default::default()
        };
        add(interp, opts, InstallAction::LegacyTarget(name.clone()));
    }
    Ok(())
}
