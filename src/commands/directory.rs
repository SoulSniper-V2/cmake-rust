//! Directory-scoped build commands and subdirectory configuration.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::sync::LazyLock;

use crate::cache::convert_to_unix_slashes;
use crate::interp::{Builtin, CmdResult, Interp, Mode, err};
use crate::state::{PendingSubdirectory, SourceGroup};
use crate::util::{collapse_full_path, expand_list, is_off, is_on, is_subdirectory};

use super::target::{
    DIRECTORY_USAGE_PROPERTIES, append_directory, directory_entries, full_path, genex_start, report,
};

pub fn register(interp: &mut Interp) {
    let commands: &[(&str, Builtin)] = &[
        ("add_subdirectory", add_subdirectory),
        ("subdirs", subdirs),
        ("include_directories", include_directories),
        ("add_definitions", add_definitions),
        ("remove_definitions", remove_definitions),
        ("add_compile_definitions", add_compile_definitions),
        ("add_compile_options", add_compile_options),
        ("add_link_options", add_link_options),
        ("link_directories", link_directories),
        ("link_libraries", link_libraries),
        ("source_group", source_group),
    ];
    // cmake::RoleProject: project and --find-package modes.
    if matches!(interp.mode, Mode::Project | Mode::FindPackage) {
        for &(name, command) in commands {
            interp.register(name, command);
        }
    }
}

pub fn initialize_subdirectory(interp: &mut Interp, parent: usize, child: usize, system: bool) {
    for prop in DIRECTORY_USAGE_PROPERTIES {
        let entries = directory_entries(interp, parent, prop);
        if !entries.is_empty() {
            interp
                .ext
                .directory_provenance
                .entry(child)
                .or_default()
                .insert((*prop).to_owned(), entries);
        }
    }
    for prop in [
        "IMPLICIT_DEPENDS_INCLUDE_TRANSFORM",
        "LABELS",
        "LINK_LIBRARIES",
    ] {
        if let Some(value) = interp.makefiles[parent].props.get(prop).map(str::to_owned) {
            interp.makefiles[child].props.set(prop, value);
        }
    }
    if system {
        interp.makefiles[child]
            .props
            .set("SYSTEM", "TRUE".to_owned());
    }
    let inherited_targets: Vec<_> = interp
        .ext
        .scoped_targets
        .iter()
        .filter(|((dir, _), key)| *dir == parent && interp.ext.targets[*key].imported)
        .map(|((_, name), key)| ((child, name.clone()), key.clone()))
        .collect();
    interp.ext.scoped_targets.extend(inherited_targets);
    let inherited_aliases: Vec<_> = interp
        .ext
        .scoped_aliases
        .iter()
        .filter(|((dir, _), _)| *dir == parent)
        .map(|((_, name), key)| ((child, name.clone()), key.clone()))
        .collect();
    interp.ext.scoped_aliases.extend(inherited_aliases);
    if let Some(includes) = interp.ext.directory_system_includes.get(&parent).cloned() {
        interp.ext.directory_system_includes.insert(child, includes);
    }
    if let Some(flags) = interp.ext.directory_define_flags.get(&parent).cloned() {
        interp.ext.directory_define_flags.insert(child, flags);
    }
}

fn can_add_subdirectory(interp: &mut Interp, source: &str, binary: &str) -> CmdResult {
    if interp.ext.defer_current_id.is_some() {
        return report(
            interp,
            "Subdirectories may not be created during deferred execution.",
        );
    }
    if interp.makefiles.iter().any(|mf| mf.binary_dir == binary)
        || interp
            .ext
            .pending_subdirs
            .values()
            .flatten()
            .any(|pending| pending.binary == binary)
    {
        return report(
            interp,
            &format!(
                "The binary directory\n  {binary}\nis already used to build a source directory.  It cannot be used to build source directory\n  {source}\nSpecify a unique binary directory name."
            ),
        );
    }
    Ok(())
}

