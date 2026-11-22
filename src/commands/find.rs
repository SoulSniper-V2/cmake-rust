//! The find command family, ported from cmFindBase and its subclasses.

mod common;
pub mod package;

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::Path;

use regex::Regex;

use crate::cache::{CacheType, convert_to_unix_slashes};
use crate::interp::{CmdErr, CmdResult, CommandDef, Interp, err};
use crate::messages::MessageType;
use crate::policy::{PolicyStatus, policy_warning};
use crate::util::{collapse_full_path, expand_list, is_notfound, is_on};

use self::common::*;

pub fn register(interp: &mut Interp) {
    interp.register("find_program", |i, a| find(i, a, Kind::Program));
    interp.register("find_library", |i, a| find(i, a, Kind::Library));
    interp.register("find_file", |i, a| find(i, a, Kind::File));
    interp.register("find_path", |i, a| find(i, a, Kind::Path));
    package::register(interp);
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Program,
    Library,
    File,
    Path,
}

impl Kind {
    fn command(self) -> &'static str {
        match self {
            Self::Program => "find_program",
            Self::Library => "find_library",
            Self::File => "find_file",
            Self::Path => "find_path",
        }
    }

    fn path_name(self) -> &'static str {
        match self {
            Self::Program => "PROGRAM",
            Self::Library => "LIBRARY",
            Self::File | Self::Path => "INCLUDE",
        }
    }

    fn documentation(self) -> &'static str {
        match self {
            Self::Program => "Path to a program.",
            Self::Library => "Path to a library.",
            Self::File | Self::Path => "Path to a file.",
        }
    }
}

#[derive(Clone, Copy)]
enum Section {
    None,
    Names,
    Hints,
    Paths,
    Suffixes,
}

struct Find {
    kind: Kind,
    variable: String,
    doc: String,
    cache_type: CacheType,
    store_cache: bool,
    options: Options,
    names: Vec<String>,
    hints: Vec<String>,
    paths: Vec<String>,
    suffixes: Vec<String>,
    names_per_dir: bool,
    required: Option<bool>,
    validator: Option<String>,
    debug: Option<Vec<String>>,
}

impl Find {
    fn new(interp: &Interp, args: &[String], kind: Kind) -> Result<(Self, Vec<String>), CmdErr> {
        if args.len() < 2 {
            return err("called with incorrect number of arguments");
        }
        let mut filtered = Vec::new();
        let mut doc = None;
        let mut iter = args.iter();
        let mut store_cache = true;
        while let Some(arg) = iter.next() {
            match arg.as_str() {
                "NO_CACHE" => store_cache = false,
                "DOC" if doc.is_none() => doc = iter.next().cloned(),
                "ENV" => {
                    if let Some(name) = iter.next() {
                        filtered.extend(env_paths(name));
                    }
                }
                _ => filtered.push(arg.clone()),
            }
        }
        if filtered.len() < 2 {
            return err("called with incorrect number of arguments");
        }
        let variable = filtered.remove(0);
        let debug_wanted = interp.find_debug_wanted(Some(&variable));
        let mut cache_type = if kind == Kind::Path {
            CacheType::Path
        } else {
            CacheType::Filepath
        };
        let mut doc = doc.unwrap_or_else(|| kind.documentation().to_owned());
        if interp.is_def_set(&variable)
            && let Some(entry) = interp.cache.get(&variable)
            && entry.kind != CacheType::Uninitialized
        {
            cache_type = entry.kind;
            if let Some(help) = entry.properties.get("HELPSTRING") {
                doc.clone_from(help);
            }
        }
        Ok((
            Self {
                kind,
                variable,
                doc,
                cache_type,
                store_cache,
                options: Options::new(interp, kind.path_name()),
                names: Vec::new(),
                hints: Vec::new(),
                paths: Vec::new(),
                suffixes: Vec::new(),
                names_per_dir: false,
                required: None,
                validator: None,
                debug: debug_wanted.then(Vec::new),
            },
            filtered,
        ))
    }

