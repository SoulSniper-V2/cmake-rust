//! `cmFileCommand`'s `HandleGetRuntimeDependenciesCommand` for Linux:
//! `cmRuntimeDependencyArchive`, `cmBinUtilsLinuxELFLinker`,
//! `cmBinUtilsLinuxELFObjdumpGetRuntimeDependenciesTool` and
//! `cmLDConfigLDConfigTool`.

use std::collections::{BTreeMap, BTreeSet, HashSet, VecDeque};
use std::os::unix::fs::MetadataExt;
use std::process::Command;

use super::archive::{Kind, maybe_report_error, parse};
use crate::cmelf::Elf;
use crate::cmregex::Regex;
use crate::commands::legacy_fs::fatal;
use crate::interp::{CmdResult, Interp, Mode};
use crate::messages::MessageType;
use crate::util::{filename_path, join_list};

/// `cmSystemTools::SameFile`.
fn same_file(a: &str, b: &str) -> bool {
    std::fs::metadata(a)
        .ok()
        .zip(std::fs::metadata(b).ok())
        .is_some_and(|(a, b)| a.dev() == b.dev() && a.ino() == b.ino())
}

/// `cmSystemTools::PathExists` (kwsys: `access(F_OK)`, so a dangling
/// symlink does not count).
fn path_exists(path: &str) -> bool {
    std::fs::metadata(path).is_ok()
}

/// `cmSystemTools::SplitString(value, ':')`: empty pieces are dropped.
fn split_path_list(value: &str) -> Vec<String> {
    value
        .split(':')
        .filter(|part| !part.is_empty())
        .map(str::to_owned)
        .collect()
}

/// A `cmsys::RegularExpression` that may have failed to compile; upstream
/// treats an uncompilable expression as never matching.
struct Filter(Option<Regex>);

impl Filter {
    fn new(pattern: &str) -> Filter {
        Filter(Regex::new(pattern).ok())
    }

    fn find(&self, text: &str) -> bool {
        self.0.as_ref().is_some_and(|re| re.find(text).is_some())
    }
}

/// `cmRuntimeDependencyArchive` merged with `cmBinUtilsLinuxELFLinker`
/// and its objdump/ldconfig tools (Linux-only port).
struct Archive<'a> {
    interp: &'a mut Interp,
    search_directories: Vec<String>,
    pre_include: Vec<Filter>,
    pre_exclude: Vec<Filter>,
    post_include: Vec<Filter>,
    post_exclude: Vec<Filter>,
    post_include_files: Vec<String>,
    post_exclude_files: Vec<String>,
    post_exclude_files_strict: Vec<String>,
    resolved_paths: BTreeMap<String, BTreeSet<String>>,
    unresolved_paths: BTreeSet<String>,
    rpaths: BTreeMap<String, Vec<String>>,
    ldconfig_paths: Vec<String>,
    machine: u16,
    objdump: Vec<String>,
    error: String,
}

/// `cmBinUtilsLinuxELFLinker::ReplaceOrigin`: the `(\$ORIGIN)([^a-zA-Z0-9_]|$)`
/// match is tried before `\$\{ORIGIN\}`.
fn replace_origin(rpath: &str, origin: &str) -> String {
    let bytes = rpath.as_bytes();
    for index in 0..bytes.len().saturating_sub(6) {
        if &bytes[index..index + 7] == b"$ORIGIN"
            && (index + 7 == bytes.len()
                || !matches!(bytes[index + 7], b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'_'))
        {
            return format!("{}{}{}", &rpath[..index], origin, &rpath[index + 7..]);
        }
    }
    if let Some(index) = rpath.find("${ORIGIN}") {
        return format!("{}{}{}", &rpath[..index], origin, &rpath[index + 9..]);
    }
    rpath.to_owned()
}