fn configure_subdirectory(
    interp: &mut Interp,
    source: &str,
    binary: &str,
    exclude: bool,
    system: bool,
) -> CmdResult {
    // cmMakefile::AddSubDirectory registers the child makefile before the
    // CMakeLists.txt existence check, so failed directories still count
    // toward CMAKE_NUMBER_OF_MAKEFILES.
    let index = interp.create_subdirectory(source, binary, exclude);
    if !Path::new(&interp.project_list_file(source)).is_file() {
        return report(
            interp,
            &format!("The source directory\n  {source}\ndoes not contain a CMakeLists.txt file."),
        );
    }
    // configure_subdirectory_dirscope additionally gives the child's
    // CMakeLists.txt the CMAKE_PARENT_LIST_FILE=self state of
    // cmMakefile::Configure.
    interp.configure_subdirectory_dirscope(index, system, &[]);
    Ok(())
}

fn add_subdirectory(interp: &mut Interp, args: &[String]) -> CmdResult {
    let Some((source, rest)) = args.split_first() else {
        return err("called with incorrect number of arguments");
    };
    let (mut binary, mut exclude, mut system) = ("", false, false);
    for arg in rest {
        match arg.as_str() {
            "EXCLUDE_FROM_ALL" => exclude = true,
            "SYSTEM" => system = true,
            arg if binary.is_empty() => binary = arg,
            _ => return err("called with incorrect number of arguments"),
        }
    }
    let current_source = interp.current_source_dir();
    let current_binary = interp.current_binary_dir();
    let provided = if full_path(source) {
        source.clone()
    } else {
        format!("{current_source}/{source}")
    };
    let source_path = collapse_full_path(&provided, &interp.home_binary);
    if !(Path::new(&provided).is_dir()
        || source.starts_with('~') && Path::new(&source_path).is_dir())
    {
        return err(format!(
            "given source \"{source}\" which is not an existing directory."
        ));
    }
    let binary = if binary.is_empty() {
        if !is_subdirectory(&source_path, &current_source) {
            return err(format!(
                "not given a binary directory but the given source directory \"{source_path}\" is not a subdirectory of \"{current_source}\".  When specifying an out-of-tree source a binary directory must be explicitly specified."
            ));
        }
        let relative =
            source_path[current_source.trim_end_matches('/').len()..].trim_start_matches('/');
        collapse_full_path(relative, &current_binary)
    } else {
        collapse_full_path(binary, &current_binary)
    };
    system |= interp.mf().props.get("SYSTEM").is_some_and(is_on);
    can_add_subdirectory(interp, &source_path, &binary)?;
    configure_subdirectory(interp, &source_path, &binary, exclude, system)
}

fn subdirs(interp: &mut Interp, args: &[String]) -> CmdResult {
    if args.is_empty() {
        return err("called with incorrect number of arguments");
    }
    let mut exclude = false;
    let mut missing = None;
    let dir = interp.mf_index();
    for arg in args {
        match arg.as_str() {
            "EXCLUDE_FROM_ALL" => exclude = true,
            "PREORDER" => {}
            _ => {
                let candidate = format!("{}/{arg}", interp.current_source_dir());
                let (source, suffix) = if Path::new(&candidate).is_dir() {
                    (candidate, arg.clone())
                } else if Path::new(arg).is_dir() {
                    (arg.clone(), crate::util::filename_name(arg))
                } else {
                    missing = Some(arg);
                    continue;
                };
                let source = collapse_full_path(&source, "");
                let binary =
                    collapse_full_path(&format!("{}/{suffix}", interp.current_binary_dir()), "");
                if can_add_subdirectory(interp, &source, &binary).is_ok() {
                    let index = interp.create_subdirectory(&source, &binary, exclude);
                    let inner_scopes = interp.capture_inner_scopes();
                    interp
                        .ext
                        .pending_subdirs
                        .entry(dir)
                        .or_default()
                        .push(PendingSubdirectory {
                            index,
                            source,
                            binary,
                            exclude_from_all: exclude,
                            backtrace: interp.backtrace.clone(),
                            inner_scopes,
                        });
                }
            }
        }
    }
    if let Some(arg) = missing {
        return err(format!(
            "Incorrect SUBDIRS command. Directory: {arg} does not exist."
        ));
    }
    Ok(())
}