    fn parse(&mut self, interp: &Interp, args: &[String]) -> CmdResult {
        let mut section = Section::Names;
        let mut new_style = false;
        let mut args = args.iter();
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "NAMES" | "HINTS" | "PATHS" | "PATH_SUFFIXES" => {
                    section = match arg.as_str() {
                        "NAMES" => Section::Names,
                        "HINTS" => Section::Hints,
                        "PATHS" => Section::Paths,
                        _ => Section::Suffixes,
                    };
                    new_style = true;
                }
                "NAMES_PER_DIR" => {
                    if !matches!(self.kind, Kind::Program | Kind::Library) {
                        return err("does not support NAMES_PER_DIR");
                    }
                    self.names_per_dir = true;
                    section = Section::None;
                }
                "NO_SYSTEM_PATH" => {
                    self.options.no_default = true;
                    section = Section::None;
                }
                "REQUIRED" | "OPTIONAL" => {
                    let required = arg == "REQUIRED";
                    if self.required.is_some_and(|previous| previous != required) {
                        return err("cannot be both REQUIRED and OPTIONAL");
                    }
                    self.required = Some(required);
                    new_style = true;
                    section = Section::None;
                }
                "REGISTRY_VIEW" => {
                    registry_view(args.next())?;
                }
                "VALIDATOR" => {
                    let name = args
                        .next()
                        .ok_or("missing required argument for VALIDATOR")?;
                    let Some(command) = interp.commands.get(&name.to_ascii_lowercase()) else {
                        return err(format!(
                            "command specified for VALIDATOR is undefined: {name}."
                        ));
                    };
                    if matches!(command, CommandDef::User(callable) if callable.is_macro) {
                        return err(format!(
                            "command specified for VALIDATOR is not a function: {name}."
                        ));
                    }
                    self.validator = Some(name.clone());
                }
                _ if self.options.parse(arg) => section = Section::None,
                _ => match section {
                    Section::Names => self.names.push(arg.clone()),
                    Section::Hints => self.hints.push(arg.clone()),
                    Section::Paths => self.paths.push(arg.clone()),
                    Section::Suffixes => self.suffixes.extend(suffix(arg)),
                    Section::None => {}
                },
            }
        }
        self.required
            .get_or_insert_with(|| interp.is_on("CMAKE_FIND_REQUIRED"));
        if self.doc.is_empty() {
            self.doc = match self.names.split_last() {
                None => "Where can the (unknown) library be found".into(),
                Some((name, [])) => format!("Where can the {name} library be found"),
                Some((last, names)) => format!(
                    "Where can one of the {} or {last} libraries be found",
                    names.join(", ")
                ),
            };
        }
        if !new_style && self.names.len() > 1 {
            self.paths.extend(self.names.drain(1..));
        }
        Ok(())
    }

    fn search_paths(&self, interp: &Interp) -> Vec<String> {
        let options = &self.options;
        let mut paths = SearchPaths::default();
        let subdir = match self.kind {
            Kind::Program => "bin",
            Kind::Library => "lib",
            _ => "include",
        };
        let kind = self.kind.path_name();
        let extra = if self.kind == Kind::Program {
            "APPBUNDLE"
        } else {
            "FRAMEWORK"
        };
        if options.uses(ROOT) {
            self.expand_prefixes(
                interp,
                &mut paths,
                subdir,
                package::root_paths(interp).cloned(),
            );
            paths.finish_group(&self.suffixes);
        }
        if options.uses(CMAKE) {
            self.expand_prefixes(
                interp,
                &mut paths,
                subdir,
                cmake_paths(interp, "CMAKE_PREFIX_PATH"),
            );
            paths.extend(cmake_paths(interp, &format!("CMAKE_{kind}_PATH")));
            paths.extend(cmake_paths(interp, &format!("CMAKE_{extra}_PATH")));
            paths.finish_group(&self.suffixes);
        }
        if options.uses(CMAKE_ENV) {
            self.expand_prefixes(interp, &mut paths, subdir, env_paths("CMAKE_PREFIX_PATH"));
            paths.extend(env_paths(&format!("CMAKE_{kind}_PATH")));
            paths.extend(env_paths(&format!("CMAKE_{extra}_PATH")));
            paths.finish_group(&self.suffixes);
        }
        paths.extend(self.hints.iter().flat_map(|path| user_paths(interp, path)));
        paths.finish_group(&self.suffixes);
        if options.uses(SYSTEM_ENV) {
            match self.kind {
                Kind::Program => {}
                Kind::Library => paths.extend(env_paths("LIB")),
                _ => paths.extend(env_paths("INCLUDE")),
            }
            paths.extend(env_paths("PATH"));
            paths.finish_group(&self.suffixes);
        }
        if options.uses(SYSTEM) {
            self.expand_prefixes(
                interp,
                &mut paths,
                subdir,
                options.system_prefixes(interp, false),
            );
            paths.extend(cmake_paths(interp, &format!("CMAKE_SYSTEM_{kind}_PATH")));
            paths.extend(cmake_paths(interp, &format!("CMAKE_SYSTEM_{extra}_PATH")));
            paths.finish_group(&self.suffixes);
        }
        paths.extend(self.paths.iter().flat_map(|path| user_paths(interp, path)));
        paths.finish_group(&self.suffixes);

        let paths = paths.into_paths(&ignored_paths(interp, false), &ignored_paths(interp, true));
        options
            .reroot(interp, paths, None)
            .into_iter()
            .map(trailing_slash)
            .collect()
    }

    fn expand_prefixes(
        &self,
        interp: &Interp,
        paths: &mut SearchPaths,
        subdir: &str,
        prefixes: impl IntoIterator<Item = String>,
    ) {
        let arch = interp.get_safe("CMAKE_LIBRARY_ARCHITECTURE");
        for prefix in prefixes {
            let dir = trailing_slash(prefix.clone());
            let prefix = if prefix == "/" {
                prefix.as_str()
            } else {
                prefix.trim_end_matches('/')
            };
            if subdir != "bin" && !arch.is_empty() {
                let short_arch = arch.replacen("-unknown-", "-", 1);
                for arch in [&short_arch, &arch] {
                    if interp.is_def_set("CMAKE_SYSROOT")
                        && interp.is_def_set("CMAKE_PREFIX_LIBRARY_ARCHITECTURE")
                    {
                        paths.add(format!("/{arch}{dir}{subdir}"), &format!("/{arch}{prefix}"));
                    } else {
                        paths.add(format!("{dir}{subdir}/{arch}"), prefix);
                    }
                }
            }
            paths.add(format!("{dir}{subdir}"), prefix);
            if subdir == "bin" {
                paths.add(format!("{dir}sbin"), prefix);
            }
            if prefix != "/" {
                paths.add(prefix.to_owned(), prefix);
            }
        }
    }

    fn validate(&self, interp: &mut Interp, path: &str) -> bool {
        let Some(validator) = &self.validator else {
            return true;
        };
        let status = format!(
            "CMAKE_{}_VALIDATOR_STATUS",
            self.kind.command().to_ascii_uppercase()
        );
        interp.push_scope();
        interp.push_policy(false, Default::default());
        interp.set_def(&status, "TRUE");
        let result = interp.call_command(validator, &[status.clone(), path.to_owned()])
            && interp.is_on(&status);
        interp.pop_policy();
        interp.pop_scope();
        result
    }

    fn failed(&mut self, path: String) {
        if let Some(considered) = &mut self.debug {
            considered.push(path);
        }
    }

    fn debug_result(&self, interp: &mut Interp, result: Option<&str>) {
        let Some(paths) = &self.debug else { return };
        let command = self.kind.command();
        let mut text = format!(
            "{command} called with the following settings:\n  VAR: {}\n  NAMES: {}\n  Documentation: {}\n",
            self.variable,
            self.names
                .iter()
                .map(|name| format!("\"{name}\""))
                .collect::<Vec<_>>()
                .join("\n         "),
            self.doc,
        );
        for (section, label, mode) in [
            ("Framework", "Frameworks", self.options.framework),
            ("AppBundle", "AppBundle", self.options.bundle),
        ] {
            let _ = writeln!(
                text,
                "  {section}\n    Only Search {label}: {}\n    Search {label} Last: {}\n    Search {label} First: {}",
                u8::from(mode == MacMode::Only),
                u8::from(mode == MacMode::Last),
                u8::from(mode == MacMode::First)
            );
        }
        if self.options.no_default {
            text.push_str("  NO_DEFAULT_PATH Enabled\n");
        } else {
            for (source, enabled) in SOURCES[CMAKE..].iter().zip(&self.options.enabled[CMAKE..]) {
                let _ = writeln!(text, "  CMAKE_FIND_USE_{source}: {}", u8::from(*enabled));
            }
        }
        let _ = writeln!(text, "{command} considered the following locations:");
        for path in paths {
            let _ = writeln!(text, "  {path}");
        }
        if let Some(path) = result {
            let _ = writeln!(text, "The item was found at\n  {path}");
        } else {
            text.push_str("The item was not found.\n");
        }
        interp.issue(MessageType::Log, &text);
    }

    fn search(&mut self, interp: &mut Interp) -> Option<String> {
        let mut dirs = self.search_paths(interp);
        let library = (self.kind == Kind::Library).then(|| LibraryNames::new(interp, &self.names));
        if library.is_some() {
            let alternative = interp
                .get_def("CMAKE_FIND_LIBRARY_CUSTOM_LIB_SUFFIX")
                .or_else(|| lib_path_variants(interp).next())
                .map(str::to_owned);
            if let Some(suffix) = alternative {
                dirs = self.architecture_paths(interp, dirs, &suffix);
            }
        }
        let mode = if self.kind == Kind::Program {
            self.options.bundle
        } else {
            self.options.framework
        };
        if matches!(mode, MacMode::First | MacMode::Only)
            && let Some(path) = self.search_special(interp, &dirs)
        {
            return Some(path);
        }
        if mode != MacMode::Only
            && let Some(path) = self.search_normal(interp, &dirs, library.as_ref())
        {
            return Some(path);
        }
        if mode == MacMode::Last {
            return self.search_special(interp, &dirs);
        }
        None
    }

    fn search_normal(
        &mut self,
        interp: &mut Interp,
        dirs: &[String],
        library: Option<&LibraryNames>,
    ) -> Option<String> {
        let names = self.names.clone();
        if self.names_per_dir {
            if self.kind == Kind::Program
                && let Some(path) = names
                    .iter()
                    .filter(|name| name.contains('/'))
                    .find_map(|name| self.candidate(interp, "", name, library))
            {
                return Some(path);
            }
            dirs.iter().find_map(|dir| {
                names
                    .iter()
                    .find_map(|name| self.candidate(interp, dir, name, library))
            })
        } else {
            names.iter().find_map(|name| {
                if self.kind == Kind::Program
                    && name.contains('/')
                    && let Some(path) = self.candidate(interp, "", name, library)
                {
                    return Some(path);
                }
                dirs.iter()
                    .find_map(|dir| self.candidate(interp, dir, name, library))
            })
        }
    }

    fn candidate(
        &mut self,
        interp: &mut Interp,
        dir: &str,
        name: &str,
        library: Option<&LibraryNames>,
    ) -> Option<String> {
        if let Some(library) = library {
            return library.search(interp, self, dir, name);
        }
        let path = if self.kind == Kind::Program {
            collapse_full_path(name, dir)
        } else {
            format!("{dir}{name}")
        };
        let exists = if self.kind == Kind::Program {
            executable(interp, &path)
        } else {
            access(&path, libc::R_OK)
        };
        let result = if self.kind == Kind::Path { dir } else { &path };
        if exists && self.validate(interp, result) {
            return Some(result.to_owned());
        }
        self.failed(path);
        None
    }

    fn search_special(&mut self, interp: &mut Interp, dirs: &[String]) -> Option<String> {
        let names = self.names.clone();
        if self.names_per_dir && self.kind == Kind::Library {
            dirs.iter().find_map(|dir| {
                names
                    .iter()
                    .find_map(|name| self.special_candidate(interp, dir, name))
            })
        } else {
            names.iter().find_map(|name| {
                dirs.iter()
                    .find_map(|dir| self.special_candidate(interp, dir, name))
            })
        }
    }

    fn special_candidate(&mut self, interp: &mut Interp, dir: &str, name: &str) -> Option<String> {
        match self.kind {
            Kind::Program => bundle_executable(&format!("{dir}{name}.app")),
            Kind::Library => [".xcframework", ".framework"].iter().find_map(|extension| {
                let path = format!("{dir}{name}{extension}");
                (Path::new(&path).is_dir() && self.validate(interp, &path)).then_some(path)
            }),
            Kind::Path | Kind::File => {
                if let Some((framework, header)) = name.split_once('/') {
                    let framework = format!("{dir}{framework}.framework");
                    let path = format!("{framework}/Headers/{header}");
                    let result = if self.kind == Kind::Path {
                        &framework
                    } else {
                        &path
                    };
                    if access(&path, libc::R_OK) && self.validate(interp, result) {
                        return Some(result.clone());
                    }
                    self.failed(path);
                }
                let mut frameworks: Vec<_> = directory_names(dir)
                    .filter(|entry| entry.ends_with(".framework"))
                    .collect();
                frameworks.sort_unstable();
                frameworks.into_iter().find_map(|framework| {
                    let headers = format!("{dir}{framework}/Headers/");
                    let path = format!("{headers}{name}");
                    access(&path, libc::R_OK).then_some(if self.kind == Kind::Path {
                        headers
                    } else {
                        path
                    })
                })
            }
        }
    }

    fn architecture_paths(
        &mut self,
        interp: &mut Interp,
        dirs: Vec<String>,
        suffix: &str,
    ) -> Vec<String> {
        let mut out = Vec::new();
        for dir in dirs {
            let start = out.len();
            architecture_path(&dir, 0, suffix, true, &mut out);
            if self.debug.is_some() {
                for path in &out[start..] {
                    interp.issue(MessageType::Log, &format!(
                        "find_library({}) added replacement path {path} to PATH_SUFFIXES for architecture suffix '{suffix}'", self.variable));
                }
                interp.issue(MessageType::Log, &format!(
                    "find_library({}) removed original suffix {dir} from PATH_SUFFIXES while adding architecture paths for suffix '{suffix}'", self.variable));
            }
        }
        out
    }

    fn normalize_result(&self, interp: &mut Interp, old: &str) -> String {
        let untyped = interp
            .cache
            .get(&self.variable)
            .is_some_and(|entry| entry.kind == CacheType::Uninitialized);
        if interp.policy_status("CMP0125") == PolicyStatus::New {
            let absolute = normalized(old);
            let value = if !old.is_empty() && access(&absolute, libc::R_OK) {
                absolute.as_str()
            } else {
                old
            };
            if !self.store_cache {
                interp.set_def(&self.variable, value);
            } else if value != old || untyped {
                interp.cache.add(
                    &self.variable,
                    Some(value),
                    Some(&self.doc),
                    self.cache_type,
                );
                if interp.policy_status("CMP0126") != PolicyStatus::New {
                    interp.unset_def(&self.variable);
                } else if interp.get_normal_def(&self.variable).is_some() {
                    interp.set_def(&self.variable, value);
                }
            }
            value.to_owned()
        } else if self.store_cache {
            let value = interp
                .cache
                .value(&self.variable)
                .unwrap_or_default()
                .to_owned();
            if untyped {
                cache_result(
                    interp,
                    &self.variable,
                    "",
                    &self.doc,
                    self.cache_type,
                    false,
                );
                if interp.policy_status("CMP0126") == PolicyStatus::New
                    && interp.get_normal_def(&self.variable).is_some()
                {
                    let value = interp
                        .cache
                        .value(&self.variable)
                        .unwrap_or_default()
                        .to_owned();
                    interp.set_def(&self.variable, value);
                }
            }
            value
        } else {
            interp.set_def(&self.variable, old);
            old.to_owned()
        }
    }

    fn store(&self, interp: &mut Interp, path: Option<&str>) {
        let not_found = format!("{}-NOTFOUND", self.variable);
        let value = path.unwrap_or(&not_found);
        if self.store_cache {
            let cached = if matches!(self.cache_type, CacheType::Path | CacheType::Filepath) {
                convert_to_unix_slashes(value)
            } else {
                value.to_owned()
            };
            let force = interp.policy_status("CMP0125") == PolicyStatus::New;
            cache_result(
                interp,
                &self.variable,
                &cached,
                &self.doc,
                self.cache_type,
                force,
            );
            if interp.policy_status("CMP0126") == PolicyStatus::New
                && interp.get_normal_def(&self.variable).is_some()
            {
                interp.set_def(&self.variable, value);
            }
        } else {
            interp.set_def(&self.variable, value);
        }
        if path.is_none() && self.required == Some(true) {
            let label = if matches!(self.kind, Kind::File | Kind::Path) {
                "files"
            } else {
                "names"
            };
            interp.issue(
                MessageType::FatalError,
                &format!(
                    "Could not find {} using the following {label}: {}",
                    self.variable,
                    self.names.join(", ")
                ),
            );
            interp.set_fatal();
        }
    }
}