impl Archive<'_> {
    fn set_error(&mut self, message: impl Into<String>) {
        self.error = message.into();
    }

    /// `cmRuntimeDependencyArchive::IsPreExcluded`.
    fn is_pre_excluded(&self, name: &str) -> bool {
        !self.pre_include.iter().any(|re| re.find(name))
            && self.pre_exclude.iter().any(|re| re.find(name))
    }

    /// `cmRuntimeDependencyArchive::IsPostExcluded`.
    fn is_post_excluded(&self, name: &str) -> bool {
        let file_search = |files: &[String]| files.iter().any(|f| same_file(f, name));
        file_search(&self.post_exclude_files_strict)
            || (!(self.post_include.iter().any(|re| re.find(name))
                || file_search(&self.post_include_files))
                && (self.post_exclude.iter().any(|re| re.find(name))
                    || file_search(&self.post_exclude_files)))
    }

    /// `cmBinUtilsLinuxELFObjdumpGetRuntimeDependenciesTool::GetFileInfo`.
    fn get_file_info(&mut self, file: &str) -> Option<(Vec<String>, Vec<String>, Vec<String>)> {
        let mut command = Command::new(&self.objdump[0]);
        command.args(&self.objdump[1..]).arg("-p").arg(file);
        let output = match command.output() {
            Ok(output) => output,
            Err(_) => {
                self.set_error(format!("Failed to start objdump process for:\n  {file}"));
                return None;
            }
        };
        let mut needed = Vec::new();
        let mut rpaths = Vec::new();
        let mut runpaths = Vec::new();
        for line in String::from_utf8_lossy(&output.stdout).lines() {
            let text = line.trim_start_matches(' ');
            if let Some(rest) = text.strip_prefix("NEEDED") {
                needed.push(rest.trim_matches(' ').to_owned());
            } else if let Some(rest) = text.strip_prefix("RPATH") {
                rpaths.extend(split_path_list(rest.trim_matches(' ')));
            } else if let Some(rest) = text.strip_prefix("RUNPATH") {
                runpaths.extend(split_path_list(rest.trim_matches(' ')));
            }
        }
        if !output.status.success() {
            self.set_error(format!("Failed to run objdump on:\n  {file}"));
            return None;
        }
        Some((needed, rpaths, runpaths))
    }

    /// `FileHasArchitecture`.
    fn file_has_architecture(&self, filename: &str) -> bool {
        let elf = Elf::open(filename);
        if !elf.is_valid() {
            return false;
        }
        self.machine == 0 || self.machine == elf.machine()
    }

    /// `cmBinUtilsLinuxELFLinker::ResolveDependency`.
    fn resolve_dependency(
        &mut self,
        name: &str,
        search_paths: &[String],
        path: &mut String,
        resolved: &mut bool,
    ) -> bool {
        for search_path in search_paths {
            *path = format!("{search_path}/{name}");
            if path_exists(path) && self.file_has_architecture(path) {
                *resolved = true;
                return true;
            }
        }
        for search_path in self.search_directories.clone() {
            *path = format!("{search_path}/{name}");
            if path_exists(path) && self.file_has_architecture(path) {
                self.interp.issue(
                    MessageType::Warning,
                    &format!(
                        "Dependency {name} found in search directory:\n  {search_path}\nSee file(GET_RUNTIME_DEPENDENCIES) documentation for more information."
                    ),
                );
                *resolved = true;
                return true;
            }
        }
        *resolved = false;
        true
    }

    /// `cmBinUtilsLinuxELFLinker::ScanDependencies` (outer entry, ELF check
    /// plus the machine-consistency rule).
    fn scan_dependencies_entry(&mut self, file: &str) -> bool {
        let elf = Elf::open(file);
        if !elf.is_valid() {
            return false;
        }
        let machine = elf.machine();
        if machine != 0 {
            if self.machine != 0 {
                if machine != self.machine {
                    self.set_error("All files must have the same architecture.");
                    return false;
                }
            } else {
                self.machine = machine;
            }
        }
        self.scan_dependencies(file)
    }

    /// `cmBinUtilsLinuxELFLinker::ScanDependencies` (BFS).
    fn scan_dependencies(&mut self, main_file: &str) -> bool {
        let mut resolved_dependencies: HashSet<String> = HashSet::new();
        let mut queue: VecDeque<(String, Vec<String>)> = VecDeque::new();
        queue.push_back((main_file.to_owned(), Vec::new()));

        while let Some((file, parent_rpaths)) = queue.pop_front() {
            let origin = filename_path(&file);
            let Some((needed, mut rpaths, mut runpaths)) = self.get_file_info(&file) else {
                return false;
            };
            for path in &mut runpaths {
                *path = replace_origin(path, &origin);
            }
            for path in &mut rpaths {
                *path = replace_origin(path, &origin);
            }
            let mut search_paths = if !runpaths.is_empty() {
                runpaths
            } else {
                let mut paths = rpaths.clone();
                paths.extend(parent_rpaths.iter().cloned());
                paths
            };
            search_paths.extend(self.ldconfig_paths.iter().cloned());

            for dep in needed {
                if resolved_dependencies.contains(&dep) || self.is_pre_excluded(&dep) {
                    continue;
                }
                if dep.contains('/') {
                    self.set_error("Paths to dependencies are not supported");
                    return false;
                }
                let mut path = String::new();
                let mut resolved = false;
                if !self.resolve_dependency(&dep, &search_paths, &mut path, &mut resolved) {
                    return false;
                }
                if resolved {
                    resolved_dependencies.insert(dep.clone());
                    if !self.is_post_excluded(&path) {
                        // cmRuntimeDependencyArchive::AddResolvedPath.
                        let entry = self.resolved_paths.entry(dep.clone()).or_default();
                        let mut unique = true;
                        for other in entry.iter() {
                            if same_file(&path, other) {
                                unique = false;
                                break;
                            }
                        }
                        entry.insert(path.clone());
                        self.rpaths.entry(path.clone()).or_default();
                        if unique {
                            let mut combined = parent_rpaths.clone();
                            combined.extend(rpaths.iter().cloned());
                            queue.push_back((path, combined));
                        }
                    }
                } else {
                    self.unresolved_paths.insert(dep);
                }
            }
        }
        true
    }
}

