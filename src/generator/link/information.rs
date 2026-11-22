//! Translate graph entries into linker items, paths and implicit Ninja inputs.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::Path;

use super::dependencies::{self, Entry, Kind, Ordered};
use super::directories::{Directories, elf_soname};
use super::*;
use crate::outconv::{FLAG_ALLOW_MAKE_VARIABLES, NINJA_UNIX, escape_for_shell};
use crate::util::{collapse_full_path, filename_name, filename_path, is_subdirectory};

pub(super) fn compute(query: &Query<'_, '_>, head: &Target) -> LinkLine {
    if !matches!(
        head.kind,
        TargetKind::Executable | TargetKind::SharedLibrary | TargetKind::ModuleLibrary
    ) {
        return LinkLine::default();
    }
    if query.language.is_empty() {
        // `cmSystemTools::Error`: bare single-line diagnostic.
        query.ctx.system_error(format!(
            "CMake can not determine linker language for target: {}",
            head.name
        ));
        return LinkLine::default();
    }
    let ordered = dependencies::compute(query, head);
    super::super::compatible::check_declarations(
        query.ctx,
        head,
        query.config,
        ordered.entries.iter().filter_map(|entry| entry.item.target),
    );
    if query.ctx.has_error() {
        return LinkLine::default();
    }
    let users = usage_property(query, head, "LINK_DIRECTORIES");
    let mut info = Information::new(query, head, users);
    info.emit_entries(&ordered.entries);
    let end_static = info.start_static || on(head, "LINK_SEARCH_END_STATIC");
    info.set_static(end_static);
    info.add_implicit(&ordered.languages);

    let output_file = query
        .generator
        .artifact(query.ctx, head, query.config, false);
    let mut artifacts = HashSet::new();
    for entry in &ordered.entries {
        if let Some(target) = entry
            .item
            .target
            .filter(|target| !target.imported && target.kind == TargetKind::SharedLibrary)
        {
            artifacts.insert(
                query
                    .generator
                    .artifact(query.ctx, target, query.config, false),
            );
            artifacts.insert(
                query
                    .generator
                    .artifact(query.ctx, target, query.config, true),
            );
            if let Some(soname) = query.generator.soname(query.ctx, target, query.config) {
                let path = query
                    .generator
                    .artifact(query.ctx, target, query.config, false);
                artifacts.insert(format!("{}/{soname}", filename_path(&path)));
            }
        }
    }

    let link_dirs = info.link_dirs.ordered(&info.implicit_dirs, &artifacts);
    let runtime_dirs = info.runtime_dirs.ordered(&info.implicit_dirs, &artifacts);
    let dependent_dirs = info.dependent_dirs.ordered(&info.implicit_dirs, &artifacts);
    let (runtime, rpath_change) = info.rpath(&ordered, &runtime_dirs, &filename_path(&output_file));
    let mut libraries = Vec::new();
    let mut lib_fragments = Vec::new();
    if !runtime.is_empty() {
        lib_fragments.push((runtime.clone(), Vec::new()));
        libraries.push(runtime);
    }
    lib_fragments.extend(std::mem::take(&mut info.fragments));
    libraries.append(&mut info.items);
    let rpath_link = if dependent_dirs.is_empty() || info.rpath_link_flag.is_empty() {
        String::new()
    } else {
        format!(
            "{}{} ",
            info.rpath_link_flag,
            shell(&dependent_dirs.join(":"))
        )
    };
    let standard = info.langvar("STANDARD_LIBRARIES");
    let extra = format!("{rpath_link}{standard}");
    if !extra.is_empty() {
        let trimmed = extra.trim_end().to_owned();
        lib_fragments.push((trimmed.clone(), Vec::new()));
        libraries.push(trimmed);
    }
    let path_flag = info.lookup(
        &format!("CMAKE_{}_LIBRARY_PATH_FLAG", query.language),
        "CMAKE_LIBRARY_PATH_FLAG",
    );
    let path_end = info.lookup(
        &format!("CMAKE_{}_LIBRARY_PATH_TERMINATOR", query.language),
        "CMAKE_LIBRARY_PATH_TERMINATOR",
    );
    let link_path = link_dirs
        .iter()
        .cloned()
        .chain(expand_list(
            &info.langvar("STANDARD_LINK_DIRECTORIES"),
            false,
        ))
        .map(|dir| format!("{path_flag}{}{path_end}", shell(&dir)))
        .collect::<Vec<_>>()
        .join("   ");

    info.depends
        .extend(usage_property(query, head, "LINK_DEPENDS"));
    let link_what_you_use = info.link_what_you_use();
    let shared_symlinks = info
        .shared_symlinks
        .into_iter()
        .filter(|file| {
            !info
                .depends
                .iter()
                .any(|dep| query.ninja_path(file) == query.ninja_path(dep))
        })
        .collect();
    LinkLine {
        libraries: libraries.join("  ").trim().to_owned(),
        link_path,
        depends: info.depends,
        lib_fragments,
        link_dirs: link_dirs.clone(),
        shared_symlinks,
        rpath_change,
        link_what_you_use,
    }
}