fn find(interp: &mut Interp, args: &[String], kind: Kind) -> CmdResult {
    let (mut find, args) = Find::new(interp, args, kind)?;
    if let Some(value) = interp
        .get_def(&find.variable)
        .filter(|value| !is_notfound(value))
        .map(str::to_owned)
    {
        let value = find.normalize_result(interp, &value);
        find.debug_result(interp, Some(&value));
        return Ok(());
    }
    find.parse(interp, &args)?;
    let path = find.search(interp);
    find.store(interp, path.as_deref());
    find.debug_result(interp, path.as_deref());
    Ok(())
}

fn executable(interp: &mut Interp, path: &str) -> bool {
    let policy = interp.policy_status("CMP0109");
    let readable = || readable_file(path);
    let executable = || access(path, libc::X_OK) && !Path::new(path).is_dir();
    match policy {
        PolicyStatus::Old => readable(),
        PolicyStatus::New => executable(),
        PolicyStatus::Warn => {
            let readable = readable();
            if executable() != readable {
                let info = if readable {
                    "is readable but not executable.  CMake is using it for compatibility."
                } else {
                    "is executable but not readable.  CMake is ignoring it for compatibility."
                };
                interp.issue(
                    MessageType::AuthorWarning,
                    &format!("{}\nThe file\n  {path}\n{info}", policy_warning(109)),
                );
            }
            readable
        }
    }
}