pub fn finish_directory(interp: &mut Interp) {
    let current = interp.mf_index();
    if interp.is_on("CMAKE_TESTING_ENABLED") {
        interp.ext.testing_enabled.insert(current);
    } else {
        interp.ext.testing_enabled.remove(&current);
    }
    let dirs = interp
        .ext
        .pending_subdirs
        .remove(&interp.mf_index())
        .unwrap_or_default();
    for dir in dirs {
        // cmMakefile::Configure runs deferred subdirectories after the
        // parent's list file; diagnostics use the parent's file-scope
        // backtrace, which is the interpreter's current backtrace here.
        if Path::new(&interp.project_list_file(&dir.source)).is_file() {
            interp.configure_subdirectory_dirscope(dir.index, false, &dir.inner_scopes);
        } else {
            let _: CmdResult = report(
                interp,
                &format!(
                    "The source directory\n  {}\ndoes not contain a CMakeLists.txt file.",
                    dir.source
                ),
            );
        }
    }
    add_cmake_depend_files_from_user(interp);
}

/// `cmMakefile::AddCMakeDependFilesFromUser`: the CMAKE_CONFIGURE_DEPENDS
/// directory property, relative entries taken from the source directory.
fn add_cmake_depend_files_from_user(interp: &mut Interp) {
    let Some(deps) = interp
        .mf()
        .props
        .get("CMAKE_CONFIGURE_DEPENDS")
        .map(str::to_owned)
    else {
        return;
    };
    let source = interp.mf().source_dir.clone();
    for dep in crate::util::expand_list(&deps, false) {
        let file = if full_path(&dep) {
            dep
        } else {
            format!("{source}/{dep}")
        };
        interp.mf_mut().list_files.push(file);
    }
}

pub fn input_path(base: &str, path: &str) -> String {
    if full_path(path) || genex_start(path) {
        path.to_owned()
    } else {
        format!("{base}/{path}")
    }
}

fn include_directories(interp: &mut Interp, args: &[String]) -> CmdResult {
    let mut before = interp.is_on("CMAKE_INCLUDE_DIRECTORIES_BEFORE");
    let args = match args.first().map(String::as_str) {
        Some("BEFORE" | "AFTER") => {
            before = args[0] == "BEFORE";
            &args[1..]
        }
        _ => args,
    };
    let mut includes = Vec::new();
    let mut system_includes = BTreeSet::new();
    let mut system = false;
    let base = interp.current_source_dir();
    for arg in args {
        if arg == "SYSTEM" {
            system = true;
            continue;
        }
        if arg.is_empty() {
            return err("given empty-string as include directory.");
        }
        let mut start = 0;
        let mut add = |text: &str| {
            let text = text.trim_matches([' ', '\r']);
            if !text.is_empty() {
                let path = if is_off(text) {
                    text.to_owned()
                } else {
                    input_path(&base, &convert_to_unix_slashes(text))
                };
                if system {
                    system_includes.insert(path.clone());
                }
                includes.push(path);
            }
        };
        // cmIncludeDirectoryCommand passes the newline's offset as substr length.
        for (end, _) in arg.match_indices('\n') {
            if end != 0 {
                let limit = (start + end).min(arg.len());
                let limit = (start..=limit)
                    .rev()
                    .find(|i| arg.is_char_boundary(*i))
                    .unwrap();
                add(&arg[start..limit]);
            }
            start = end + 1;
        }
        add(&arg[start..]);
    }
    if includes.is_empty() {
        return Ok(());
    }
    if before {
        includes.reverse();
    }
    add_include_directories(interp, &includes.join(";"), before);
    let dir = interp.mf_index();
    interp
        .ext
        .directory_system_includes
        .entry(dir)
        .or_default()
        .extend(system_includes.iter().cloned());
    for key in interp.mf().targets.clone() {
        let target = interp.ext.targets.get_mut(&key).unwrap();
        target
            .system_include_directories
            .extend(system_includes.iter().cloned());
    }
    Ok(())
}

/// `cmMakefile::AddIncludeDirectories`.
pub(super) fn add_include_directories(interp: &mut Interp, value: &str, before: bool) {
    append_directory(interp, "INCLUDE_DIRECTORIES", value, before);
    for key in interp.mf().targets.clone() {
        let target = interp.ext.targets.get_mut(&key).unwrap();
        target.insert_entry("INCLUDE_DIRECTORIES", value, &interp.backtrace, before);
    }
}

