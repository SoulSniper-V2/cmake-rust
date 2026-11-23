//! Search modes and path sources shared by the find commands.

use std::collections::HashSet;
use std::env;
use std::ffi::CString;
use std::fs;
use std::path::Path;
use std::sync::LazyLock;

use regex::Regex;

use crate::cache::{CacheType, convert_to_unix_slashes};
use crate::interp::{CmdErr, Interp, err};
use crate::util::{
    collapse_full_path, expand_list, is_off, is_on, is_subdirectory, join_list, strtol,
};

pub(super) const SOURCES: [&str; 6] = [
    "PACKAGE_ROOT_PATH",
    "CMAKE_PATH",
    "CMAKE_ENVIRONMENT_PATH",
    "SYSTEM_ENVIRONMENT_PATH",
    "CMAKE_SYSTEM_PATH",
    "INSTALL_PREFIX",
];
pub(super) const ROOT: usize = 0;
pub(super) const CMAKE: usize = 1;
pub(super) const CMAKE_ENV: usize = 2;
pub(super) const SYSTEM_ENV: usize = 3;
pub(super) const SYSTEM: usize = 4;
pub(super) const INSTALL: usize = 5;

#[derive(Clone, Copy, PartialEq, Eq)]
enum RootMode {
    Both,
    Only,
    Never,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum MacMode {
    First,
    Last,
    Only,
    Never,
}

impl MacMode {
    fn from_def(interp: &Interp, name: &str) -> Self {
        match interp.get_def(name) {
            Some("FIRST") => Self::First,
            Some("LAST") => Self::Last,
            Some("ONLY") => Self::Only,
            Some("NEVER") => Self::Never,
            _ if cfg!(target_os = "macos") => Self::First,
            _ => Self::Never,
        }
    }
}

#[derive(Clone, Copy)]
pub(super) struct Options {
    pub(super) no_default: bool,
    pub(super) enabled: [bool; SOURCES.len()],
    root_mode: RootMode,
    pub(super) framework: MacMode,
    pub(super) bundle: MacMode,
}

impl Options {
    pub(super) fn new(interp: &Interp, kind: &str) -> Self {
        Self {
            no_default: false,
            enabled: SOURCES.map(|source| {
                interp
                    .get_def(&format!("CMAKE_FIND_USE_{source}"))
                    .is_none_or(is_on)
            }),
            root_mode: match interp.get_def(&format!("CMAKE_FIND_ROOT_PATH_MODE_{kind}")) {
                Some("NEVER") => RootMode::Never,
                Some("ONLY") => RootMode::Only,
                _ => RootMode::Both,
            },
            framework: MacMode::from_def(interp, "CMAKE_FIND_FRAMEWORK"),
            bundle: MacMode::from_def(interp, "CMAKE_FIND_APPBUNDLE"),
        }
    }

    /// `CMAKE_FIND_ROOT_PATH_MODE` as the configure log spells it.
    pub(super) fn root_mode_name(&self) -> &'static str {
        match self.root_mode {
            RootMode::Never => "NEVER",
            RootMode::Only => "ONLY",
            RootMode::Both => "BOTH",
        }
    }

    pub(super) fn uses(&self, source: usize) -> bool {
        !self.no_default && self.enabled[source]
    }

    pub(super) fn parse(&mut self, arg: &str) -> bool {
        match arg {
            "NO_DEFAULT_PATH" => self.no_default = true,
            "NO_CMAKE_INSTALL_PREFIX" => self.enabled[INSTALL] = false,
            "NO_CMAKE_FIND_ROOT_PATH" => self.root_mode = RootMode::Never,
            "ONLY_CMAKE_FIND_ROOT_PATH" => self.root_mode = RootMode::Only,
            "CMAKE_FIND_ROOT_PATH_BOTH" => self.root_mode = RootMode::Both,
            _ => {
                let Some(source) = arg.strip_prefix("NO_") else {
                    return false;
                };
                let Some(index) = SOURCES[..INSTALL].iter().position(|name| *name == source) else {
                    return false;
                };
                self.enabled[index] = false;
            }
        }
        true
    }