fn distinct_directory(candidate: &str, original: &str) -> bool {
    let candidate = Path::new(candidate.trim_end_matches('/'));
    let original = Path::new(original.trim_end_matches('/'));
    candidate.is_dir()
        && (!(candidate.is_symlink() || original.is_symlink())
            || candidate.canonicalize().ok() != original.canonicalize().ok())
}

fn architecture_path(dir: &str, start: usize, suffix: &str, fresh: bool, out: &mut Vec<String>) {
    if let Some(position) = dir[start..]
        .find("lib/")
        .map(|position| start + position + 3)
    {
        let (lib, rest) = dir.split_at(position);
        let alternative = format!("{lib}{suffix}");
        if distinct_directory(&alternative, lib) {
            architecture_path(
                &format!("{alternative}{rest}"),
                alternative.len() + 1,
                suffix,
                true,
                out,
            );
        }
        if Path::new(lib).is_dir() {
            architecture_path(dir, position + 1, suffix, false, out);
        }
    }
    if fresh {
        let alternative = format!("{dir}{suffix}");
        if distinct_directory(&alternative, dir) {
            out.push(trailing_slash(alternative));
        }
        if Path::new(dir).is_dir() {
            out.push(dir.to_owned());
        }
    }
}

struct LibraryNames {
    prefixes: Vec<String>,
    suffixes: Vec<String>,
    by_name: BTreeMap<String, LibraryName>,
}