fn add_definitions(interp: &mut Interp, args: &[String]) -> CmdResult {
    define_flags(interp, args, false);
    Ok(())
}

fn remove_definitions(interp: &mut Interp, args: &[String]) -> CmdResult {
    define_flags(interp, args, true);
    Ok(())
}

fn define_flags(interp: &mut Interp, args: &[String], remove: bool) {
    static DEFINITION: LazyLock<regex::Regex> =
        LazyLock::new(|| regex::Regex::new(r"(?s)^[-/]D[A-Za-z_][A-Za-z0-9_]*(=.*)?$").unwrap());
    let dir = interp.mf_index();
    for flag in args.iter().filter(|flag| !flag.is_empty()) {
        if DEFINITION.is_match(flag) {
            let definition = &flag[2..];
            if !remove {
                append_directory(interp, "COMPILE_DEFINITIONS", definition, false);
            } else if interp.mf().props.contains("COMPILE_DEFINITIONS") {
                let mut entries = directory_entries(interp, dir, "COMPILE_DEFINITIONS");
                for (value, _) in &mut entries {
                    *value = expand_list(value, false)
                        .into_iter()
                        .filter(|value| value != definition)
                        .collect::<Vec<_>>()
                        .join(";");
                }
                entries.retain(|(value, _)| !value.is_empty());
                let value = entries
                    .iter()
                    .map(|(value, _)| value.as_str())
                    .collect::<Vec<_>>()
                    .join(";");
                interp.mf_mut().props.set("COMPILE_DEFINITIONS", value);
                interp
                    .ext
                    .directory_provenance
                    .entry(dir)
                    .or_default()
                    .insert("COMPILE_DEFINITIONS".to_owned(), entries);
            }
        } else {
            // cmMakefile initializes DefineFlags to " ".
            let flags = interp
                .ext
                .directory_define_flags
                .entry(dir)
                .or_insert_with(|| " ".to_owned());
            if !remove {
                flags.push(' ');
                flags.push_str(&flag.replace(['\n', '\r'], " "));
            } else {
                let bytes = flags.as_bytes();
                let matches: Vec<_> = flags
                    .match_indices(flag)
                    .filter_map(|(start, matched)| {
                        let end = start + matched.len();
                        ((start == 0 || bytes[start - 1].is_ascii_whitespace())
                            && (end == bytes.len() || bytes[end].is_ascii_whitespace()))
                        .then_some(start..end)
                    })
                    .collect();
                for range in matches.into_iter().rev() {
                    flags.replace_range(range, "");
                }
            }
        }
    }
}

fn add_compile_definitions(interp: &mut Interp, args: &[String]) -> CmdResult {
    for arg in args {
        append_directory(interp, "COMPILE_DEFINITIONS", arg, false);
    }
    Ok(())
}

fn add_compile_options(interp: &mut Interp, args: &[String]) -> CmdResult {
    for arg in args {
        append_directory(interp, "COMPILE_OPTIONS", arg, false);
    }
    Ok(())
}

fn add_link_options(interp: &mut Interp, args: &[String]) -> CmdResult {
    for arg in args {
        append_directory(interp, "LINK_OPTIONS", arg, false);
    }
    Ok(())
}

fn link_directories(interp: &mut Interp, args: &[String]) -> CmdResult {
    if args.is_empty() {
        return Ok(());
    }
    let (before, dirs) = match args[0].as_str() {
        "BEFORE" => (true, &args[1..]),
        "AFTER" => (false, &args[1..]),
        _ => (interp.is_on("CMAKE_LINK_DIRECTORIES_BEFORE"), args),
    };
    let base = interp.current_source_dir();
    let paths: Vec<_> = dirs
        .iter()
        .map(|dir| input_path(&base, &convert_to_unix_slashes(dir)))
        .collect();
    append_directory(interp, "LINK_DIRECTORIES", &paths.join(";"), before);
    Ok(())
}