    pub(super) fn reroot(
        &self,
        interp: &Interp,
        paths: Vec<String>,
        mut debug: Option<&mut String>,
    ) -> Vec<String> {
        let vars = [
            "CMAKE_FIND_ROOT_PATH",
            "CMAKE_SYSROOT_COMPILE",
            "CMAKE_SYSROOT_LINK",
            "CMAKE_SYSROOT",
        ];
        if self.root_mode == RootMode::Never
            || vars.iter().all(|name| interp.get_safe(name).is_empty())
        {
            return paths;
        }
        let mut roots = list(interp, vars[0]);
        roots.extend(
            vars[1..]
                .iter()
                .map(|name| interp.get_safe(name))
                .filter(|path| !path.is_empty()),
        );
        if let Some(debug) = debug.as_mut() {
            debug.push_str("Prepending the following roots to each prefix:\n");
            for name in vars {
                debug_paths(debug, name, &list(interp, name));
            }
        }
        let staging = interp.get_def("CMAKE_STAGING_PREFIX");
        let mut result = Vec::new();
        for root in roots {
            let root = convert_to_unix_slashes(&root);
            for path in &paths {
                if within(path, &root) || staging.is_some_and(|stage| within(path, stage)) {
                    result.push(path.clone());
                } else if path.is_empty() || path.starts_with('~') {
                    result.push(String::new());
                } else if path.trim_matches('/').is_empty() {
                    result.push(root.clone());
                } else {
                    result.push(format!("{root}/{}", path.trim_start_matches('/')));
                }
            }
        }
        if self.root_mode == RootMode::Both {
            result.extend(paths);
        }
        result
    }

    pub(super) fn system_prefixes(&self, interp: &Interp, package: bool) -> Vec<String> {
        let mut paths = list(interp, "CMAKE_SYSTEM_PREFIX_PATH");
        let in_list = !interp.is_on("CMAKE_FIND_NO_INSTALL_PREFIX");
        let mut raw = false;
        if !self.enabled[INSTALL] && in_list {
            let prefixes = if package {
                &["INSTALL"][..]
            } else {
                &["INSTALL", "STAGING"][..]
            };
            for prefix in prefixes {
                let base = format!("_CMAKE_SYSTEM_PREFIX_PATH_{prefix}_PREFIX");
                let count = strtol(&interp.get_safe(&format!("{base}_COUNT")), 10).0;
                let value = interp.get_safe(&format!("{base}_VALUE"));
                if count > 0 && !value.is_empty() {
                    raw = package;
                    if let Some(index) = paths
                        .iter()
                        .enumerate()
                        .filter(|(_, path)| **path == value)
                        .nth(count as usize - 1)
                        .map(|(index, _)| index)
                    {
                        paths.remove(index);
                    }
                }
            }
        } else if self.enabled[INSTALL]
            && !in_list
            && interp.is_def_set("CMAKE_FIND_USE_INSTALL_PREFIX")
        {
            let mut prefix = list(interp, "CMAKE_INSTALL_PREFIX");
            if !package {
                prefix.extend(list(interp, "CMAKE_STAGING_PREFIX"));
            }
            prefix.append(&mut paths);
            paths = prefix;
        }
        if !raw {
            for path in &mut paths {
                *path = source_path(interp, path);
            }
        }
        paths
    }
}

#[derive(Default)]
pub(super) struct SearchPaths {
    emitted: HashSet<(String, String)>,
    paths: Vec<(String, String)>,
    group_start: usize,
}

impl SearchPaths {
    pub(super) fn add(&mut self, path: String, prefix: &str) {
        if !path.is_empty() {
            let entry = (path, prefix.to_owned());
            if self.emitted.insert(entry.clone()) {
                self.paths.push(entry);
            }
        }
    }