struct LibraryName {
    raw: bool,
    debug: String,
    sensitive: Option<Regex>,
    insensitive: Option<Regex>,
}

impl LibraryNames {
    fn new(interp: &Interp, names: &[String]) -> Self {
        let default_suffixes = if cfg!(target_os = "macos") {
            ".tbd;.dylib;.so;.a"
        } else {
            ".so;.a"
        };
        let prefixes = expand_list(
            interp
                .get_def("CMAKE_FIND_LIBRARY_PREFIXES")
                .unwrap_or("lib"),
            true,
        );
        let suffixes = expand_list(
            interp
                .get_def("CMAKE_FIND_LIBRARY_SUFFIXES")
                .unwrap_or(default_suffixes),
            true,
        );
        let version = if interp
            .global_props
            .get("FIND_LIBRARY_USE_OPENBSD_VERSIONING")
            .is_some_and(is_on)
        {
            r"(\.[0-9]+\.[0-9]+)?"
        } else {
            ""
        };
        let group = |list: &[String]| {
            format!(
                "({})",
                list.iter()
                    .map(|item| regex::escape(item))
                    .collect::<Vec<_>>()
                    .join("|")
            )
        };
        let prefix = group(&prefixes);
        let suffix = group(&suffixes);
        let debug_prefix = if prefixes.len() == 1 {
            &prefixes[0]
        } else {
            &prefix
        };
        let debug_suffix = if suffixes.len() == 1 {
            &suffixes[0]
        } else {
            &suffix
        };
        let by_name = names
            .iter()
            .map(|name| {
                let raw = suffixes.iter().any(|suffix| {
                    name.len() > suffix.len()
                        && (name.ends_with(suffix) || name.contains(&format!("{suffix}.")))
                });
                let pattern = format!("^{prefix}{}{suffix}{version}$", regex::escape(name));
                let entry = LibraryName {
                    raw,
                    debug: format!("{debug_prefix}{name}{debug_suffix}"),
                    sensitive: Regex::new(&pattern).ok(),
                    insensitive: Regex::new(&pattern.to_ascii_lowercase()).ok(),
                };
                (name.clone(), entry)
            })
            .collect();
        Self {
            prefixes,
            suffixes,
            by_name,
        }
    }