fn link_libraries(interp: &mut Interp, args: &[String]) -> CmdResult {
    let mut args = args.iter().peekable();
    while let Some(arg) = args.next() {
        if matches!(arg.as_str(), "debug" | "optimized") {
            if args.peek().is_none() {
                return err(format!(
                    "The \"{arg}\" argument must be followed by a library"
                ));
            }
            append_directory(interp, "LINK_LIBRARIES", arg, false);
            append_directory(interp, "LINK_LIBRARIES", args.next().unwrap(), false);
        } else {
            if !arg.is_empty() {
                append_directory(interp, "LINK_LIBRARIES", arg, false);
            }
        }
    }
    Ok(())
}

pub(crate) fn normalized_group(name: &str, delimiters: &str) -> String {
    name.split(|ch| delimiters.contains(ch))
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("\\")
}

fn source_group(interp: &mut Interp, args: &[String]) -> CmdResult {
    const OPTIONS: &[&str] = &["TREE", "PREFIX", "FILES", "REGULAR_EXPRESSION"];
    let Some((first, rest)) = args.split_first() else {
        return err("called with incorrect number of arguments");
    };
    let dir = interp.mf_index();
    let base = interp.current_source_dir();
    let mut parsed: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    let short_tree = first == "TREE" && args.len() == 2 && Path::new(&args[1]).is_dir();
    if args.len() == 2 && args[1] != "FILES" && !short_tree {
        parsed.insert("REGULAR_EXPRESSION", vec![&args[1]]);
    } else {
        let mut args = if OPTIONS.contains(&first.as_str()) {
            args
        } else {
            rest
        };
        while let Some((key, tail)) = args.split_first() {
            let end = tail
                .iter()
                .position(|arg| OPTIONS.contains(&arg.as_str()))
                .unwrap_or(tail.len());
            parsed
                .entry(key)
                .or_default()
                .extend(tail[..end].iter().map(String::as_str));
            args = &tail[end..];
        }
    }
    for option in ["PREFIX", "TREE", "REGULAR_EXPRESSION"] {
        if parsed.get(option).is_some_and(|args| args.len() != 1) {
            return err("unknown error.");
        }
    }
    if let Some(tree) = parsed.get("TREE") {
        let root = collapse_full_path(tree[0], "");
        let prefix = parsed.get("PREFIX").map_or("", |args| args[0]);
        let files = parsed
            .get("FILES")
            .map(|files| files.iter().map(|file| (*file).to_owned()).collect())
            .unwrap_or_else(|| {
                interp
                    .ext
                    .sources
                    .get(&dir)
                    .into_iter()
                    .flatten()
                    .filter(|file| !file.generated)
                    .map(|file| file.location.path())
                    .collect::<Vec<_>>()
            });
        let files: BTreeSet<_> = files
            .iter()
            .map(|file| collapse_full_path(file, &base))
            .filter(|file| !Path::new(file).is_dir())
            .collect();
        if let Some(file) = files.iter().find(|file| !file.starts_with(&root)) {
            return err(format!("ROOT: {root} is not a prefix of file: {file}"));
        }
        for file in files {
            let relative = file.get(root.len() + 1..).unwrap_or_default();
            let parent = relative.rsplit_once('/').map_or("", |(parent, _)| parent);
            let group = normalized_group(&format!("{prefix}/{parent}"), "/\\");
            interp
                .ext
                .source_groups
                .entry(dir)
                .or_default()
                .entry(group)
                .or_insert_with(|| SourceGroup {
                    files: BTreeSet::new(),
                    regex: None,
                })
                .files
                .insert(file);
        }
    } else {
        if OPTIONS.contains(&first.as_str()) && !(args.len() == 2 && args[1] != "FILES") {
            return err("Missing source group name.");
        }
        let name = normalized_group(
            first,
            interp.get_def("SOURCE_GROUP_DELIMITER").unwrap_or("/\\"),
        );
        let group = interp
            .ext
            .source_groups
            .entry(dir)
            .or_default()
            .entry(name)
            .or_insert_with(|| SourceGroup {
                files: BTreeSet::new(),
                regex: None,
            });
        if let Some(regex) = parsed.get("REGULAR_EXPRESSION") {
            group.regex = Some(regex[0].to_owned());
        }
        group.files.extend(
            parsed
                .get("FILES")
                .into_iter()
                .flatten()
                .map(|path| collapse_full_path(path, &base)),
        );
    }
    Ok(())
}