#[cfg(test)]
pub(super) fn objects(query: &Query<'_, '_>, head: &Target) -> Vec<String> {
    let direct = implementation(query, head);
    let targets = direct
        .into_iter()
        .filter_map(|item| item.target)
        .filter(|target| target.kind == TargetKind::ObjectLibrary);
    unique(targets.flat_map(|target| {
        if target.imported {
            return expand_list(
                import_property(target, query.config, "IMPORTED_OBJECTS").unwrap_or_default(),
                false,
            );
        }
        let dir = query.generator.object_dir(query.ctx, target, query.config);
        query
            .generator
            .sources(query.ctx, target, query.config)
            .into_iter()
            .filter(|source| source.kind == target::SourceKind::ObjectSource)
            .map(|source| format!("{}/{}", dir.trim_end_matches('/'), source.object_name))
            .collect()
    }))
}

fn usage_property(query: &Query<'_, '_>, head: &Target, property: &str) -> Vec<String> {
    fn entries(
        query: &Query<'_, '_>,
        owner: &Target,
        head: &Target,
        property: &str,
    ) -> Vec<String> {
        let Some(entries) = owner.provenance.get(property) else {
            return expand_list(&query.property(owner, head, property), false);
        };
        entries
            .iter()
            .flat_map(|(value, backtrace)| {
                expand_list(
                    &query.ctx.genex_with_dag_usage(
                        (value, backtrace),
                        owner,
                        head,
                        property,
                        Some(query.language),
                        false,
                    ),
                    false,
                )
            })
            .collect()
    }

    fn collect<'i>(
        query: &Query<'_, 'i>,
        head: &Target,
        property: &str,
        items: Vec<Item<'i>>,
        seen: &mut HashSet<usize>,
        values: &mut Vec<String>,
    ) {
        for target in items.into_iter().filter_map(|item| item.target) {
            if !seen.insert(std::ptr::from_ref(target) as usize) {
                continue;
            }
            for value in entries(query, target, head, property) {
                if property == "INTERFACE_LINK_DIRECTORIES" && !Path::new(&value).is_absolute() {
                    query.error(format!("Target \"{}\" contains relative path in its INTERFACE_LINK_DIRECTORIES:\n  \"{value}\"", target.name));
                }
                values.push(value);
            }
            collect(
                query,
                head,
                property,
                interface(query, target, head).libraries,
                seen,
                values,
            );
        }
    }
    let mut values = entries(query, head, head, property);
    if property == "LINK_DIRECTORIES" {
        // `cmGeneratorTarget::EvaluateLinkDirectories`: CMP0081 gates the
        // relative-path diagnostic; the target's directory backtrace is
        // attached in all modes.
        let status = query.policy(head, "CMP0081");
        if status != PolicyStatus::Old {
            let backtrace = query.ctx.dir_backtrace(head.makefile);
            for value in values
                .iter()
                .filter(|value| !Path::new(value).is_absolute())
            {
                let message = format!(
                    "Found relative path while evaluating link directories of \"{}\":\n  \"{value}\"\n",
                    head.name
                );
                match status {
                    PolicyStatus::New => query.error_at(backtrace.clone(), message),
                    PolicyStatus::Warn => query.ctx.warning(
                        backtrace.clone(),
                        format!(
                            "{}\n{message}",
                            crate::commands::target::policy_warning("CMP0081")
                        ),
                    ),
                    PolicyStatus::Old => {}
                }
            }
        }
    }
    // INTEGRATION: For CMP0099 OLD, traverse in compile mode to exclude
    // LINK_ONLY edges. The genex evaluator currently provides link mode.
    collect(
        query,
        head,
        &format!("INTERFACE_{property}"),
        implementation(query, head),
        &mut HashSet::from([std::ptr::from_ref(head) as usize]),
        &mut values,
    );
    if property == "LINK_DIRECTORIES" {
        values
            .iter_mut()
            .for_each(|value| *value = normalize_slashes(value));
    }
    unique(values)
}

struct Information<'a, 'q, 'i> {
    query: &'a Query<'q, 'i>,
    head: &'a Target,
    items: Vec<String>,
    /// Per-item emitted strings with their link-item backtraces
    /// (file API codemodel "libraries" fragments).
    fragments: Vec<(String, Vec<crate::messages::Frame>)>,
    depends: Vec<String>,
    shared_symlinks: Vec<String>,
    link_dirs: Directories,
    runtime_dirs: Directories,
    dependent_dirs: Directories,
    implicit_dirs: HashSet<String>,
    features: HashMap<String, Option<Feature>>,
    groups: HashMap<String, Option<Feature>>,
    static_flag: String,
    dynamic_flag: String,
    start_static: bool,
    current_static: bool,
    lib_flag: String,
    lib_suffix: String,
    runtime_flag: String,
    runtime_sep: String,
    rpath_link_flag: String,
    classifier: Classifier,
}