    fn search(
        &self,
        interp: &mut Interp,
        find: &mut Find,
        dir: &str,
        name: &str,
    ) -> Option<String> {
        let pattern = &self.by_name[name];
        if pattern.raw {
            let path = format!("{dir}{name}");
            if readable_file(&path) {
                let path = normalized(&path);
                if find.validate(interp, &path) {
                    return Some(path);
                }
            }
            if find.debug.is_some() {
                find.failed(format!("{dir}/{}", pattern.debug));
            }
        }
        let insensitive = case_insensitive(dir);
        let regex = if insensitive {
            &pattern.insensitive
        } else {
            &pattern.sensitive
        }
        .as_ref()?;
        let mut entries: Vec<_> = directory_names(dir).collect();
        entries.sort_unstable();
        let mut best = None;
        for entry in entries {
            let folded;
            let test_name = if insensitive {
                folded = entry.to_ascii_lowercase();
                &folded
            } else {
                &entry
            };
            let Some(captures) = regex.captures(test_name) else {
                continue;
            };
            let path = format!("{dir}{entry}");
            if !readable_file(&path) {
                continue;
            }
            let path = normalized(&path);
            if !find.validate(interp, &path) {
                continue;
            }
            let index = |items: &[String], name: &str| {
                items
                    .iter()
                    .position(|item| item == name)
                    .unwrap_or(items.len())
            };
            let prefix = index(&self.prefixes, &captures[1]);
            let suffix = index(&self.suffixes, &captures[2]);
            let version = captures
                .get(3)
                .and_then(|version| version.as_str()[1..].split_once('.'))
                .map(|(major, minor)| {
                    (
                        major.parse::<u32>().unwrap_or(0),
                        minor.parse::<u32>().unwrap_or(0),
                    )
                })
                .unwrap_or_default();
            let rank = (prefix, suffix, std::cmp::Reverse(version));
            if best.as_ref().is_none_or(|(best, _)| rank < *best) {
                best = Some((rank, path));
            }
        }
        if best.is_none() && find.debug.is_some() {
            find.failed(format!("{dir}/{}", pattern.debug));
        }
        best.map(|(_, path)| path)
    }
}