    pub(super) fn extend(&mut self, paths: impl IntoIterator<Item = String>) {
        for path in paths {
            self.add(path, "");
        }
    }

    pub(super) fn finish_group(&mut self, suffixes: &[String]) {
        let group: Vec<_> = self.paths.drain(self.group_start..).collect();
        for (path, prefix) in group {
            let path = convert_to_unix_slashes(&path);
            let prefix = convert_to_unix_slashes(&prefix);
            for suffix in suffixes {
                self.paths.push((
                    format!("{}{suffix}", trailing_slash(path.clone())),
                    prefix.clone(),
                ));
            }
            self.paths.push((path, prefix));
        }
        self.group_start = self.paths.len();
    }

    pub(super) fn since(&self, start: usize) -> Vec<String> {
        self.paths[start..]
            .iter()
            .map(|entry| entry.0.clone())
            .collect()
    }

    pub(super) fn len(&self) -> usize {
        self.paths.len()
    }

    pub(super) fn into_paths(self, ignore: &[String], ignore_prefix: &[String]) -> Vec<String> {
        self.paths
            .into_iter()
            .filter(|(path, prefix)| !ignore.contains(path) && !ignore_prefix.contains(prefix))
            .map(|(path, _)| path)
            .collect()
    }
}

pub(super) fn debug_paths(debug: &mut String, label: &str, paths: &[String]) {
    debug.push_str(label);
    debug.push('\n');
    if paths.is_empty() {
        debug.push_str("  none\n");
    }
    for path in paths {
        debug.push_str("  ");
        debug.push_str(path);
        debug.push('\n');
    }
}

pub(super) fn list(interp: &Interp, name: &str) -> Vec<String> {
    expand_list(&interp.get_safe(name), false)
}

pub(super) fn cmake_paths(interp: &Interp, name: &str) -> Vec<String> {
    list(interp, name)
        .iter()
        .map(|path| source_path(interp, path))
        .collect()
}

pub(super) fn source_path(interp: &Interp, path: &str) -> String {
    collapse_full_path(&convert_to_unix_slashes(path), &interp.current_source_dir())
}

pub(super) fn env_paths(name: &str) -> Vec<String> {
    env::var(name).map_or_else(|_| Vec::new(), |value| split_env_paths(&value))
}

pub(super) fn split_env_paths(value: &str) -> Vec<String> {
    env::split_paths(value)
        .filter(|path| !path.as_os_str().is_empty())
        .map(|path| normalized(&path.to_string_lossy()))
        .collect()
}

pub(super) fn normalized(path: &str) -> String {
    collapse_full_path(path, "")
}

pub(super) fn user_paths(interp: &Interp, path: &str) -> Vec<String> {
    static REGISTRY: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(
        r"\[(?:\{.+\})?(?:HKCU|HKEY_CURRENT_USER|HKLM|HKEY_LOCAL_MACHINE|HKCR|HKEY_CLASSES_ROOT|HKCC|HKEY_CURRENT_CONFIG|HKU|HKEY_USERS)[^\]]*\]"
    ).unwrap()
    });
    if REGISTRY.is_match(path) {
        return Vec::new();
    }
    let mut expanded = Vec::new();
    glob_dirs(path, &mut expanded);
    expanded
        .iter()
        .map(|path| source_path(interp, path))
        .collect()
}

fn glob_dirs(path: &str, out: &mut Vec<String>) {
    if let Some((base, rest)) = path.split_once("/*") {
        for name in directory_names(base) {
            let dir = format!("{base}/{name}");
            if Path::new(&dir).is_dir() {
                glob_dirs(&format!("{dir}{rest}"), out);
            }
        }
    } else {
        out.push(path.to_owned());
    }
}