/// `cmLDConfigLDConfigTool::GetLDConfigPaths`.
fn ldconfig_paths(interp: &Interp, error: &mut String) -> Option<Vec<String>> {
    let mut ldconfig = interp.get_safe("CMAKE_LDCONFIG_COMMAND");
    if ldconfig.is_empty() {
        for dir in ["/sbin", "/usr/sbin", "/usr/local/sbin"] {
            let candidate = format!("{dir}/ldconfig");
            if std::fs::metadata(&candidate).is_ok() {
                ldconfig = candidate;
                break;
            }
        }
        if ldconfig.is_empty() {
            // cmSystemTools::FindProgram also searches PATH.
            if let Some(path) = std::env::var_os("PATH") {
                for dir in std::env::split_paths(&path) {
                    let candidate = dir.join("ldconfig");
                    if candidate.is_file() {
                        ldconfig = candidate.to_string_lossy().into_owned();
                        break;
                    }
                }
            }
        }
        if ldconfig.is_empty() {
            *error = "Could not find ldconfig".to_owned();
            return None;
        }
    }
    let output = match Command::new(&ldconfig).args(["-v", "-N", "-X"]).output() {
        Ok(output) => output,
        Err(_) => {
            *error = "Failed to start ldconfig process".to_owned();
            return None;
        }
    };
    if !output.status.success() {
        *error = "Failed to run ldconfig".to_owned();
        return None;
    }
    let mut paths = Vec::new();
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        // "^\([^\t:]*\):" — directory headers of the ldconfig listing.
        let text = line.split('\t').next().unwrap_or(line);
        if let Some((dir, _)) = text.split_once(':')
            && !dir.is_empty()
        {
            paths.push(dir.to_owned());
        }
    }
    Some(paths)
}

/// `cmRuntimeDependencyArchive::GetGetRuntimeDependenciesCommand`
/// restricted to the objdump path: `CMAKE_GET_RUNTIME_DEPENDENCIES_COMMAND`,
/// `CMAKE_OBJDUMP`, then a program search for `objdump`.
fn get_dependencies_command(interp: &Interp, search: &str) -> Vec<String> {
    let mut tool_command = interp.get_safe("CMAKE_GET_RUNTIME_DEPENDENCIES_COMMAND");
    if tool_command.is_empty() && search == "objdump" {
        tool_command = interp.get_safe("CMAKE_OBJDUMP");
    }
    if !tool_command.is_empty() {
        return crate::util::expand_list(&tool_command, false);
    }
    if let Some(path) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&path) {
            let candidate = dir.join(search);
            if candidate.is_file() {
                return vec![candidate.to_string_lossy().into_owned()];
            }
        }
    }
    Vec::new()
}

