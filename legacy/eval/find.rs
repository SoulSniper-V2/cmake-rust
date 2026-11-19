use std::env;
use std::fs::Metadata;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;

use super::{Flow, Interpreter, is_false, need, path_string};
use crate::syntax::split_list;
use crate::{Result, fail};

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum FindKind {
    Program,
    Library,
    File,
    Path,
}

#[derive(Clone, Copy)]
enum Section {
    Names,
    Hints,
    Paths,
    Suffixes,
}

impl Interpreter {
    pub(super) fn find_item(&mut self, kind: FindKind, args: &[String]) -> Result<()> {
        need(args, 2)?;
        let variable = &args[0];
        let mut names = Vec::new();
        let mut hints = Vec::new();
        let mut paths = Vec::new();
        let mut suffixes = Vec::new();
        let mut exclude = Vec::new();
        let mut section = Section::Names;
        let mut names_per_dir = kind == FindKind::Path || kind == FindKind::File;
        let mut required = false;
        let mut no_cache = false;
        let mut doc = "Path found by the find command.";
        let mut validator = None;
        let mut tail = args[1..].iter();
        while let Some(arg) = tail.next() {
            match arg.as_str() {
                "NAMES" => section = Section::Names,
                "HINTS" => section = Section::Hints,
                "PATHS" => section = Section::Paths,
                "PATH_SUFFIXES" => section = Section::Suffixes,
                "DOC" => doc = tail.next().ok_or("DOC requires a help string")?,
                "VALIDATOR" => {
                    validator = Some(tail.next().ok_or("VALIDATOR requires a function")?)
                }
                "NO_CACHE" => no_cache = true,
                "REQUIRED" => required = true,
                "NAMES_PER_DIR" if matches!(kind, FindKind::Library | FindKind::Program) => {
                    names_per_dir = true
                }
                "NO_DEFAULT_PATH"
                | "NO_PACKAGE_ROOT_PATH"
                | "NO_CMAKE_PATH"
                | "NO_CMAKE_ENVIRONMENT_PATH"
                | "NO_SYSTEM_ENVIRONMENT_PATH"
                | "NO_CMAKE_SYSTEM_PATH"
                | "NO_CMAKE_INSTALL_PREFIX" => exclude.push(arg.as_str()),
                "REGISTRY_VIEW"
                | "NO_CMAKE_FIND_ROOT_PATH"
                | "ONLY_CMAKE_FIND_ROOT_PATH"
                | "CMAKE_FIND_ROOT_PATH_BOTH"
                | "OPTIONAL" => return fail(format!("unsupported find option {arg}")),
                _ => match section {
                    Section::Names => names.push(arg.as_str()),
                    Section::Suffixes => suffixes.push(arg.as_str()),
                    Section::Hints | Section::Paths => {
                        let results = if matches!(section, Section::Hints) {
                            &mut hints
                        } else {
                            &mut paths
                        };
                        if arg == "ENV" {
                            let key = tail.next().ok_or("ENV requires a variable name")?;
                            results.extend(env::var_os(key).iter().flat_map(env::split_paths));
                        } else if !arg.is_empty() {
                            results.push(PathBuf::from(arg));
                        }
                    }
                },
            }
        }
        let callable = if let Some(validator) = validator {
            let callable = self
                .functions
                .get(&validator.to_ascii_lowercase())
                .filter(|callable| callable.scoped)
                .ok_or_else(|| format!("find VALIDATOR {validator:?} must name a function"))?;
            Some(callable.clone())
        } else {
            None
        };
        if self
            .get(variable)
            .is_some_and(|value| value != "NOTFOUND" && !value.ends_with("-NOTFOUND"))
        {
            return Ok(());
        }

        let search = |flag: &str, variable: &str| {
            !exclude.contains(&"NO_DEFAULT_PATH")
                && !exclude.contains(&flag)
                && self.get(variable).is_none_or(|value| !is_false(&value))
        };
        let prefix_paths = |prefixes: Vec<PathBuf>| self.expand_find_prefixes(kind, prefixes);
        let kind_path_var = match kind {
            FindKind::Program => "PROGRAM",
            FindKind::Library => "LIBRARY",
            FindKind::File | FindKind::Path => "INCLUDE",
        };
        let mut directories = Vec::new();
        if search("NO_PACKAGE_ROOT_PATH", "CMAKE_FIND_USE_PACKAGE_ROOT_PATH") {
            if let Some(package) = self.get("CMAKE_FIND_PACKAGE_NAME") {
                let root = format!("{package}_ROOT");
                let mut prefixes = self.find_path_list(&root);
                prefixes.extend(env::var_os(root).iter().flat_map(env::split_paths));
                directories.extend(prefix_paths(prefixes));
            }
        }
        let path_var = format!("CMAKE_{kind_path_var}_PATH");
        if search("NO_CMAKE_PATH", "CMAKE_FIND_USE_CMAKE_PATH") {
            directories.extend(prefix_paths(self.find_path_list("CMAKE_PREFIX_PATH")));
            directories.extend(self.find_path_list(&path_var));
        }
        if search(
            "NO_CMAKE_ENVIRONMENT_PATH",
            "CMAKE_FIND_USE_CMAKE_ENVIRONMENT_PATH",
        ) {
            directories.extend(prefix_paths(
                env::var_os("CMAKE_PREFIX_PATH")
                    .iter()
                    .flat_map(env::split_paths)
                    .collect(),
            ));
            directories.extend(env::var_os(path_var).iter().flat_map(env::split_paths));
        }
        directories.extend(hints);
        if search(
            "NO_SYSTEM_ENVIRONMENT_PATH",
            "CMAKE_FIND_USE_SYSTEM_ENVIRONMENT_PATH",
        ) {
            if kind != FindKind::Program {
                let variable = if kind == FindKind::Library {
                    "LIB"
                } else {
                    "INCLUDE"
                };
                directories.extend(env::var_os(variable).iter().flat_map(env::split_paths));
            }
            directories.extend(env::var_os("PATH").iter().flat_map(env::split_paths));
        }
        if search("NO_CMAKE_SYSTEM_PATH", "CMAKE_FIND_USE_CMAKE_SYSTEM_PATH") {
            let mut prefixes = self
                .get("CMAKE_SYSTEM_PREFIX_PATH")
                .map(|_| self.find_path_list("CMAKE_SYSTEM_PREFIX_PATH"))
                .unwrap_or_else(|| {
                    ["/usr/local", "/usr", "/", "/opt"]
                        .into_iter()
                        .map(PathBuf::from)
                        .collect()
                });
            if search("NO_CMAKE_INSTALL_PREFIX", "CMAKE_FIND_USE_INSTALL_PREFIX") {
                let install = self.get("CMAKE_INSTALL_PREFIX").unwrap_or_default();
                if !install.is_empty() {
                    prefixes.push(install.into());
                }
            }
            directories.extend(prefix_paths(prefixes));
            directories.extend(self.find_path_list(&format!("CMAKE_SYSTEM_{kind_path_var}_PATH")));
        }
        directories.extend(paths);
        let directories = directories
            .into_iter()
            .flat_map(|path| {
                let path = self.directory().source.join(path);
                let nested = suffixes
                    .iter()
                    .map(|suffix| path.join(suffix))
                    .collect::<Vec<_>>();
                nested.into_iter().chain(std::iter::once(path))
            })
            .collect::<Vec<_>>();
        let variants: Vec<Vec<PathBuf>> = names
            .into_iter()
            .map(|name| self.find_names(kind, name))
            .collect();
        let mut candidates = Vec::new();
        if names_per_dir {
            for directory in &directories {
                candidates.extend(variants.iter().flatten().map(|name| directory.join(name)));
            }
        } else {
            for names in &variants {
                for directory in &directories {
                    candidates.extend(names.iter().map(|name| directory.join(name)));
                }
            }
        }

        let mut visited = std::collections::BTreeSet::new();
        for candidate in candidates {
            if !visited.insert(candidate.clone())
                || !candidate
                    .metadata()
                    .is_ok_and(|metadata| valid(&metadata, kind))
            {
                continue;
            }
            let found = if kind == FindKind::Path {
                candidate.parent().ok_or("found file has no directory")?
            } else {
                &candidate
            };
            let value = path_string(found);
            if let Some(callable) = &callable {
                self.push_scope();
                let accept = "CMAKE_RUST_FIND_ACCEPT";
                self.set(accept, "TRUE");
                let result = self.invoke(callable, &[accept.into(), value.clone()]);
                let accepted = self.get(accept).is_some_and(|value| !is_false(&value));
                self.scopes.pop();
                if result? != Flow::Continue || !accepted {
                    continue;
                }
            }
            self.save_find_result(kind, variable, value, doc, no_cache);
            return Ok(());
        }
        if required {
            return fail(format!("could not find {variable}"));
        }
        self.save_find_result(
            kind,
            variable,
            format!("{variable}-NOTFOUND"),
            doc,
            no_cache,
        );
        Ok(())
    }