pub(super) fn directory_names(dir: &str) -> impl Iterator<Item = String> {
    fs::read_dir(dir)
        .into_iter()
        .flatten()
        .filter_map(|entry| entry.ok()?.file_name().into_string().ok())
}

pub(super) fn ignored_paths(interp: &Interp, prefix: bool) -> Vec<String> {
    let tag = if prefix { "_PREFIX" } else { "" };
    ["CMAKE_SYSTEM", "CMAKE"]
        .into_iter()
        .flat_map(|base| list(interp, &format!("{base}_IGNORE{tag}_PATH")))
        .map(|path| convert_to_unix_slashes(&path))
        .collect()
}

pub(super) fn suffix(arg: &str) -> Option<String> {
    let arg = arg.strip_prefix('/').unwrap_or(arg);
    let arg = arg.strip_suffix('/').unwrap_or(arg);
    (!arg.is_empty()).then(|| arg.to_owned())
}

pub(super) fn trailing_slash(mut value: String) -> String {
    if !value.is_empty() && !value.ends_with('/') {
        value.push('/');
    }
    value
}

fn within(path: &str, root: &str) -> bool {
    path == root
        || is_subdirectory(path, root)
        || fs::canonicalize(path)
            .ok()
            .is_some_and(|path| fs::canonicalize(root).ok() == Some(path))
}

pub(super) fn registry_view(value: Option<&String>) -> Result<String, CmdErr> {
    let value = value.ok_or("missing required argument for REGISTRY_VIEW")?;
    match value.as_str() {
        "64" | "32" | "64_32" | "32_64" | "HOST" | "TARGET" | "BOTH" => Ok(value.clone()),
        _ => err(format!("given invalid value for REGISTRY_VIEW: {value}")),
    }
}

pub(super) fn access(path: &str, mode: libc::c_int) -> bool {
    CString::new(path).is_ok_and(|path| {
        // The CString remains alive for the duration of the access check.
        unsafe { libc::access(path.as_ptr(), mode) == 0 }
    })
}

pub(super) fn readable_file(path: &str) -> bool {
    access(path, libc::R_OK) && !Path::new(path).is_dir()
}

pub(super) fn lib_path_variants(interp: &Interp) -> impl Iterator<Item = &'static str> + use<> {
    let x32 = interp.get_def("CMAKE_INTERNAL_PLATFORM_ABI") == Some("ELF X32");
    let pointer = strtol(&interp.get_safe("CMAKE_SIZEOF_VOID_P"), 10).0;
    [
        ("32", pointer == 4 && !x32),
        ("64", pointer == 8),
        ("x32", x32),
    ]
    .into_iter()
    .filter_map(move |(bits, enabled)| {
        (enabled
            && interp
                .global_props
                .get(&format!(
                    "FIND_LIBRARY_USE_LIB{}_PATHS",
                    bits.to_ascii_uppercase()
                ))
                .is_some_and(is_on))
        .then_some(bits)
    })
    .collect::<Vec<_>>()
    .into_iter()
}

/// cmMakefile assigns metadata to untyped -D paths before normal-variable
/// policy handling. Values recognized as OFF, including NOTFOUND, stay literal.
pub(super) fn cache_result(
    interp: &mut Interp,
    name: &str,
    value: &str,
    doc: &str,
    kind: CacheType,
    force: bool,
) {
    let mut value = value.to_owned();
    if matches!(kind, CacheType::Path | CacheType::Filepath)
        && let Some(previous) = interp
            .cache
            .get(name)
            .filter(|entry| entry.kind == CacheType::Uninitialized)
    {
        let paths = expand_list(if force { &value } else { &previous.value }, false)
            .into_iter()
            .map(|path| {
                if is_off(&path) {
                    path
                } else {
                    normalized(&path)
                }
            })
            .collect::<Vec<_>>();
        value = join_list(&paths);
        interp.cache.add(name, Some(&value), Some(doc), kind);
    }
    interp.add_cache_def(name, Some(&value), Some(doc), kind, force);
}