impl<'a, 'q, 'i> Information<'a, 'q, 'i> {
    fn new(query: &'a Query<'q, 'i>, head: &'a Target, users: Vec<String>) -> Self {
        let var = |name: &str| query.var(head, name);
        let mut implicit = expand_list(&var("CMAKE_PLATFORM_IMPLICIT_LINK_DIRECTORIES"), false);
        let arch = var("CMAKE_LIBRARY_ARCHITECTURE");
        if !arch.is_empty() {
            implicit.extend(
                implicit
                    .clone()
                    .into_iter()
                    .map(|dir| format!("{dir}/{arch}")),
            );
        }
        let language = query.language;
        implicit.extend(expand_list(
            &var(&format!("CMAKE_{language}_IMPLICIT_LINK_DIRECTORIES")),
            false,
        ));
        let (switch, runtime) = match head.kind {
            TargetKind::Executable => ("EXE", "EXECUTABLE"),
            TargetKind::ModuleLibrary => ("SHARED_MODULE", "SHARED_LIBRARY"),
            _ => ("SHARED_LIBRARY", "SHARED_LIBRARY"),
        };
        let start_static = on(head, "LINK_SEARCH_START_STATIC");
        let lookup = |name: &str| {
            query
                .ctx
                .var(head.makefile, &format!("CMAKE_{language}_{name}"))
                .unwrap_or_else(|| var(&format!("CMAKE_{name}")))
        };
        let mut dependent_dirs = Directories::new(Vec::new());
        dependent_dirs.add_language(expand_list(&var("CMAKE_PLATFORM_RUNTIME_PATH"), false));
        Self {
            query,
            head,
            items: Vec::new(),
            fragments: Vec::new(),
            depends: Vec::new(),
            shared_symlinks: Vec::new(),
            link_dirs: Directories::new(users.clone()),
            runtime_dirs: Directories::new(users),
            dependent_dirs,
            implicit_dirs: implicit.into_iter().collect(),
            features: HashMap::new(),
            groups: HashMap::new(),
            static_flag: var(&format!("CMAKE_{switch}_LINK_STATIC_{language}_FLAGS")),
            dynamic_flag: var(&format!("CMAKE_{switch}_LINK_DYNAMIC_{language}_FLAGS")),
            start_static,
            current_static: start_static,
            lib_flag: lookup("LINK_LIBRARY_FLAG"),
            lib_suffix: lookup("LINK_LIBRARY_SUFFIX"),
            runtime_flag: var(&format!("CMAKE_{runtime}_RUNTIME_{language}_FLAG")),
            runtime_sep: var(&format!("CMAKE_{runtime}_RUNTIME_{language}_FLAG_SEP")),
            rpath_link_flag: var(&format!("CMAKE_{runtime}_RPATH_LINK_{language}_FLAG")),
            classifier: Classifier::new(query, head),
        }
    }

    fn var(&self, name: &str) -> String {
        self.query.var(self.head, name)
    }

    fn langvar(&self, name: &str) -> String {
        self.var(&format!("CMAKE_{}_{name}", self.query.language))
    }

    fn lookup(&self, name: &str, generic: &str) -> String {
        self.query
            .ctx
            .var(self.head.makefile, name)
            .unwrap_or_else(|| self.var(generic))
    }

    fn set_static(&mut self, next: bool) {
        if self.current_static != next
            && !self.static_flag.is_empty()
            && !self.dynamic_flag.is_empty()
        {
            self.items.push(
                if next {
                    &self.static_flag
                } else {
                    &self.dynamic_flag
                }
                .clone(),
            );
        }
        self.current_static = next;
    }