pub(super) fn get_runtime_dependencies(interp: &mut Interp, args: &[String]) -> CmdResult {
    let system = interp.get_safe("CMAKE_HOST_SYSTEM_NAME");
    // cmRuntimeDependencyArchive::PlatformSupportsRuntimeDependencies.
    if !matches!(system.as_str(), "Windows" | "Linux" | "Darwin") {
        return Err(fatal(
            interp,
            &format!("file GET_RUNTIME_DEPENDENCIES is not supported on system \"{system}\""),
        ));
    }
    if interp.mode == Mode::Project {
        interp.issue(
            MessageType::AuthorWarning,
            "You have used file(GET_RUNTIME_DEPENDENCIES) in project mode. This is probably not what you intended to do. Instead, please consider using it in an install(CODE) or install(SCRIPT) command. For example:\n  install(CODE [[\n    file(GET_RUNTIME_DEPENDENCIES\n      # ...\n      )\n    ]])",
        );
    }
    let parsed = parse(
        &args[1..],
        &[
            ("RESOLVED_DEPENDENCIES_VAR", Kind::Single),
            ("UNRESOLVED_DEPENDENCIES_VAR", Kind::Single),
            ("CONFLICTING_DEPENDENCIES_PREFIX", Kind::Single),
            ("RPATH_PREFIX", Kind::Single),
            ("BUNDLE_EXECUTABLE", Kind::Single),
            ("EXECUTABLES", Kind::Multi),
            ("LIBRARIES", Kind::Multi),
            ("MODULES", Kind::Multi),
            ("DIRECTORIES", Kind::Multi),
            ("PRE_INCLUDE_REGEXES", Kind::Multi),
            ("PRE_EXCLUDE_REGEXES", Kind::Multi),
            ("POST_INCLUDE_REGEXES", Kind::Multi),
            ("POST_EXCLUDE_REGEXES", Kind::Multi),
            ("POST_INCLUDE_FILES", Kind::Multi),
            ("POST_EXCLUDE_FILES", Kind::Multi),
            ("POST_EXCLUDE_FILES_STRICT", Kind::Multi),
        ],
    );
    if let Some(bad) = parsed.unrecognized.first() {
        return Err(fatal(
            interp,
            &format!("file Unrecognized argument: \"{bad}\""),
        ));
    }
    if maybe_report_error(interp, &parsed) {
        return Ok(());
    }

    // cmRuntimeDependencyArchive::Prepare.
    let mut platform = interp.get_safe("CMAKE_GET_RUNTIME_DEPENDENCIES_PLATFORM");
    if platform.is_empty() {
        platform = match system.as_str() {
            "Windows" => "windows+pe",
            "Darwin" => "macos+macho",
            "Linux" => "linux+elf",
            _ => "",
        }
        .to_owned();
    }
    if platform != "linux+elf" {
        return Err(fatal(
            interp,
            &format!("file Invalid value for CMAKE_GET_RUNTIME_DEPENDENCIES_PLATFORM: {platform}"),
        ));
    }
    let mut tool = interp.get_safe("CMAKE_GET_RUNTIME_DEPENDENCIES_TOOL");
    if tool.is_empty() {
        tool = "objdump".to_owned();
    }
    if tool != "objdump" {
        return Err(fatal(
            interp,
            &format!("file Invalid value for CMAKE_GET_RUNTIME_DEPENDENCIES_TOOL: {tool}"),
        ));
    }
    let mut ldconfig_tool = interp.get_safe("CMAKE_LDCONFIG_TOOL");
    if ldconfig_tool.is_empty() {
        ldconfig_tool = "ldconfig".to_owned();
    }
    if ldconfig_tool != "ldconfig" {
        return Err(fatal(
            interp,
            &format!("file Invalid value for CMAKE_LDCONFIG_TOOL: {ldconfig_tool}"),
        ));
    }
    let mut error = String::new();
    let Some(ldconfig_paths) = ldconfig_paths(interp, &mut error) else {
        return Err(fatal(interp, &format!("file {error}")));
    };
    let objdump = get_dependencies_command(interp, "objdump");
    if objdump.is_empty() {
        return Err(fatal(interp, "file Could not find objdump"));
    }

    let mut archive = Archive {
        search_directories: parsed.multi("DIRECTORIES").to_vec(),
        pre_include: parsed
            .multi("PRE_INCLUDE_REGEXES")
            .iter()
            .map(|p| Filter::new(p))
            .collect(),
        pre_exclude: parsed
            .multi("PRE_EXCLUDE_REGEXES")
            .iter()
            .map(|p| Filter::new(p))
            .collect(),
        post_include: parsed
            .multi("POST_INCLUDE_REGEXES")
            .iter()
            .map(|p| Filter::new(p))
            .collect(),
        post_exclude: parsed
            .multi("POST_EXCLUDE_REGEXES")
            .iter()
            .map(|p| Filter::new(p))
            .collect(),
        post_include_files: parsed.multi("POST_INCLUDE_FILES").to_vec(),
        post_exclude_files: parsed.multi("POST_EXCLUDE_FILES").to_vec(),
        post_exclude_files_strict: parsed.multi("POST_EXCLUDE_FILES_STRICT").to_vec(),
        resolved_paths: BTreeMap::new(),
        unresolved_paths: BTreeSet::new(),
        rpaths: BTreeMap::new(),
        ldconfig_paths,
        machine: 0,
        objdump,
        error: String::new(),
        interp,
    };

    let inputs = [
        parsed.multi("EXECUTABLES"),
        parsed.multi("LIBRARIES"),
        parsed.multi("MODULES"),
    ]
    .concat();
    for file in &inputs {
        if !archive.scan_dependencies_entry(file) {
            let message = if archive.error.is_empty() {
                "file unknown error.".to_owned()
            } else {
                format!("file {}", archive.error)
            };
            let interp = archive.interp;
            return Err(fatal(interp, &message));
        }
    }

    let resolved_dependencies_var = parsed
        .single("RESOLVED_DEPENDENCIES_VAR")
        .unwrap_or_default();
    let unresolved_dependencies_var = parsed
        .single("UNRESOLVED_DEPENDENCIES_VAR")
        .unwrap_or_default();
    let conflicting_dependencies_prefix = parsed
        .single("CONFLICTING_DEPENDENCIES_PREFIX")
        .unwrap_or_default();
    let rpath_prefix = parsed.single("RPATH_PREFIX").unwrap_or_default();

    let mut deps = Vec::new();
    let mut unresolved_deps = Vec::new();
    let mut conflicting_deps = Vec::new();
    let resolved_paths = std::mem::take(&mut archive.resolved_paths);
    for (name, paths) in &resolved_paths {
        let mut iter = paths.iter();
        let first_path = iter.next().expect("non-empty path set");
        let unique = iter.all(|other| same_file(first_path, other));
        if unique {
            deps.push(first_path.clone());
            if !rpath_prefix.is_empty() {
                archive.interp.set_def(
                    format!("{rpath_prefix}_{first_path}"),
                    archive
                        .rpaths
                        .get(first_path)
                        .map(|paths| join_list(paths))
                        .unwrap_or_default(),
                );
            }
        } else if !conflicting_dependencies_prefix.is_empty() {
            conflicting_deps.push(name.clone());
            archive.interp.set_def(
                format!("{conflicting_dependencies_prefix}_{name}"),
                join_list(&paths.iter().collect::<Vec<_>>()),
            );
        } else {
            let mut message = format!("file Multiple conflicting paths found for {name}:");
            for path in paths {
                message.push_str(&format!("\n  {path}"));
            }
            return Err(fatal(archive.interp, &message));
        }
    }
    if !archive.unresolved_paths.is_empty() {
        if !unresolved_dependencies_var.is_empty() {
            unresolved_deps.extend(archive.unresolved_paths.iter().cloned());
        } else {
            let mut message = "file Could not resolve runtime dependencies:".to_owned();
            for path in &archive.unresolved_paths {
                message.push_str(&format!("\n  {path}"));
            }
            return Err(fatal(archive.interp, &message));
        }
    }
    if !resolved_dependencies_var.is_empty() {
        archive
            .interp
            .set_def(resolved_dependencies_var, join_list(&deps));
    }
    if !unresolved_dependencies_var.is_empty() {
        archive
            .interp
            .set_def(unresolved_dependencies_var, join_list(&unresolved_deps));
    }
    if !conflicting_dependencies_prefix.is_empty() {
        archive.interp.set_def(
            format!("{conflicting_dependencies_prefix}_FILENAMES"),
            join_list(&conflicting_deps),
        );
    }
    Ok(())
}