#[cfg(target_os = "linux")]
fn case_insensitive(dir: &str) -> bool {
    use std::os::fd::AsRawFd;

    const FS_CASEFOLD_FL: libc::c_int = 0x40000000;
    Path::new(dir).is_dir()
        && std::fs::File::open(dir).is_ok_and(|file| {
            let mut flags: libc::c_int = 0;
            // FS_IOC_GETFLAGS reads the inode flags into a writable int.
            let status =
                unsafe { libc::ioctl(file.as_raw_fd(), libc::FS_IOC_GETFLAGS, &mut flags) };
            status == 0 && flags & FS_CASEFOLD_FL != 0
        })
}

#[cfg(not(target_os = "linux"))]
fn case_insensitive(dir: &str) -> bool {
    cfg!(any(target_os = "macos", target_os = "windows")) && Path::new(dir).is_dir()
}

#[cfg(not(target_os = "macos"))]
fn bundle_executable(_: &str) -> Option<String> {
    None
}

#[cfg(target_os = "macos")]
fn bundle_executable(path: &str) -> Option<String> {
    use std::ffi::{CStr, c_void};
    type CfRef = *const c_void;
    #[link(name = "CoreFoundation", kind = "framework")]
    unsafe extern "C" {
        fn CFURLCreateFromFileSystemRepresentation(
            allocator: CfRef,
            bytes: *const u8,
            length: isize,
            directory: u8,
        ) -> CfRef;
        fn CFBundleCreate(allocator: CfRef, url: CfRef) -> CfRef;
        fn CFBundleCopyExecutableURL(bundle: CfRef) -> CfRef;
        fn CFURLGetFileSystemRepresentation(
            url: CfRef,
            resolve: u8,
            bytes: *mut u8,
            length: isize,
        ) -> u8;
        fn CFRelease(object: CfRef);
    }
    let allocator = std::ptr::null();
    let mut buffer = [0u8; 1024];
    // Every Create/Copy reference is released after its last use.
    unsafe {
        let url = CFURLCreateFromFileSystemRepresentation(
            allocator,
            path.as_ptr(),
            path.len() as isize,
            1,
        );
        if url.is_null() {
            return None;
        }
        let bundle = CFBundleCreate(allocator, url);
        CFRelease(url);
        if bundle.is_null() {
            return None;
        }
        let executable = CFBundleCopyExecutableURL(bundle);
        CFRelease(bundle);
        if executable.is_null() {
            return None;
        }
        let found = CFURLGetFileSystemRepresentation(
            executable,
            1,
            buffer.as_mut_ptr(),
            buffer.len() as isize,
        );
        CFRelease(executable);
        (found != 0).then(|| normalized(&CStr::from_ptr(buffer.as_ptr().cast()).to_string_lossy()))
    }
}