    fn emit_entries(&mut self, entries: &[Entry<'_>]) {
        let mut active: Option<(&str, Feature)> = None;
        for entry in entries {
            let group = matches!(entry.kind, Kind::Group | Kind::GroupEnd);
            if active.as_ref().is_some_and(|(name, _)| {
                entry.kind == Kind::GroupEnd || (!group && name != &entry.item.feature)
            }) && let Some((_, feature)) = active.take()
            {
                self.emit_if_nonempty(feature.suffix);
            }
            if group {
                if let Some(feature) = self.feature(&entry.item.feature, true) {
                    self.emit_if_nonempty(if entry.kind == Kind::Group {
                        feature.prefix
                    } else {
                        feature.suffix
                    });
                }
                continue;
            }
            if entry.item.feature != DEFAULT && active.is_none() {
                let Some(feature) = self.feature(&entry.item.feature, false) else {
                    continue;
                };
                self.emit_if_nonempty(feature.prefix.clone());
                active = Some((&entry.item.feature, feature));
            }
            if entry.kind == Kind::SharedDep {
                self.add_shared(entry);
            } else {
                let emitted = self.items.len();
                self.add_item(entry, active.as_ref().map(|(_, feature)| feature));
                for string in &self.items[emitted..] {
                    self.fragments
                        .push((string.clone(), entry.item.backtrace.clone()));
                }
            }
        }
        if let Some((_, feature)) = active {
            self.emit_if_nonempty(feature.suffix);
        }
    }

    fn emit_if_nonempty(&mut self, value: String) {
        if !value.is_empty() {
            self.items.push(value);
        }
    }

    fn add_item(&mut self, entry: &Entry<'_>, feature: Option<&Feature>) {
        let name = &entry.item.text;
        if let Some(target) = entry.item.target {
            match target.kind {
                TargetKind::InterfaceLibrary => {
                    if target.imported
                        && let Some(lib) =
                            import_property(target, self.query.config, "IMPORTED_LIBNAME")
                        && !lib.is_empty()
                    {
                        self.add_user(lib, None);
                    }
                    return;
                }
                TargetKind::ObjectLibrary => return,
                TargetKind::Executable => {
                    let loader = if self.head.kind == TargetKind::ModuleLibrary {
                        self.var(&format!(
                            "CMAKE_SHARED_MODULE_LOADER_{}_FLAG",
                            self.query.language
                        ))
                    } else {
                        String::new()
                    };
                    if !loader.is_empty() {
                        let full = self.query.generator.artifact(
                            self.query.ctx,
                            target,
                            self.query.config,
                            true,
                        );
                        self.items
                            .push(format!("{loader}{}", shell(&self.query.ninja_path(&full))));
                        self.depends.push(full);
                    }
                    return;
                }
                TargetKind::StaticLibrary
                | TargetKind::SharedLibrary
                | TargetKind::UnknownLibrary => {}
                _ => {
                    self.add_user(name, feature);
                    return;
                }
            }
            let full =
                self.query
                    .generator
                    .artifact(self.query.ctx, target, self.query.config, true);
            if full.is_empty() {
                return;
            }
            if target.kind != TargetKind::SharedLibrary || !on(self.head, "LINK_DEPENDS_NO_SHARED")
            {
                self.depends.push(full.clone());
            }
            if target.kind == TargetKind::SharedLibrary {
                self.shared_symlinks.push(self.query.generator.artifact(
                    self.query.ctx,
                    target,
                    self.query.config,
                    false,
                ));
            }
            if target.kind != TargetKind::StaticLibrary {
                self.set_static(false);
            }
            let no_soname = target.imported
                && target.kind == TargetKind::SharedLibrary
                && is_on_opt(import_property(
                    target,
                    self.query.config,
                    "IMPORTED_NO_SONAME",
                ));
            if no_soname && is_on(&self.var("CMAKE_PLATFORM_USES_PATH_WHEN_NO_SONAME")) {
                self.add_without_soname(&full, feature);
            } else {
                self.emit_path(&full, feature, false);
            }
            self.add_runtime(&full, Some(target));
        } else if name.ends_with("-NOTFOUND") {
            self.query.error(format!("The following variables are used in this project, but they are set to NOTFOUND.\nPlease set them or make sure they are set and tested correctly in the CMake files:\n{}\n    linked by target \"{}\" in directory {}", name.trim_end_matches("-NOTFOUND"), self.head.name, self.query.ctx.interp.makefiles[self.head.makefile].source_dir));
        } else if Path::new(name).is_absolute() {
            if Path::new(name).is_dir() {
                return;
            }
            self.depends.push(name.clone());
            let filename = filename_name(name);
            let kind = self.classifier.parse(&filename).map(|(_, _, kind)| kind);
            if kind == Some(LibraryType::Shared)
                && is_on(&self.var("CMAKE_PLATFORM_USES_PATH_WHEN_NO_SONAME"))
                && elf_soname(name).is_none()
            {
                self.add_without_soname(name, feature);
            } else {
                match kind {
                    Some(LibraryType::Shared) => self.set_static(false),
                    _ => self.set_static(self.start_static),
                }
                self.emit_path(name, feature, entry.kind == Kind::Object);
            }
            self.add_runtime(name, None);
        } else if entry.kind != Kind::Object {
            self.add_user(name, feature);
        }
    }

    fn add_without_soname(&mut self, full: &str, feature: Option<&Feature>) {
        let file = filename_name(full);
        self.add_user(&file, feature);
        let alternatives = self
            .classifier
            .parse(&file)
            .map(|(name, extension, _)| {
                let stem = file.strip_suffix(extension).unwrap_or(name);
                self.classifier
                    .extensions
                    .iter()
                    .filter(|(suffix, _)| suffix != extension)
                    .map(|(suffix, _)| format!("{stem}{suffix}"))
                    .collect()
            })
            .unwrap_or_default();
        self.link_dirs.library(full, alternatives);
    }

    fn add_user(&mut self, item: &str, feature: Option<&Feature>) {
        if item.starts_with(['-', '$', '`']) {
            self.set_static(self.start_static);
            self.items.push(item.to_owned());
        } else if item.starts_with("LINKER:") {
            self.emit_if_nonempty(linker_wrapper(self.query, self.head, item));
        } else {
            let (name, mode) = self
                .classifier
                .parse(item)
                .map_or((item, LibraryType::Unknown), |(name, _, kind)| (name, kind));
            let base = format!("{name}{}", self.lib_suffix);
            self.set_static(match mode {
                LibraryType::Shared => false,
                LibraryType::Static => true,
                LibraryType::Unknown => self.start_static,
            });
            let link_item = format!("{}{base}", self.lib_flag);
            self.items.push(feature.map_or_else(
                || link_item.clone(),
                |feature| format_feature(&feature.name, &base, item, &link_item),
            ));
        }
    }

    fn emit_path(&mut self, full: &str, feature: Option<&Feature>, object: bool) {
        let path = shell(&self.query.ninja_path(full));
        if let Some(feature) = feature {
            self.items
                .push(format_feature(&feature.path, &path, &path, &path));
        } else {
            let kind = if object {
                "LINK_OBJECT_FILE_FLAG"
            } else {
                "LINK_LIBRARY_FILE_FLAG"
            };
            let flag = self.lookup(
                &format!("CMAKE_{}_{kind}", self.query.language),
                &format!("CMAKE_{kind}"),
            );
            self.items.push(format!("{flag}{path}"));
        }
    }

    fn is_imported_stub(&self, target: &Target) -> bool {
        target.imported
            && target.kind == TargetKind::SharedLibrary
            && target::import_info(target, self.query.config).is_none_or(|i| i.location.is_empty())
    }

    fn add_runtime(&mut self, full: &str, target: Option<&Target>) {
        let soname = match target {
            Some(target) if self.is_imported_stub(target) => return,
            Some(target) if target.kind == TargetKind::SharedLibrary => self
                .query
                .generator
                .soname(self.query.ctx, target, self.query.config),
            Some(target) if target.kind != TargetKind::UnknownLibrary => return,
            _ if self
                .classifier
                .parse(&filename_name(full))
                .is_some_and(|(_, _, kind)| kind == LibraryType::Shared) =>
            {
                None
            }
            _ => return,
        };
        self.runtime_dirs.runtime(full, soname.clone());
        if is_on(&self.var(&format!(
            "CMAKE_SHARED_LIBRARY_LINK_{}_WITH_RUNTIME_PATH",
            self.query.language
        ))) {
            self.link_dirs.runtime(full, soname);
        }
    }

    fn add_shared(&mut self, entry: &Entry<'_>) {
        let target = entry.item.target;
        if let Some(target) = target {
            if target.kind != TargetKind::SharedLibrary {
                return;
            }
        } else if !Path::new(&entry.item.text).is_absolute()
            || !self
                .classifier
                .parse(&filename_name(&entry.item.text))
                .is_some_and(|(_, _, kind)| kind == LibraryType::Shared)
        {
            return;
        }
        let link_files = is_on(&self.var("CMAKE_LINK_DEPENDENT_LIBRARY_FILES"));
        let add_to_link_dirs = is_on(&self.var("CMAKE_LINK_DEPENDENT_LIBRARY_DIRS"));
        if !link_files && !add_to_link_dirs && self.rpath_link_flag.is_empty() {
            return;
        }
        // A stub may not have a file named for its SONAME. Give it directly
        // to linkers that recursively check undefined shared-library symbols.
        if link_files
            || (target.is_some_and(|t| self.is_imported_stub(t))
                && matches!(
                    self.langvar("COMPILER_LINKER_ID").as_str(),
                    "GNU" | "Solaris"
                ))
        {
            self.add_item(entry, None);
            return;
        }
        let full = target.map_or_else(
            || entry.item.text.clone(),
            |target| {
                self.query
                    .generator
                    .artifact(self.query.ctx, target, self.query.config, false)
            },
        );
        if full.is_empty() || full.ends_with("-NOTFOUND") {
            return;
        }
        self.add_runtime(&full, target);
        let soname = target.and_then(|target| {
            self.query
                .generator
                .soname(self.query.ctx, target, self.query.config)
        });
        let directories = if add_to_link_dirs {
            &mut self.link_dirs
        } else {
            &mut self.dependent_dirs
        };
        directories.runtime(&full, soname);
    }

    fn add_implicit(&mut self, languages: &BTreeSet<String>) {
        let implicit: HashSet<_> = expand_list(&self.langvar("IMPLICIT_LINK_LIBRARIES"), false)
            .into_iter()
            .filter(|item| !item.starts_with('-') || item.starts_with("-l"))
            .collect();
        for language in languages
            .iter()
            .filter(|language| *language != self.query.language)
        {
            for value in expand_list(
                &self.var(&format!("CMAKE_{language}_IMPLICIT_LINK_LIBRARIES")),
                false,
            ) {
                if !implicit.contains(&value) {
                    self.add_item(
                        &Entry {
                            item: Item::new(value, None, DEFAULT),
                            kind: Kind::Library,
                        },
                        None,
                    );
                }
            }
            self.link_dirs.add_language(expand_list(
                &self.var(&format!("CMAKE_{language}_IMPLICIT_LINK_DIRECTORIES")),
                false,
            ));
        }
    }

    fn link_what_you_use(&self) -> Option<LinkUsageCheck> {
        if !on(self.head, "LINK_WHAT_YOU_USE") {
            return None;
        }
        let flag = self.langvar("LINK_WHAT_YOU_USE_FLAG");
        if flag.is_empty() {
            return None;
        }
        Some(LinkUsageCheck {
            flag: linker_wrapper(self.query, self.head, &flag),
            checker: self
                .query
                .ctx
                .var(self.head.makefile, "CMAKE_LINK_WHAT_YOU_USE_CHECK"),
        })
    }

    fn rpath(
        &self,
        ordered: &Ordered<'_>,
        dirs: &[String],
        output: &str,
    ) -> (String, Option<(String, String)>) {
        let enabled = !is_on(&self.var("CMAKE_SKIP_RPATH")) && !self.runtime_flag.is_empty();
        let for_install = on(self.head, "BUILD_WITH_INSTALL_RPATH");
        let list = self.runtime_paths(ordered, dirs, output, enabled, for_install);
        if self.runtime_sep.is_empty() {
            let flags = list
                .iter()
                .map(|dir| format!("{}{}", self.runtime_flag, shell(dir)))
                .collect::<Vec<_>>()
                .join(" ");
            return (flags, None);
        }
        let mut list = list.join(&self.runtime_sep);
        let change = if enabled
            && !for_install
            && self.head.have_install_rule
            && !is_on(&self.var("CMAKE_NO_BUILTIN_CHRPATH"))
            && self.var("CMAKE_EXECUTABLE_FORMAT") == "ELF"
        {
            let install = self
                .runtime_paths(ordered, dirs, output, enabled, true)
                .join(&self.runtime_sep);
            if !list.is_empty() {
                list.push_str(&self.runtime_sep);
            }
            while list.len() < install.len() {
                list.push_str(&self.runtime_sep);
            }
            Some((list.clone(), install))
        } else {
            None
        };
        let flags = if list.is_empty() {
            String::new()
        } else {
            format!("{}{}", self.runtime_flag, shell(&list))
        };
        (flags, change)
    }

    fn runtime_paths(
        &self,
        ordered: &Ordered<'_>,
        dirs: &[String],
        output: &str,
        enabled: bool,
        install: bool,
    ) -> Vec<String> {
        let skip_install = is_on(&self.var("CMAKE_SKIP_INSTALL_RPATH"));
        let explicit_name = if install {
            "INSTALL_RPATH"
        } else {
            "BUILD_RPATH"
        };
        let explicit = self.query.property(self.head, self.head, explicit_name);
        let build = enabled
            && !install
            && !on(self.head, "SKIP_BUILD_RPATH")
            && (!explicit.is_empty() || ordered.has_implementation);
        let install_rpath = enabled && install && !skip_install;
        let mut paths = if build || install_rpath {
            expand_list(&explicit, false)
        } else {
            Vec::new()
        };
        if build || (install_rpath && on(self.head, "INSTALL_RPATH_USE_LINK_PATH")) {
            let origin = self.var("CMAKE_SHARED_LIBRARY_RPATH_ORIGIN_TOKEN");
            let relative =
                !origin.is_empty() && on(self.head, "BUILD_RPATH_USE_ORIGIN") && !output.is_empty();
            let root = normalize_slashes(&self.lookup("CMAKE_SYSROOT_LINK", "CMAKE_SYSROOT"));
            let staging = self.var("CMAKE_STAGING_PREFIX");
            let prefix = self.var("CMAKE_INSTALL_PREFIX");
            let source = &self.query.ctx.interp.makefiles[0].source_dir;
            let binary = self.query.top_binary();
            for dir in dirs {
                if install && (under(dir, source) || under(dir, binary)) {
                    continue;
                }
                let path = if !root.is_empty() && dir.starts_with(&root) {
                    dir[root.len()..].to_owned()
                } else if !staging.is_empty() && dir.starts_with(&staging) {
                    normalize_slashes(&format!("{prefix}/{}", &dir[staging.len()..]))
                } else if build && relative && under(dir, binary) {
                    let tail = relative_path(output, dir);
                    if tail.is_empty() {
                        origin.clone()
                    } else {
                        format!("{origin}/{tail}")
                    }
                } else {
                    dir.clone()
                };
                paths.push(path);
            }
        }
        for language in &ordered.languages {
            if is_on(&self.var(&format!(
                "CMAKE_{language}_USE_IMPLICIT_LINK_DIRECTORIES_IN_RUNTIME_PATH"
            ))) {
                paths.extend(expand_list(
                    &self.var(&format!("CMAKE_{language}_IMPLICIT_LINK_DIRECTORIES")),
                    false,
                ));
            }
        }
        paths.extend(expand_list(
            &self.var("CMAKE_PLATFORM_REQUIRED_RUNTIME_PATH"),
            false,
        ));
        unique(paths)
    }

    fn feature(&mut self, name: &str, group: bool) -> Option<Feature> {
        let cached = if group {
            self.groups.get(name)
        } else {
            self.features.get(name)
        };
        if let Some(cached) = cached {
            return cached.clone();
        }
        let result = self.load_feature(name, group);
        let cache = if group {
            &mut self.groups
        } else {
            &mut self.features
        };
        cache.insert(name.to_owned(), result.clone());
        result
    }

    fn load_feature(&self, name: &str, group: bool) -> Option<Feature> {
        let kind = if group { "LINK_GROUP" } else { "LINK_LIBRARY" };
        let mut key = format!("CMAKE_{}_{kind}_USING_{name}", self.query.language);
        let mut supported = self
            .query
            .ctx
            .var(self.head.makefile, &format!("{key}_SUPPORTED"));
        if supported.is_none() {
            key = format!("CMAKE_{kind}_USING_{name}");
            supported = self
                .query
                .ctx
                .var(self.head.makefile, &format!("{key}_SUPPORTED"));
        }
        let error = |problem| {
            self.query.error(format!("Feature '{name}', specified through generator-expression '$<{kind}>' to link target '{}', is not {problem} for the '{}' link language.", self.head.name, self.query.language));
        };
        if !is_on_opt(supported.as_deref()) {
            error("supported");
            return None;
        }
        let Some(raw) = self.query.ctx.var(self.head.makefile, &key) else {
            error("defined");
            return None;
        };
        let items = expand_list(&raw, true);
        let mut feature = Feature::default();
        let format = match (group, items.as_slice()) {
            (true, [prefix, suffix]) => {
                feature.prefix = linker_wrapper(self.query, self.head, prefix);
                feature.suffix = linker_wrapper(self.query, self.head, suffix);
                return Some(feature);
            }
            (false, [format]) => format,
            (false, [prefix, format, suffix]) => {
                feature.prefix = linker_wrapper(self.query, self.head, prefix);
                feature.suffix = linker_wrapper(self.query, self.head, suffix);
                format
            }
            _ => {
                self.query.error(format!("Feature '{name}', specified by variable '{key}', is malformed (wrong number of elements) and cannot be used to link target '{}'.", self.head.name));
                return None;
            }
        };
        for (path, tag, opposite) in [(true, "PATH", "NAME"), (false, "NAME", "PATH")] {
            let value = feature_format(format, tag, opposite);
            if !["<LIBRARY>", "<LIB_ITEM>", "<LINK_ITEM>"]
                .iter()
                .any(|pattern| value.contains(pattern))
            {
                let alternative = if format.contains("PATH{") || format.contains("NAME{") {
                    format!(" for \"{tag}{{}}\" alternative")
                } else {
                    String::new()
                };
                self.query.error(format!("Feature '{name}', specified by variable '{key}', is malformed (\"<LIBRARY>\", \"<LIB_ITEM>\", or \"<LINK_ITEM>\" patterns are missing{alternative}) and cannot be used to link target '{}'.", self.head.name));
                return None;
            }
            let value = linker_wrapper(self.query, self.head, &value);
            *if path {
                &mut feature.path
            } else {
                &mut feature.name
            } = value;
        }
        Some(feature)
    }
}

/// Resolve LINKER: fragments using the selected language's compiler wrapper.
fn linker_wrapper(query: &Query<'_, '_>, head: &Target, value: &str) -> String {
    let Some(args) = value.strip_prefix("LINKER:") else {
        return value.to_owned();
    };
    let tokens = if let Some(args) = args.strip_prefix("SHELL:") {
        shlex::split(args).unwrap_or_default()
    } else {
        args.split(',')
            .filter(|arg| !arg.is_empty())
            .map(str::to_owned)
            .collect()
    };
    let lang = query.language;
    let mut wrapper = expand_list(
        &query.var(head, &format!("CMAKE_{lang}_LINKER_WRAPPER_FLAG")),
        false,
    );
    let separate = wrapper.last().is_some_and(|flag| flag == " ");
    if separate {
        wrapper.pop();
    }
    let separator = query.var(head, &format!("CMAKE_{lang}_LINKER_WRAPPER_FLAG_SEP"));
    let tokens = if !separator.is_empty() {
        vec![tokens.join(&separator)]
    } else {
        tokens
    };
    tokens
        .into_iter()
        .map(|token| {
            let prefix = wrapper.join(" ");
            if prefix.is_empty() {
                token
            } else if separate {
                format!("{prefix} {token}")
            } else {
                format!("{prefix}{token}")
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

#[derive(Clone, Default)]
struct Feature {
    prefix: String,
    suffix: String,
    path: String,
    name: String,
}

fn feature_format(format: &str, tag: &str, opposite: &str) -> String {
    let tag = format!("{tag}{{");
    let opposite = format!("{opposite}{{");
    let mut rest = format;
    let mut result = String::new();
    while let Some(index) = [rest.find(&tag), rest.find(&opposite)]
        .into_iter()
        .flatten()
        .min()
    {
        result.push_str(&rest[..index]);
        let use_content = rest[index..].starts_with(&tag);
        let content = &rest[index
            + if use_content {
                tag.len()
            } else {
                opposite.len()
            }..];
        let Some(end) = content.find('}') else {
            result.push_str(&rest[index..]);
            return result;
        };
        if use_content {
            result.push_str(&content[..end]);
        }
        rest = &content[end + 1..];
    }
    result.push_str(rest);
    result
}

fn format_feature(format: &str, library: &str, lib_item: &str, link_item: &str) -> String {
    format
        .replace("<LIBRARY>", library)
        .replace("<LIB_ITEM>", lib_item)
        .replace("<LINK_ITEM>", link_item)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum LibraryType {
    Static,
    Shared,
    Unknown,
}

struct Classifier {
    prefixes: Vec<String>,
    extensions: Vec<(String, LibraryType)>,
    openbsd: bool,
}

impl Classifier {
    fn new(query: &Query<'_, '_>, head: &Target) -> Self {
        let var = |name| query.var(head, name);
        let prefixes = unique([
            var("CMAKE_STATIC_LIBRARY_PREFIX"),
            var("CMAKE_SHARED_LIBRARY_PREFIX"),
        ])
        .into_iter()
        .filter(|prefix| !prefix.is_empty())
        .collect();
        let mut extensions = Vec::new();
        for (key, kind) in [
            ("CMAKE_IMPORT_LIBRARY_SUFFIX", LibraryType::Shared),
            ("CMAKE_SHARED_LIBRARY_SUFFIX", LibraryType::Shared),
            ("CMAKE_EXTRA_SHARED_LIBRARY_SUFFIXES", LibraryType::Shared),
            ("CMAKE_STATIC_LIBRARY_SUFFIX", LibraryType::Static),
            ("CMAKE_LINK_LIBRARY_SUFFIX", LibraryType::Unknown),
            ("CMAKE_EXTRA_LINK_EXTENSIONS", LibraryType::Unknown),
        ] {
            extensions.extend(
                expand_list(&var(key), false)
                    .into_iter()
                    .map(|suffix| (suffix, kind)),
            );
        }
        let openbsd = is_on_opt(
            query
                .ctx
                .interp
                .global_props
                .get("FIND_LIBRARY_USE_OPENBSD_VERSIONING"),
        );
        Self {
            prefixes,
            extensions,
            openbsd,
        }
    }

    fn parse<'a>(&'a self, file: &'a str) -> Option<(&'a str, &'a str, LibraryType)> {
        if file.contains(['/', ':']) {
            return None;
        }
        let base = self
            .prefixes
            .iter()
            .find_map(|prefix| file.strip_prefix(prefix))
            .unwrap_or(file);
        self.extensions.iter().find_map(|(extension, kind)| {
            let match_version = self.openbsd || *kind == LibraryType::Shared;
            let name = base.strip_suffix(extension).or_else(|| {
                if !match_version {
                    return None;
                }
                let (name, version) = base.rsplit_once(extension)?;
                (version.starts_with('.')
                    && version[1..].split('.').all(|component| {
                        !component.is_empty() && component.bytes().all(|b| b.is_ascii_digit())
                    }))
                .then_some(name)
            })?;
            Some((name, extension.as_str(), *kind))
        })
    }
}

fn shell(value: &str) -> String {
    escape_for_shell(value, NINJA_UNIX | FLAG_ALLOW_MAKE_VARIABLES)
}

fn under(path: &str, top: &str) -> bool {
    path == top || is_subdirectory(path, top)
}

fn relative_path(from: &str, to: &str) -> String {
    let from = Path::new(from).components().collect::<Vec<_>>();
    let to = Path::new(to).components().collect::<Vec<_>>();
    let common = from
        .iter()
        .zip(&to)
        .take_while(|(from, to)| from == to)
        .count();
    // KWSys RelativePath emits "../" for every ascent, including the last
    // one when the destination is an ancestor of the output directory.
    let up = "../".repeat(from.len() - common);
    let down = to[common..]
        .iter()
        .map(|part| part.as_os_str().to_str().unwrap_or_default())
        .collect::<Vec<_>>()
        .join("/");
    up + &down
}

fn normalize_slashes(value: &str) -> String {
    let value = value.replace('\\', "/");
    if value.starts_with('/') {
        collapse_full_path(&value, "")
    } else {
        value
    }
}