    fn find_path_list(&self, variable: &str) -> Vec<PathBuf> {
        self.get(variable).map_or_else(Vec::new, |value| {
            split_list(&value)
                .into_iter()
                .filter(|value| !value.is_empty())
                .map(PathBuf::from)
                .collect()
        })
    }

    fn expand_find_prefixes(&self, kind: FindKind, prefixes: Vec<PathBuf>) -> Vec<PathBuf> {
        let arch = self
            .get("CMAKE_LIBRARY_ARCHITECTURE")
            .filter(|value| !value.is_empty());
        let mut output = Vec::new();
        for prefix in prefixes {
            match kind {
                FindKind::Program => {
                    output.push(prefix.join("bin"));
                    output.push(prefix.join("sbin"));
                }
                FindKind::Library => {
                    if let Some(arch) = &arch {
                        output.push(prefix.join("lib").join(arch));
                    }
                    output.push(prefix.join("lib"));
                }
                FindKind::File | FindKind::Path => {
                    if let Some(arch) = &arch {
                        output.push(prefix.join("include").join(arch));
                    }
                    output.push(prefix.join("include"));
                }
            }
            output.push(prefix);
        }
        output
    }

    fn find_names(&self, kind: FindKind, name: &str) -> Vec<PathBuf> {
        if kind != FindKind::Library || name.contains('/') {
            return vec![name.into()];
        }
        let prefixes = self
            .get("CMAKE_FIND_LIBRARY_PREFIXES")
            .unwrap_or_else(|| "lib".into());
        let defaults = if cfg!(target_os = "macos") {
            ".dylib;.so;.a"
        } else {
            ".so;.a"
        };
        let suffixes = self
            .get("CMAKE_FIND_LIBRARY_SUFFIXES")
            .unwrap_or_else(|| defaults.into());
        let mut names = Vec::new();
        if split_list(&suffixes)
            .iter()
            .any(|suffix| !suffix.is_empty() && name.ends_with(suffix))
        {
            names.push(name.into());
        }
        for prefix in split_list(&prefixes) {
            for suffix in split_list(&suffixes) {
                names.push(PathBuf::from(format!("{prefix}{name}{suffix}")));
            }
        }
        names
    }

    fn save_find_result(
        &mut self,
        kind: FindKind,
        variable: &str,
        value: String,
        doc: &str,
        no_cache: bool,
    ) {
        if no_cache {
            self.set(variable, value);
        } else {
            let kind = if kind == FindKind::Path {
                "PATH"
            } else {
                "FILEPATH"
            };
            self.cache_set(variable, value, kind, doc);
            self.variables_mut().remove(variable);
        }
    }
}

fn valid(metadata: &Metadata, kind: FindKind) -> bool {
    metadata.is_file() && (kind != FindKind::Program || metadata.permissions().mode() & 0o111 != 0)
}
