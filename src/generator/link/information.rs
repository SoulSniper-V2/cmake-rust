//! Port of `cmComputeLinkInformation` and `cmLinkLineComputer`: linker items,
//! search paths and the files the link step depends on.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::Path;
use std::rc::Rc;

use super::dependencies::{self, Entry, Kind};
use super::directories::{OrderDirectories, Reporter, elf_soname};
use super::*;
use crate::apple::{
    FrameworkFormat, is_path_to_framework, is_path_to_xcframework, split_framework_path,
};
use crate::cmregex::Regex;
use crate::commands::install::unix_slashes;
use crate::util::{collapse_full_path, filename_name, filename_path, is_subdirectory};

/// `cmComputeLinkInformation` after `Compute()`.
pub(in crate::generator) struct LinkInformation {
    language: String,
    pub items: Vec<LinkItem>,
    /// `GetDepends`.
    pub depends: Vec<String>,
    /// `GetFrameworkPaths`.
    pub framework_paths: Vec<String>,
    /// `GetSharedLibrariesLinked`: target keys.
    pub shared_libraries_linked: Vec<String>,
    /// `GetRuntimeDLLs`: target keys.
    pub runtime_dlls: Vec<String>,
    linker_search_path: OrderDirectories,
    runtime_search_path: OrderDirectories,
    /// Only with a `-rpath-link` style flag.
    dependent_search_path: Option<OrderDirectories>,
    runtime_flag: String,
    runtime_sep: String,
    runtime_always: String,
    runtime_use_chrpath: bool,
    rpath_link_flag: String,
    /// `GetLinkClosure(config)->Languages`.
    closure_languages: Vec<String>,
    has_implementation: bool,
}

/// `cmComputeLinkInformation::Item`.
#[derive(Clone)]
pub(in crate::generator) struct LinkItem {
    pub value: String,
    pub is_path: bool,
    /// The key of the linked target.
    pub target: Option<String>,
    /// An INTERFACE or OBJECT library, which has no place on the link line.
    unlinked: bool,
    feature: Option<Rc<Feature>>,
    backtrace: Vec<Frame>,
}

impl LinkItem {
    fn new(value: String, backtrace: &[Frame]) -> LinkItem {
        LinkItem {
            value,
            is_path: false,
            target: None,
            unlinked: false,
            feature: None,
            backtrace: backtrace.to_vec(),
        }
    }

    /// `GetFormattedItem`.
    fn formatted(&self, path: &str) -> String {
        match &self.feature {
            Some(feature) => feature.decorate(path, path, path, self.is_path),
            None => path.to_owned(),
        }
    }
}

/// `FeatureDescriptor`.
#[derive(Default)]
struct Feature {
    name: String,
    supported: bool,
    prefix: String,
    suffix: String,
    item_path: String,
    item_name: String,
}

impl Feature {
    fn of_item(name: &str, format: String) -> Feature {
        Feature {
            name: name.to_owned(),
            supported: true,
            item_path: format.clone(),
            item_name: format,
            ..Feature::default()
        }
    }

    /// `GetDecoratedItem`.
    fn decorate(&self, library: &str, lib_item: &str, link_item: &str, is_path: bool) -> String {
        let format = if is_path {
            &self.item_path
        } else {
            &self.item_name
        };
        crate::generator::flags::expand_placeholders(format, |name| {
            match name {
                "LIBRARY" => library,
                "LIB_ITEM" => lib_item,
                "LINK_ITEM" => link_item,
                _ => name,
            }
            .to_owned()
        })
    }
}

/// `cmGeneratorTarget::GetLinkInformation(config)`, which is computed once.
pub(in crate::generator) fn link_information(
    ctx: &GenCtx,
    target: &Target,
    config: &str,
    language: &str,
) -> Option<Rc<LinkInformation>> {
    let key = (ctx.target_key(target), config.to_ascii_uppercase());
    if let Some(found) = ctx.link_information.borrow().get(&key) {
        return found.clone();
    }
    let computed = compute(&Query::new(ctx, config, language), target).map(Rc::new);
    ctx.link_information
        .borrow_mut()
        .insert(key, computed.clone());
    computed
}

/// `cmComputeLinkInformation::Compute` and `CheckPropertyCompatibility`.
pub(super) fn compute(query: &Query<'_, '_>, head: &Target) -> Option<LinkInformation> {
    let links = matches!(
        head.kind,
        TargetKind::Executable
            | TargetKind::SharedLibrary
            | TargetKind::ModuleLibrary
            | TargetKind::StaticLibrary
    ) || (target::can_compile_sources(head)
        && super::super::module_support::needs_link_information(query.ctx, head, query.config));
    if !links {
        return None;
    }
    // We require a link language for the target.
    if query.language.is_empty() {
        query.system_error(format!(
            "CMake can not determine linker language for target: {}",
            head.name
        ));
        return None;
    }
    if let Some(strategy) = head
        .props
        .get("LINK_LIBRARIES_STRATEGY")
        .filter(|strategy| !matches!(*strategy, "REORDER_MINIMALLY" | "REORDER_FREELY"))
    {
        query.error_at(
            head.backtrace.clone(),
            format!("LINK_LIBRARIES_STRATEGY value '{strategy}' is not recognized."),
        );
        return None;
    }
    let mut builder = Builder::new(query, head);
    let ordered = dependencies::compute(query, head);
    builder.info.has_implementation = ordered.has_implementation;
    builder.add_entries(&ordered.entries);
    // Restore the target link type so the correct system runtime libraries
    // are found.
    let end = if on(head, "LINK_SEARCH_END_STATIC") {
        LinkType::Static
    } else {
        builder.start_type
    };
    builder.set_type(end);
    builder.add_implicit();
    super::super::compatible::check_declarations(
        query.ctx,
        head,
        query.config,
        builder.linked.iter().copied(),
    );
    Some(builder.info)
}

/// The `cmLinkLineComputer` view of the link information of `head`.
pub(super) fn line(query: &Query<'_, '_>, head: &Target, info: &LinkInformation) -> LinkLine {
    let reporter = Reporter {
        query,
        target: head,
    };
    let var = |name: &str| query.var(head, name);
    let lookup = |name: &str| {
        query
            .ctx
            .var(head.makefile, &format!("CMAKE_{}_{name}", info.language))
            .unwrap_or_else(|| var(&format!("CMAKE_{name}")))
    };
    // cmLinkLineComputer::ComputeFrameworkPath.
    let framework_path = query
        .ctx
        .var(
            head.makefile,
            &format!("CMAKE_{}_FRAMEWORK_SEARCH_FLAG", info.language),
        )
        .map(|flag| {
            info.framework_paths
                .iter()
                .map(|dir| format!("{flag}{} ", query.shell(dir)))
                .collect::<String>()
        })
        .unwrap_or_default();

    // cmLinkLineComputer::ComputeLinkPath.
    let flag = lookup("LIBRARY_PATH_FLAG");
    let terminator = lookup("LIBRARY_PATH_TERMINATOR");
    let mut link_path = Vec::new();
    if info.language == "Swift" {
        let mut dirs = String::new();
        for target in info.items.iter().filter_map(|item| item.target.as_deref()) {
            let Some(target) = query.ctx.interp.ext.targets.get(target).filter(|target| {
                matches!(
                    target.kind,
                    TargetKind::StaticLibrary | TargetKind::SharedLibrary
                )
            }) else {
                continue;
            };
            let dir = target::directory(
                query.ctx,
                target,
                query.config,
                link_artifact(query, target),
            );
            dirs.push_str(&format!(" {flag}{}{terminator} ", query.shell(&dir)));
        }
        if !dirs.is_empty() {
            link_path.push((dirs, None));
        }
    }
    let link_dirs = info.linker_search_path.ordered(&reporter).to_vec();
    for dir in &link_dirs {
        link_path.push((
            format!(" {flag}{}{terminator} ", query.shell(dir)),
            Some(dir.clone()),
        ));
    }
    for dir in expand_list(
        &var(&format!(
            "CMAKE_{}_STANDARD_LINK_DIRECTORIES",
            info.language
        )),
        false,
    ) {
        link_path.push((format!(" {flag}{}{terminator} ", query.shell(&dir)), None));
    }

    // cmLinkLineComputer::ComputeLinkLibraries.
    let mut fragments: Vec<(String, Vec<Frame>)> = Vec::new();
    let rpath = info.rpath_flags(&reporter);
    if !rpath.is_empty() {
        fragments.push((rpath, Vec::new()));
    }
    for item in info.items.iter().filter(|item| !item.unlinked) {
        let text = if item.is_path {
            item.formatted(&query.shell(&query.link_reference(&item.value)))
        } else {
            item.value.clone()
        };
        fragments.push((format!("{text} "), item.backtrace.clone()));
    }
    let mut rest = String::new();
    if let Some(order) = &info.dependent_search_path {
        let dirs = order.ordered(&reporter).join(":");
        if !info.rpath_link_flag.is_empty() && !dirs.is_empty() {
            rest.push_str(&format!(
                "{}{} ",
                info.rpath_link_flag,
                query.escape(&dirs, 0)
            ));
        }
    }
    let standard = var(&format!("CMAKE_{}_STANDARD_LIBRARIES", info.language));
    if !standard.is_empty() {
        rest.push_str(&format!("{standard} "));
    }
    if !rest.is_empty() {
        fragments.push((rest, Vec::new()));
    }

    let mut shared_symlinks = Vec::new();
    if !head.platform.dll
        && matches!(
            head.kind,
            TargetKind::Executable | TargetKind::SharedLibrary | TargetKind::ModuleLibrary
        )
    {
        for target in info.items.iter().filter_map(|item| item.target.as_deref()) {
            if let Some(target) = query.ctx.interp.ext.targets.get(target).filter(|target| {
                target.kind == TargetKind::SharedLibrary && !target.is_framework_on_apple()
            }) {
                shared_symlinks.push(query.generator.artifact(
                    query.ctx,
                    target,
                    query.config,
                    ArtifactType::Runtime,
                    false,
                ));
            }
        }
    }
    LinkLine {
        libraries: append_flags(fragments.iter().map(|(text, _)| text.as_str())),
        link_path: append_flags(link_path.iter().map(|(text, _)| text.as_str())),
        framework_path,
        depends: info.depends.clone(),
        link_depends: usage_property(query, head, "LINK_DEPENDS"),
        lib_fragments: fragments,
        path_fragments: link_path,
        shared_symlinks,
        rpath_change: info.rpath_change(&reporter),
        rpath_dirs: (info.runtime_use_chrpath
            && is_on_opt(
                query
                    .ctx
                    .var(head.makefile, "CMAKE_PLATFORM_HAS_INSTALLNAME")
                    .as_deref(),
            ))
        .then(|| {
            (
                info.runtime_paths(query.ctx, head, query.config, false),
                info.runtime_paths(query.ctx, head, query.config, true),
            )
        }),
        shared_libraries_linked: info.shared_libraries_linked.clone(),
    }
}

/// `cmLocalGenerator::AppendFlags`.
fn append_flags<'a>(flags: impl Iterator<Item = &'a str>) -> String {
    flags
        .filter(|flag| !flag.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
        .trim()
        .to_owned()
}

/// The artifact of `target` that is linked.
fn link_artifact(query: &Query<'_, '_>, target: &Target) -> ArtifactType {
    if query
        .generator
        .has_import_library(query.ctx, target, query.config)
    {
        ArtifactType::Import
    } else {
        ArtifactType::Runtime
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
        // The objects' source locations are normalized.
        let dir = query.generator.object_dir(query.ctx, target, query.config);
        let dir = crate::util::collapse_full_path(&dir, "");
        query
            .generator
            .sources(query.ctx, target, query.config)
            .into_iter()
            .filter(|source| source.kind == target::SourceKind::ObjectSource)
            .map(|source| format!("{dir}/{}", source.object_name))
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
        // EvaluateInterfaceProperty/AddInterfaceEntries: each interface
        // property entry is checked under the consuming property's checker,
        // so the DAG Top is (head, "LINK_<X>") while the entry's own checker
        // is (owner, "INTERFACE_LINK_<X>").
        let root_property = property.strip_prefix("INTERFACE_").unwrap_or(property);
        entries
            .iter()
            .flat_map(|(value, backtrace)| {
                expand_list(
                    &query.ctx.genex_with_dag_bt(
                        value,
                        owner,
                        head,
                        property,
                        root_property,
                        (Some(query.language), false),
                        backtrace,
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
        values: &mut Vec<(String, Option<String>)>,
    ) {
        for target in items.into_iter().filter_map(|item| item.target) {
            if !seen.insert(std::ptr::from_ref(target) as usize) {
                continue;
            }
            for value in entries(query, target, head, property) {
                values.push((value, Some(target.name.clone())));
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
    let mut values: Vec<(String, Option<String>)> = entries(query, head, head, property)
        .into_iter()
        .map(|value| (value, None))
        .collect();
    // CMP0099 OLD: the transitive interface-property traversal uses
    // UseTo::Compile, so $<LINK_ONLY:...> edges (private dependencies of
    // static libraries) are not followed
    // (cmGeneratorTarget::GetLinkDirectories/GetLinkOptions/GetLinkDepends).
    let compile_query;
    let query = if query.policy(head, "CMP0099") != PolicyStatus::New {
        compile_query = Query {
            compile_usage: true,
            base_computer: query.base_computer,
            ..Query::new(query.ctx, query.config, query.language)
        };
        &compile_query
    } else {
        query
    };
    collect(
        query,
        head,
        &format!("INTERFACE_{property}"),
        implementation(query, head),
        &mut HashSet::from([std::ptr::from_ref(head) as usize]),
        &mut values,
    );
    if property == "LINK_DIRECTORIES" {
        return link_directories(query, head, values);
    }
    unique(values.into_iter().map(|(value, _)| value))
}

/// `processLinkDirectories`: the first relative directory that is an error
/// ends the list.
fn link_directories(
    query: &Query<'_, '_>,
    head: &Target,
    values: Vec<(String, Option<String>)>,
) -> Vec<String> {
    let mut directories = Vec::new();
    for (value, provider) in values {
        if !crate::util::is_full_path(&value) {
            // The target's directory backtrace is attached in all modes.
            let backtrace = query.ctx.dir_backtrace(head.makefile);
            if let Some(provider) = provider {
                query.error_at(
                    backtrace,
                    format!(
                        "Target \"{provider}\" contains relative path in its INTERFACE_LINK_DIRECTORIES:\n  \"{value}\""
                    ),
                );
                break;
            }
            let message = format!(
                "Found relative path while evaluating link directories of \"{}\":\n  \"{value}\"\n",
                head.name
            );
            match query.policy(head, "CMP0081") {
                PolicyStatus::Old => {}
                PolicyStatus::Warn => query.ctx.warning(
                    backtrace,
                    format!(
                        "{}\n{message}",
                        crate::commands::target::policy_warning("CMP0081")
                    ),
                ),
                PolicyStatus::New => {
                    query.error_at(backtrace, message);
                    break;
                }
            }
        }
        directories.push(unix_slashes(&value));
    }
    unique(directories)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum LinkType {
    Unknown,
    Static,
    Shared,
}

/// `SharedDepMode`.
#[derive(Clone, Copy, PartialEq, Eq)]
enum SharedDependencies {
    Ignore,
    /// Add their directories to `-rpath-link`.
    Directory,
    /// Add their directories to the linker search path.
    LibraryDirectory,
    Link,
}

struct Builder<'a, 'q, 'i> {
    query: &'a Query<'q, 'i>,
    head: &'a Target,
    info: LinkInformation,
    /// The targets of `info.items`.
    linked: Vec<&'i Target>,
    features: HashMap<String, Rc<Feature>>,
    groups: HashMap<String, Rc<Feature>>,
    link_depends_no_shared: bool,
    loader_flag: Option<String>,
    lib_flag: String,
    lib_suffix: String,
    with_runtime_path: bool,
    no_soname_uses_path: bool,
    archives_may_be_shared: bool,
    has_install_name: bool,
    /// Flags that switch to static and dynamic mode.
    type_flags: Option<(String, String)>,
    start_type: LinkType,
    current_type: LinkType,
    names: LibraryNames,
    shared_dependencies: SharedDependencies,
    implicit_libraries: HashSet<String>,
    framework_paths_emitted: HashSet<String>,
}

impl<'a, 'q, 'i> Builder<'a, 'q, 'i> {
    /// `cmComputeLinkInformation::cmComputeLinkInformation`.
    fn new(query: &'a Query<'q, 'i>, head: &'a Target) -> Self {
        let language = query.language;
        let var = |name: &str| query.var(head, name);
        let definition = |name: &str| query.ctx.var(head.makefile, name);
        let lookup = |name: &str| {
            definition(&format!("CMAKE_{language}_{name}"))
                .unwrap_or_else(|| var(&format!("CMAKE_{name}")))
        };
        let on_var = |name: &str| is_on(&var(name));

        // On platforms without import libraries there may be a special flag
        // to use when creating a plugin (module) that obtains symbols from
        // the program that will load it.
        let loader_flag = if !head.platform.dll && head.kind == TargetKind::ModuleLibrary {
            definition(&format!("CMAKE_SHARED_MODULE_LOADER_{language}_FLAG"))
        } else {
            None
        };

        // Options needed to specify RPATHs.
        let mut runtime_flag = String::new();
        let mut runtime_sep = String::new();
        let mut runtime_always = String::new();
        let mut runtime_use_chrpath = false;
        let mut rpath_link_flag = String::new();
        if head.kind != TargetKind::StaticLibrary {
            let kind = if head.kind == TargetKind::Executable {
                "EXECUTABLE"
            } else {
                "SHARED_LIBRARY"
            };
            runtime_flag = var(&format!("CMAKE_{kind}_RUNTIME_{language}_FLAG"));
            runtime_sep = var(&format!("CMAKE_{kind}_RUNTIME_{language}_FLAG_SEP"));
            runtime_always = var("CMAKE_PLATFORM_REQUIRED_RUNTIME_PATH");
            runtime_use_chrpath = is_chrpath_used(query, head);
            rpath_link_flag = var(&format!("CMAKE_{kind}_RPATH_LINK_{language}_FLAG"));
        }

        // Feature descriptors for standard library and object link.
        let mut features = HashMap::new();
        let mut standard = |name: &str, format: String| {
            features.insert(name.to_owned(), Rc::new(Feature::of_item(name, format)));
        };
        let library_file_flag = lookup("LINK_LIBRARY_FILE_FLAG");
        if !library_file_flag.is_empty() {
            standard(
                "__CMAKE_LINK_LIBRARY",
                format!("{library_file_flag}<LIBRARY>"),
            );
        }
        let object_file_flag = lookup("LINK_OBJECT_FILE_FLAG");
        if !object_file_flag.is_empty() {
            standard(
                "__CMAKE_LINK_OBJECT",
                format!("{object_file_flag}<LIBRARY>"),
            );
        }
        if let Some(flag) = loader_flag.as_ref().filter(|flag| !flag.is_empty()) {
            standard("__CMAKE_LINK_EXECUTABLE", format!("{flag}<LIBRARY>"));
        }
        standard("__CMAKE_LINK_FRAMEWORK", "<LIBRARY>".to_owned());
        standard("__CMAKE_LINK_XCFRAMEWORK", "<LIBRARY>".to_owned());

        // ComputeLinkTypeInfo.
        let type_flags = match head.kind {
            TargetKind::Executable => Some("EXE"),
            TargetKind::SharedLibrary => Some("SHARED_LIBRARY"),
            TargetKind::ModuleLibrary => Some("SHARED_MODULE"),
            _ => None,
        }
        .map(|kind| {
            (
                var(&format!("CMAKE_{kind}_LINK_STATIC_{language}_FLAGS")),
                var(&format!("CMAKE_{kind}_LINK_DYNAMIC_{language}_FLAGS")),
            )
        })
        .filter(|(fixed, dynamic)| !fixed.is_empty() && !dynamic.is_empty());
        let start_type = if on(head, "LINK_SEARCH_START_STATIC") {
            LinkType::Static
        } else {
            LinkType::Shared
        };

        let names = LibraryNames::new(query, head);

        // ComputeFrameworkInfo: avoid adding implicit framework paths.
        let mut framework_paths_emitted: HashSet<String> = HashSet::new();
        for name in [
            "CMAKE_PLATFORM_IMPLICIT_LINK_FRAMEWORK_DIRECTORIES".to_owned(),
            format!("CMAKE_{language}_IMPLICIT_LINK_FRAMEWORK_DIRECTORIES"),
        ] {
            framework_paths_emitted.extend(expand_list(&var(&name), false));
        }

        // Choose a mode for dealing with shared library dependencies.
        let shared_dependencies = if on_var("CMAKE_LINK_DEPENDENT_LIBRARY_FILES") {
            SharedDependencies::Link
        } else if on_var("CMAKE_LINK_DEPENDENT_LIBRARY_DIRS") {
            SharedDependencies::LibraryDirectory
        } else if !rpath_link_flag.is_empty() {
            SharedDependencies::Directory
        } else {
            SharedDependencies::Ignore
        };

        // The search path entries requested by the user.
        let users = usage_property(query, head, "LINK_DIRECTORIES");

        // LoadImplicitLinkInfo.
        let mut implicit = expand_list(&var("CMAKE_PLATFORM_IMPLICIT_LINK_DIRECTORIES"), false);
        let mut implicit_dirs = BTreeSet::new();
        if let Some(arch) = definition("CMAKE_LIBRARY_ARCHITECTURE") {
            implicit_dirs.extend(implicit.iter().map(|dir| format!("{dir}/{arch}")));
        }
        implicit.extend(expand_list(
            &var(&format!("CMAKE_{language}_IMPLICIT_LINK_DIRECTORIES")),
            false,
        ));
        implicit_dirs.extend(implicit);
        // Items starting in '-' but not '-l' are flags, not libraries, and
        // should not be filtered by this implicit list.
        let implicit_libraries = expand_list(
            &var(&format!("CMAKE_{language}_IMPLICIT_LINK_LIBRARIES")),
            false,
        )
        .into_iter()
        .filter(|item| !item.starts_with('-') || item.starts_with("-l"))
        .collect();

        let mut linker_search_path = OrderDirectories::new("linker search path", &implicit_dirs);
        let mut runtime_search_path = OrderDirectories::new("runtime search path", &implicit_dirs);
        linker_search_path
            .set_link_extensions(names.extensions.clone(), names.remove_extension.clone());
        linker_search_path.add_users(users.clone());
        runtime_search_path.add_users(users);
        let dependent_search_path =
            (shared_dependencies == SharedDependencies::Directory).then(|| {
                let mut order = OrderDirectories::new("dependent library path", &implicit_dirs);
                order.add_languages(expand_list(&var("CMAKE_PLATFORM_RUNTIME_PATH"), false));
                order
            });

        Self {
            query,
            head,
            info: LinkInformation {
                language: language.to_owned(),
                items: Vec::new(),
                depends: Vec::new(),
                framework_paths: Vec::new(),
                shared_libraries_linked: Vec::new(),
                runtime_dlls: Vec::new(),
                linker_search_path,
                runtime_search_path,
                dependent_search_path,
                runtime_flag,
                runtime_sep,
                runtime_always,
                runtime_use_chrpath,
                rpath_link_flag,
                closure_languages: query
                    .generator
                    .closure_languages(query.ctx, head, query.config),
                has_implementation: false,
            },
            linked: Vec::new(),
            features,
            groups: HashMap::new(),
            link_depends_no_shared: on(head, "LINK_DEPENDS_NO_SHARED"),
            loader_flag,
            lib_flag: lookup("LINK_LIBRARY_FLAG"),
            lib_suffix: lookup("LINK_LIBRARY_SUFFIX"),
            with_runtime_path: on_var(&format!(
                "CMAKE_SHARED_LIBRARY_LINK_{language}_WITH_RUNTIME_PATH"
            )),
            no_soname_uses_path: on_var("CMAKE_PLATFORM_USES_PATH_WHEN_NO_SONAME"),
            archives_may_be_shared: is_on_opt(
                query
                    .ctx
                    .interp
                    .global_props
                    .get("TARGET_ARCHIVES_MAY_BE_SHARED_LIBS"),
            ),
            has_install_name: on_var("CMAKE_PLATFORM_HAS_INSTALLNAME"),
            type_flags,
            start_type,
            current_type: start_type,
            names,
            shared_dependencies,
            implicit_libraries,
            framework_paths_emitted,
        }
    }

    fn var(&self, name: &str) -> String {
        self.query.var(self.head, name)
    }

    fn add_entries(&mut self, entries: &[Entry<'i>]) {
        let mut current: Option<Rc<Feature>> = None;
        for entry in entries {
            let item = &entry.item;
            if matches!(entry.kind, Kind::Group | Kind::GroupEnd) {
                let group = self.group_feature(&item.feature);
                if group.supported {
                    if entry.kind == Kind::GroupEnd {
                        self.end_feature(current.take());
                    }
                    let flag = if entry.kind == Kind::Group {
                        &group.prefix
                    } else {
                        &group.suffix
                    };
                    self.push_item(LinkItem::new(flag.clone(), &item.backtrace));
                }
                continue;
            }
            if current
                .as_ref()
                .is_some_and(|feature| feature.name != item.feature)
            {
                self.end_feature(current.take());
            }
            if item.feature != DEFAULT && current.is_none() {
                if !self.add_library_feature(&item.feature) {
                    continue;
                }
                current = self.find_feature(&item.feature);
                if let Some(feature) = current.as_ref().filter(|f| !f.prefix.is_empty()) {
                    self.push_item(LinkItem::new(feature.prefix.clone(), &item.backtrace));
                }
            }
            if entry.kind == Kind::SharedDep {
                self.add_shared_dependency(entry);
            } else {
                self.add_item(entry);
            }
        }
        self.end_feature(current);
    }

    /// Emit the feature suffix, if any.
    fn end_feature(&mut self, feature: Option<Rc<Feature>>) {
        if let Some(feature) = feature.filter(|feature| !feature.suffix.is_empty()) {
            let backtrace = self
                .info
                .items
                .last()
                .map(|item| item.backtrace.clone())
                .unwrap_or_default();
            self.push_item(LinkItem::new(feature.suffix.clone(), &backtrace));
        }
    }

    fn push_item(&mut self, item: LinkItem) {
        self.info.items.push(item);
    }

    fn push_target(&mut self, mut item: LinkItem, target: &'i Target) {
        item.target = Some(self.query.ctx.target_key(target));
        item.unlinked = matches!(
            target.kind,
            TargetKind::InterfaceLibrary | TargetKind::ObjectLibrary
        );
        self.linked.push(target);
        self.push_item(item);
    }

    /// The feature to use for an entry without an explicit one.
    fn feature_or(&self, entry: &Entry<'_>, default: &str) -> Option<Rc<Feature>> {
        self.find_feature(if entry.item.feature == DEFAULT {
            default
        } else {
            &entry.item.feature
        })
    }

    fn path_item(&self, value: &str, entry: &Entry<'_>, default: &str) -> LinkItem {
        LinkItem {
            is_path: true,
            feature: self.feature_or(entry, default),
            ..LinkItem::new(value.to_owned(), &entry.item.backtrace)
        }
    }

    /// A path item built from a plain string, as the framework link names
    /// are: `BT<std::string>` without the link entry's backtrace.
    fn path_item_unbacktraced(&self, value: &str, entry: &Entry<'_>, default: &str) -> LinkItem {
        LinkItem {
            backtrace: Vec::new(),
            ..self.path_item(value, entry, default)
        }
    }

    fn add_item(&mut self, entry: &Entry<'i>) {
        match entry.item.target {
            Some(target) => {
                let has_import = self.query.generator.has_import_library(
                    self.query.ctx,
                    target,
                    self.query.config,
                );
                let import_executable = target.is_executable_with_exports();
                if import_executable && !has_import && self.loader_flag.is_none() {
                    // Skip linking to executables on platforms with no
                    // import libraries or loader flags.
                    return;
                }
                let linkable = import_executable
                    || matches!(
                        target.kind,
                        TargetKind::StaticLibrary
                            | TargetKind::SharedLibrary
                            | TargetKind::ModuleLibrary
                            | TargetKind::UnknownLibrary
                            | TargetKind::ObjectLibrary
                            | TargetKind::InterfaceLibrary
                    );
                if linkable {
                    self.add_linkable(entry, target, has_import);
                } else {
                    self.add_named(entry);
                }
            }
            None => self.add_named(entry),
        }
    }

    /// A CMake target: ask the target for its real name.
    fn add_linkable(&mut self, entry: &Entry<'i>, target: &'i Target, has_import: bool) {
        let query = self.query;
        let artifact = if has_import {
            ArtifactType::Import
        } else {
            ArtifactType::Runtime
        };
        if target.is_executable_with_exports() && self.loader_flag.is_some() {
            // This link item is an executable that may provide symbols used
            // by this target. A special flag is needed on this platform.
            let exe = query
                .generator
                .artifact(query.ctx, target, query.config, artifact, true);
            let item = self.path_item(&exe, entry, "__CMAKE_LINK_EXECUTABLE");
            self.push_target(item, target);
            self.info.depends.push(exe);
        } else if target.kind == TargetKind::InterfaceLibrary {
            // Add the interface library as an item so it can be considered
            // as part of COMPATIBLE_INTERFACE_ enforcement.
            self.push_target(LinkItem::new(String::new(), &[]), target);
            // Also add the item the interface specifies to be used in its
            // place.
            let name = target::import_info(target, query.config)
                .map(|info| info.libname.to_owned())
                .unwrap_or_default();
            if !name.is_empty() {
                self.add_text(name, &entry.item.backtrace);
            }
        } else if target.kind == TargetKind::ObjectLibrary {
            self.push_target(
                LinkItem::new(entry.item.text.clone(), &entry.item.backtrace),
                target,
            );
        } else {
            let lib = query
                .generator
                .artifact(query.ctx, target, query.config, artifact, true);
            if target.platform.aix && lib.ends_with("-NOTFOUND") && artifact == ArtifactType::Import
            {
                // An imported executable on AIX that has ENABLE_EXPORTS but
                // not IMPORTED_IMPLIB.
                return;
            }
            if !self.link_depends_no_shared || target.kind != TargetKind::SharedLibrary {
                self.info.depends.push(lib.clone());
            }
            self.add_target_item(entry, target, &lib);
            if target.platform.apple && has_import {
                // Use the library rather than the tbd file for runpath
                // computation.
                let library = query.generator.artifact(
                    query.ctx,
                    target,
                    query.config,
                    ArtifactType::Runtime,
                    true,
                );
                self.add_target_runtime(&library, target);
            } else {
                self.add_target_runtime(&lib, target);
            }
            if target.kind == TargetKind::SharedLibrary && self.head.platform.dll {
                self.add_runtime_dll(target);
            }
        }
        let xcframework = imported_xcframework_path(target, query.config);
        if !xcframework.is_empty() {
            self.parse_xcframework(&xcframework, &entry.item.backtrace);
        }
    }

    /// An item that is not a CMake target: use the name given.
    fn add_named(&mut self, entry: &Entry<'i>) {
        let item = &entry.item;
        let name = item.text.as_str();
        let default = item.feature == DEFAULT;
        let apple = self.head.platform.apple;
        if item.feature.ends_with("FRAMEWORK") || (default && is_path_to_framework(name) && apple) {
            self.add_framework(entry);
        } else if default && is_path_to_xcframework(name) && apple {
            self.parse_xcframework(name, &item.backtrace);
        } else if Path::new(name).is_absolute() {
            if Path::new(name).is_dir() {
                self.query.ctx.message(
                    MessageType::Warning,
                    item.backtrace.clone(),
                    format!(
                        "Target \"{}\" requests linking to directory \"{name}\".  Targets may link only to libraries.  CMake is dropping the item.",
                        self.head.name
                    ),
                );
            } else {
                // Use the full path given to the library file.
                self.info.depends.push(name.to_owned());
                self.add_full(entry);
                self.add_file_runtime(name);
            }
        } else if entry.kind != Kind::Object {
            // This is a library or option specified by the user.
            self.add_user(entry);
        }
    }

    /// `AddItem` of a plain string.
    fn add_text(&mut self, text: String, backtrace: &[Frame]) {
        let mut item = Item::new(text, None, DEFAULT);
        item.backtrace = backtrace.to_vec();
        self.add_item(&Entry {
            item,
            kind: Kind::Library,
        });
    }

    /// `AddSharedDepItem`.
    fn add_shared_dependency(&mut self, entry: &Entry<'i>) {
        let query = self.query;
        let target = entry.item.target;
        // Record dependencies on DLLs.
        if let Some(target) = target
            && target.kind == TargetKind::SharedLibrary
            && self.head.platform.dll
            && self.shared_dependencies != SharedDependencies::Link
        {
            self.add_runtime_dll(target);
        }
        // If dropping shared library dependencies, ignore them.
        if self.shared_dependencies == SharedDependencies::Ignore {
            return;
        }
        // Skip items that are not full paths to shared libraries.
        let text = &entry.item.text;
        if let Some(target) = target {
            if target.kind != TargetKind::SharedLibrary {
                return;
            }
        } else if !Path::new(text).is_absolute() || !self.names.is_shared(&filename_name(text)) {
            return;
        }
        // If in linking mode, just link to the shared library. An imported
        // shared library without a known runtime artifact, such as a stub,
        // may have no file named with the real soname, so '-rpath-link'
        // cannot help linkers find it.
        if self.shared_dependencies == SharedDependencies::Link
            || target.is_some_and(|target| {
                target.imported
                    && !has_known_runtime_artifact(target, query.config)
                    && matches!(
                        self.var(&format!("CMAKE_{}_COMPILER_LINKER_ID", query.language))
                            .as_str(),
                        "GNU" | "Solaris"
                    )
            })
        {
            self.add_item(entry);
            return;
        }
        // Add it to the runtime path computation so that the target being
        // linked will be able to find it.
        let lib = if let Some(target) = target {
            let artifact = link_artifact(query, target);
            let lib = query
                .generator
                .artifact(query.ctx, target, query.config, artifact, false);
            if target.platform.apple && artifact == ArtifactType::Import {
                let library = query.generator.artifact(
                    query.ctx,
                    target,
                    query.config,
                    ArtifactType::Runtime,
                    true,
                );
                self.add_target_runtime(&library, target);
            } else {
                self.add_target_runtime(&lib, target);
            }
            lib
        } else {
            self.add_file_runtime(text);
            text.clone()
        };
        // Check if we need to include the dependent shared library in other
        // path ordering.
        let soname =
            target.and_then(|target| query.generator.soname(query.ctx, target, query.config));
        if self.shared_dependencies == SharedDependencies::LibraryDirectory {
            if !self.with_runtime_path {
                self.info.linker_search_path.add_runtime(&lib, soname);
            }
        } else if let Some(order) = &mut self.info.dependent_search_path {
            order.add_runtime(&lib, soname);
        }
    }

    fn add_runtime_dll(&mut self, target: &Target) {
        let key = self.query.ctx.target_key(target);
        if !self.info.runtime_dlls.contains(&key) {
            self.info.runtime_dlls.push(key);
        }
    }

    /// `SetCurrentLinkType`.
    fn set_type(&mut self, next: LinkType) {
        if self.current_type == next {
            return;
        }
        self.current_type = next;
        if let Some((fixed, dynamic)) = &self.type_flags {
            let flag = match next {
                LinkType::Static => fixed.clone(),
                LinkType::Shared => dynamic.clone(),
                LinkType::Unknown => return,
            };
            self.push_item(LinkItem::new(flag, &[]));
        }
    }

    /// `AddTargetItem`: a full path to a target.
    fn add_target_item(&mut self, entry: &Entry<'i>, target: &'i Target, path: &str) {
        let query = self.query;
        // If the target is not a static library make sure the link type is
        // shared, because static-mode can handle only static libraries.
        if target.kind != TargetKind::StaticLibrary {
            self.set_type(LinkType::Shared);
        }
        if target.kind == TargetKind::SharedLibrary {
            let key = query.ctx.target_key(target);
            if !self.info.shared_libraries_linked.contains(&key) {
                self.info.shared_libraries_linked.push(key);
            }
        }
        // Handle case of an imported shared library with no soname.
        if self.no_soname_uses_path
            && target.kind == TargetKind::SharedLibrary
            && target::import_info(target, query.config).is_some_and(|info| info.no_soname)
        {
            self.add_without_soname(entry, path);
            return;
        }
        let imported_folder = is_imported_framework_folder(query, target);
        if !target.is_framework_on_apple() && !imported_folder {
            let item = self.path_item(path, entry, "__CMAKE_LINK_LIBRARY");
            self.push_target(item, target);
            return;
        }
        // Add the framework directory and the framework item itself.
        let Some(framework) = split_framework_path(path, FrameworkFormat::Extended) else {
            query.ctx.error(
                entry.item.backtrace.clone(),
                format!(
                    "Could not parse framework path \"{path}\" linked by target {}.",
                    self.head.name
                ),
            );
            return;
        };
        if !framework.directory.is_empty() {
            self.add_framework_path(&framework.directory);
        }
        let item = if entry.item.feature.ends_with("FRAMEWORK") {
            self.path_item_unbacktraced(&framework.link_name(), entry, "")
        } else if entry.item.feature == DEFAULT && imported_folder {
            self.add_library_feature("FRAMEWORK");
            self.path_item_unbacktraced(&framework.link_name(), entry, "FRAMEWORK")
        } else {
            self.path_item(path, entry, "__CMAKE_LINK_LIBRARY")
        };
        self.push_target(item, target);
    }

    /// `AddFullItem`.
    fn add_full(&mut self, entry: &Entry<'i>) {
        let path = entry.item.text.as_str();
        let file = filename_name(path);
        // CheckSharedLibNoSOName.
        if self.no_soname_uses_path && self.names.is_shared(&file) && elf_soname(path).is_none() {
            self.add_without_soname(entry, path);
            return;
        }
        if self.type_flags.is_some() {
            if self.names.is_shared(&file) {
                self.set_type(LinkType::Shared);
            } else if !self.names.is_static(path) {
                // We cannot determine the type. Assume it is the target's
                // default type.
                self.set_type(self.start_type);
            }
        }
        let default = if entry.kind == Kind::Object {
            "__CMAKE_LINK_OBJECT"
        } else {
            "__CMAKE_LINK_LIBRARY"
        };
        let item = self.path_item(path, entry, default);
        self.push_item(item);
    }

    /// `AddUserItem`: not a CMake target and not a full path. A library file
    /// name requests the proper link type from the linker.
    fn add_user(&mut self, entry: &Entry<'i>) {
        let item = &entry.item;
        let name = item.text.as_str();
        if name.starts_with(['-', '$', '`']) {
            // Pass flags through untouched, restoring the link type since
            // this item does not specify one.
            self.set_type(self.start_type);
            self.push_item(LinkItem::new(name.to_owned(), &item.backtrace));
            return;
        }
        if name.starts_with("LINKER:") {
            let flag = linker_wrapper(self.query, self.head, name);
            // The wrapped flags do not keep the backtrace.
            self.push_item(LinkItem::new(flag, &[]));
            return;
        }
        // Search for shared library names first because some platforms have
        // shared libraries with names that match the static library pattern.
        let (next, lib) = if let Some(lib) = self.names.shared_name(name) {
            (LinkType::Shared, lib)
        } else if let Some(lib) = self.names.static_name(name) {
            (LinkType::Static, lib)
        } else if let Some(lib) = self.names.any_name(name) {
            (self.start_type, lib)
        } else {
            (self.start_type, name.to_owned())
        };
        self.set_type(next);
        let base = format!("{lib}{}", self.lib_suffix);
        let out = format!("{}{base}", self.lib_flag);
        let text = if item.feature == DEFAULT {
            out
        } else {
            self.find_feature(&item.feature)
                .map(|feature| feature.decorate(&base, name, &out, false))
                .unwrap_or_default()
        };
        self.push_item(LinkItem::new(text, &item.backtrace));
    }

    /// `AddFrameworkItem`.
    fn add_framework(&mut self, entry: &Entry<'i>) {
        let name = &entry.item.text;
        let default = entry.item.feature == DEFAULT;
        let format = if default {
            FrameworkFormat::Relaxed
        } else {
            FrameworkFormat::Extended
        };
        let Some(framework) = split_framework_path(name, format) else {
            self.query.ctx.system_error(format!(
                "Could not parse framework path \"{name}\" linked by target {}.",
                self.head.name
            ));
            return;
        };
        if !framework.directory.is_empty() {
            self.add_framework_path(&framework.directory);
        }
        self.add_file_runtime(&framework.full_path());
        if default {
            // Ensure the FRAMEWORK feature is loaded.
            self.add_library_feature("FRAMEWORK");
        }
        let item = self.path_item_unbacktraced(&framework.link_name(), entry, "FRAMEWORK");
        self.push_item(item);
    }

    fn add_framework_path(&mut self, path: &str) {
        if self.framework_paths_emitted.insert(path.to_owned()) {
            self.info.framework_paths.push(path.to_owned());
        }
    }

    /// `cmParseXcFrameworkPlist` runs `/usr/bin/plutil`, which only macOS
    /// hosts have.
    fn parse_xcframework(&mut self, path: &str, backtrace: &[Frame]) {
        let text = if Path::new("/usr/bin/plutil").exists() {
            format!("Linking to the XCFramework\n  {path}\nis not supported by cmake-rust.")
        } else {
            format!("Unable to parse plist file:\n  {path}/Info.plist")
        };
        self.query.ctx.error(backtrace.to_vec(), text);
    }

    /// `AddSharedLibNoSOName`: ask the linker to locate the item because
    /// otherwise the path we give to it will be embedded in the target.
    fn add_without_soname(&mut self, entry: &Entry<'i>, path: &str) {
        let mut item = entry.item.clone();
        item.target = None;
        item.text = filename_name(path);
        item.backtrace = Vec::new();
        self.add_user(&Entry {
            item,
            kind: entry.kind,
        });
        // Make sure the link directory ordering will find the library.
        self.info.linker_search_path.add_library(path);
    }

    /// `AddLibraryRuntimeInfo(fullPath, target)`.
    fn add_target_runtime(&mut self, path: &str, target: &Target) {
        let query = self.query;
        // Ignore targets on Apple where install_name is not @rpath.
        if self.has_install_name
            && !super::super::install_name::has_rpath_install_name_dir(
                query.ctx,
                target,
                query.config,
            )
        {
            return;
        }
        // Libraries with unknown type must be handled using just the file on
        // disk.
        if target.kind == TargetKind::UnknownLibrary {
            self.add_file_runtime(path);
            return;
        }
        // Skip targets that are not shared libraries or that do not have a
        // known runtime artifact.
        if target.kind != TargetKind::SharedLibrary
            || !has_known_runtime_artifact(target, query.config)
        {
            return;
        }
        // Only files with the soname could possibly conflict.
        let soname = query.generator.soname(query.ctx, target, query.config);
        self.add_runtime(path, soname);
    }

    /// `AddLibraryRuntimeInfo(fullPath)`.
    fn add_file_runtime(&mut self, path: &str) {
        // GuessLibraryInstallName needs a Mach-O parser, which CMake has
        // only on macOS hosts.
        if self.has_install_name {
            return;
        }
        let file = filename_name(path);
        if self.names.is_shared(&file)
            || (self.archives_may_be_shared && self.names.is_static(&file))
            || split_framework_path(path, FrameworkFormat::Strict).is_some()
        {
            self.add_runtime(path, None);
        }
    }

    fn add_runtime(&mut self, path: &str, soname: Option<String>) {
        if self.with_runtime_path {
            self.info
                .linker_search_path
                .add_runtime(path, soname.clone());
        }
        self.info.runtime_search_path.add_runtime(path, soname);
    }

    /// `AddImplicitLinkInfo`: language runtime libraries and directories.
    fn add_implicit(&mut self) {
        for language in self.info.closure_languages.clone() {
            // These need to go before the other implicit link information as
            // they could require symbols from those other library.
            if language == "CUDA" || language == "HIP" {
                self.add_runtime_library(&language);
            }
            // Skip those of the linker language. They are implicit.
            if language == self.query.language {
                continue;
            }
            self.add_implicit_libraries(
                &self.var(&format!("CMAKE_{language}_IMPLICIT_LINK_LIBRARIES")),
            );
            let dirs = self.var(&format!("CMAKE_{language}_IMPLICIT_LINK_DIRECTORIES"));
            self.info
                .linker_search_path
                .add_languages(expand_list(&dirs, false));
        }
    }

    fn add_implicit_libraries(&mut self, list: &str) {
        for item in expand_list(list, false) {
            if !self.implicit_libraries.contains(&item) {
                self.add_text(item, &[]);
            }
        }
    }

    /// `AddRuntimeLinkLibrary`.
    fn add_runtime_library(&mut self, language: &str) {
        let query = self.query;
        // GetRuntimeLinkLibrary is activated by the presence of a default
        // selection whether or not it is overridden by a property.
        let default = self.var(&format!("CMAKE_{language}_RUNTIME_LIBRARY_DEFAULT"));
        if default.is_empty() {
            return;
        }
        let property = format!("{language}_RUNTIME_LIBRARY");
        let raw = self.head.props.get(&property).unwrap_or(&default);
        let library = query
            .ctx
            .for_config(query.config)
            .genex(raw, Some(self.head), None)
            .to_ascii_uppercase();
        if !library.is_empty() {
            self.add_implicit_libraries(&self.var(&format!(
                "CMAKE_{language}_RUNTIME_LIBRARY_LINK_OPTIONS_{library}"
            )));
        }
    }

    fn find_feature(&self, name: &str) -> Option<Rc<Feature>> {
        self.features.get(name).cloned()
    }

    /// The variable defining a feature: the language specific one, falling
    /// back to the generic one when its support is not specified.
    fn feature_variable(&self, kind: &str, name: &str) -> (String, bool) {
        let definition = |key: &str| {
            self.query
                .ctx
                .var(self.head.makefile, &format!("{key}_SUPPORTED"))
        };
        let key = format!("CMAKE_{}_{kind}_USING_{name}", self.query.language);
        if let Some(supported) = definition(&key) {
            return (key, is_on(&supported));
        }
        let key = format!("CMAKE_{kind}_USING_{name}");
        let supported = definition(&key).is_some_and(|value| is_on(&value));
        (key, supported)
    }

    /// The elements of a feature definition, or the error about it.
    fn feature_definition(&self, kind: &str, name: &str) -> Result<(String, Vec<String>), String> {
        let problem = |problem: &str| {
            format!(
                "Feature '{name}', specified through generator-expression '$<{kind}>' to link target '{}', is not {problem} for the '{}' link language.",
                self.head.name, self.query.language
            )
        };
        let (key, supported) = self.feature_variable(kind, name);
        if !supported {
            return Err(problem("supported"));
        }
        let Some(value) = self.query.ctx.var(self.head.makefile, &key) else {
            return Err(problem("defined"));
        };
        Ok((key, expand_list(&value, true)))
    }

    fn malformed(&self, key: &str, name: &str, problem: &str) -> String {
        format!(
            "Feature '{name}', specified by variable '{key}', is malformed ({problem}) and cannot be used to link target '{}'.",
            self.head.name
        )
    }

    /// `AddLibraryFeature`.
    fn add_library_feature(&mut self, name: &str) -> bool {
        if let Some(found) = self.features.get(name) {
            return found.supported;
        }
        let feature = self.load_library_feature(name).unwrap_or_else(|error| {
            self.query.error_at(self.head.backtrace.clone(), error);
            Feature::default()
        });
        let supported = feature.supported;
        self.features.insert(name.to_owned(), Rc::new(feature));
        supported
    }

    fn load_library_feature(&self, name: &str) -> Result<Feature, String> {
        let (key, items) = self.feature_definition("LINK_LIBRARY", name)?;
        let patterns = |alternative: &str| {
            let tag = if alternative.is_empty() {
                String::new()
            } else {
                format!(" for \"{alternative}{{}}\" alternative")
            };
            self.malformed(
                &key,
                name,
                &format!(
                    "\"<LIBRARY>\", \"<LIB_ITEM>\", or \"<LINK_ITEM>\" patterns are missing{tag}"
                ),
            )
        };
        let format = match items.as_slice() {
            [format] | [_, format, _] => format,
            _ => return Err(self.malformed(&key, name, "wrong number of elements")),
        };
        let valid = |format: &str| {
            ["<LIBRARY>", "<LIB_ITEM>", "<LINK_ITEM>"]
                .iter()
                .any(|pattern| format.contains(pattern))
        };
        if !valid(format) {
            return Err(patterns(""));
        }
        // Now, handle possible "PATH{}" and "NAME{}" patterns.
        let item_path = finalize_format(format, "PATH{", "NAME{");
        let item_name = finalize_format(format, "NAME{", "PATH{");
        if !valid(&item_path) {
            return Err(patterns("PATH"));
        }
        if !valid(&item_name) {
            return Err(patterns("NAME"));
        }
        let wrap = |value: &str| linker_wrapper(self.query, self.head, value);
        let (prefix, suffix) = match items.as_slice() {
            [prefix, _, suffix] => (wrap(prefix), wrap(suffix)),
            _ => Default::default(),
        };
        Ok(Feature {
            name: name.to_owned(),
            supported: true,
            prefix,
            suffix,
            item_path: wrap(&item_path),
            item_name: wrap(&item_name),
        })
    }

    /// `GetGroupFeature`.
    fn group_feature(&mut self, name: &str) -> Rc<Feature> {
        if let Some(found) = self.groups.get(name) {
            return found.clone();
        }
        let loaded =
            self.feature_definition("LINK_GROUP", name)
                .and_then(|(key, items)| match items.as_slice() {
                    [prefix, suffix] => Ok(Feature {
                        name: name.to_owned(),
                        supported: true,
                        prefix: linker_wrapper(self.query, self.head, prefix),
                        suffix: linker_wrapper(self.query, self.head, suffix),
                        ..Feature::default()
                    }),
                    _ => Err(self.malformed(&key, name, "wrong number of elements")),
                });
        let feature = Rc::new(loaded.unwrap_or_else(|error| {
            self.query.error_at(self.head.backtrace.clone(), error);
            Feature::default()
        }));
        self.groups.insert(name.to_owned(), feature.clone());
        feature
    }
}

impl LinkInformation {
    /// `GetRPath`.
    fn rpath(&self, reporter: &Reporter<'_, '_, '_>, for_install: bool) -> Vec<String> {
        let (query, head) = (reporter.query, reporter.target);
        let var = |name: &str| query.var(head, name);
        let on_var = |name: &str| is_on(&var(name));
        // GetRPATH evaluates without a head target.
        let property = |name: &str| {
            head.props.get(name).map(|value| {
                if value.contains("$<") {
                    query.ctx.for_config(query.config).genex_full(
                        value,
                        head.makefile,
                        None,
                        None,
                        true,
                        &[],
                    )
                } else {
                    value.to_owned()
                }
            })
        };
        // Select whether to generate runtime search directories, and whether
        // to generate an rpath for the install tree or the build tree.
        let output_runtime = !on_var("CMAKE_SKIP_RPATH") && !self.runtime_flag.is_empty();
        let for_install = for_install || on(head, "BUILD_WITH_INSTALL_RPATH");
        let skip_install = on_var("CMAKE_SKIP_INSTALL_RPATH");
        let use_install = output_runtime && for_install && !skip_install;
        let build_rpath = property("BUILD_RPATH");
        let use_build = output_runtime
            && !for_install
            && !on(head, "SKIP_BUILD_RPATH")
            && (build_rpath.is_some() || self.has_implementation);
        let use_link = use_install && on(head, "INSTALL_RPATH_USE_LINK_PATH");

        // Select whether to use $ORIGIN in RPATHs for artifacts in the build
        // tree.
        let origin = var("CMAKE_SHARED_LIBRARY_RPATH_ORIGIN_TOKEN");
        let output = filename_path(&query.generator.artifact(
            query.ctx,
            head,
            query.config,
            ArtifactType::Runtime,
            false,
        ));
        let relative =
            on(head, "BUILD_RPATH_USE_ORIGIN") && !origin.is_empty() && !output.is_empty();

        let mut paths = Vec::new();
        if use_install {
            paths.extend(expand_list(
                &property("INSTALL_RPATH").unwrap_or_default(),
                false,
            ));
        }
        if use_build && let Some(list) = &build_rpath {
            paths.extend(expand_list(list, false));
        }
        if use_build || use_link {
            let root = query
                .ctx
                .var(head.makefile, "CMAKE_SYSROOT_LINK")
                .unwrap_or_else(|| var("CMAKE_SYSROOT"));
            let root = unix_slashes(&root);
            let stage = var("CMAKE_STAGING_PREFIX");
            let prefix = var("CMAKE_INSTALL_PREFIX");
            let source = &query.ctx.interp.makefiles[0].source_dir;
            let binary = query.top_binary();
            for dir in self.runtime_search_path.ordered(reporter) {
                // Do not add any path inside the source or build tree when
                // using the link path as an rpath.
                if !use_build && (under(dir, source) || under(dir, binary)) {
                    continue;
                }
                paths.push(if !root.is_empty() && dir.starts_with(&root) {
                    dir[root.len()..].to_owned()
                } else if !stage.is_empty() && dir.starts_with(&stage) {
                    unix_slashes(&format!("{prefix}/{}", &dir[stage.len()..]))
                } else if use_build && relative && under(dir, binary) {
                    let tail = relative_path(&output, dir);
                    if tail.is_empty() {
                        origin.clone()
                    } else {
                        format!("{origin}/{tail}")
                    }
                } else {
                    dir.clone()
                });
            }
        }
        // Add runtime paths required by the languages to always be present.
        // This is done even when skipping rpath support.
        for language in &self.closure_languages {
            if on_var(&format!(
                "CMAKE_{language}_USE_IMPLICIT_LINK_DIRECTORIES_IN_RUNTIME_PATH"
            )) {
                paths.extend(expand_list(
                    &var(&format!("CMAKE_{language}_IMPLICIT_LINK_DIRECTORIES")),
                    false,
                ));
            }
        }
        // Add runtime paths required by the platform to always be present.
        paths.extend(expand_list(&self.runtime_always, false));
        unique(paths)
    }

    /// `GetRPathString`.
    fn rpath_string(&self, reporter: &Reporter<'_, '_, '_>, for_install: bool) -> String {
        let sep = &self.runtime_sep;
        let mut rpath = self.rpath(reporter, for_install).join(sep);
        // If the rpath will be replaced at install time, prepare space.
        if !for_install && self.runtime_use_chrpath {
            if !rpath.is_empty() {
                // One trailing separator so the linker does not reuse the
                // rpath .dynstr entry for a symbol name that happens to
                // match the end of the rpath string.
                rpath.push_str(sep);
            }
            // Make sure it is long enough to hold the replacement value.
            let length = self.rpath_string(reporter, true).len();
            while !sep.is_empty() && rpath.len() < length {
                rpath.push_str(sep);
            }
        }
        rpath
    }

    /// `cmLinkLineComputer::ComputeRPath`.
    fn rpath_flags(&self, reporter: &Reporter<'_, '_, '_>) -> String {
        if self.runtime_sep.is_empty() {
            // Each rpath entry gets its own option ("-R a -R b -R c").
            return self
                .rpath(reporter, false)
                .iter()
                .map(|dir| format!("{}{} ", self.runtime_flag, reporter.query.shell(dir)))
                .collect();
        }
        // All rpath entries are combined ("-Wl,-rpath,a:b:c").
        let rpath = self.rpath_string(reporter, false);
        if rpath.is_empty() {
            return String::new();
        }
        format!("{}{} ", self.runtime_flag, reporter.query.escape(&rpath, 0))
    }

    /// The rpath in the build tree and its replacement at install time.
    fn rpath_change(&self, reporter: &Reporter<'_, '_, '_>) -> Option<(String, String)> {
        (self.runtime_use_chrpath && !self.runtime_sep.is_empty()).then(|| {
            (
                self.rpath_string(reporter, false),
                self.rpath_string(reporter, true),
            )
        })
    }

    /// `GetRPath` of `target`, whose link information this is.
    pub fn runtime_paths(
        &self,
        ctx: &GenCtx,
        target: &Target,
        config: &str,
        for_install: bool,
    ) -> Vec<String> {
        let query = Query::new(ctx, config, &self.language);
        let reporter = Reporter {
            query: &query,
            target,
        };
        self.rpath(&reporter, for_install)
    }
}

/// `cmGeneratorTarget::IsChrpathUsed`.
fn is_chrpath_used(query: &Query<'_, '_>, head: &Target) -> bool {
    let on_var = |name: &str| is_on(&query.var(head, name));
    if !matches!(
        head.kind,
        TargetKind::Executable | TargetKind::SharedLibrary | TargetKind::ModuleLibrary
    ) || !head.have_install_rule
        || on_var("CMAKE_SKIP_RPATH")
        || on(head, "BUILD_WITH_INSTALL_RPATH")
        || on_var("CMAKE_NO_BUILTIN_CHRPATH")
    {
        return false;
    }
    if on_var("CMAKE_PLATFORM_HAS_INSTALLNAME") {
        return true;
    }
    // Enable if the rpath flag uses a separator and the target uses binaries
    // we know how to edit.
    !query
        .var(
            head,
            &format!("CMAKE_SHARED_LIBRARY_RUNTIME_{}_FLAG_SEP", query.language),
        )
        .is_empty()
        && query.var(head, "CMAKE_EXECUTABLE_FORMAT") == "ELF"
}

/// `cmGeneratorTarget::HasKnownRuntimeArtifactLocation`.
fn has_known_runtime_artifact(target: &Target, config: &str) -> bool {
    matches!(
        target.kind,
        TargetKind::Executable | TargetKind::SharedLibrary | TargetKind::ModuleLibrary
    ) && (!target.imported
        || target::import_info(target, config).is_some_and(|info| !info.location.is_empty()))
}

/// `cmGeneratorTarget::IsImportedFrameworkFolderOnApple`.
fn is_imported_framework_folder(query: &Query<'_, '_>, target: &Target) -> bool {
    target.platform.apple
        && target.imported
        && matches!(
            target.kind,
            TargetKind::StaticLibrary | TargetKind::SharedLibrary | TargetKind::UnknownLibrary
        )
        && is_path_to_framework(&query.generator.artifact(
            query.ctx,
            target,
            query.config,
            ArtifactType::Runtime,
            false,
        ))
}

/// `cmGeneratorTarget::GetImportedXcFrameworkPath`.
fn imported_xcframework_path(target: &Target, config: &str) -> String {
    if !target.platform.apple
        || !target.imported
        || !matches!(
            target.kind,
            TargetKind::StaticLibrary | TargetKind::SharedLibrary | TargetKind::UnknownLibrary
        )
    {
        return String::new();
    }
    target::mapped_import_config(target, config)
        .and_then(|found| {
            found.location.or_else(|| {
                target
                    .props
                    .get(&format!("IMPORTED_LOCATION{}", found.suffix))
                    .or_else(|| target.props.get("IMPORTED_LOCATION"))
            })
        })
        .filter(|path| is_path_to_xcframework(path))
        .unwrap_or_default()
        .to_owned()
}

/// `ResolveLinkerWrapper(..., joinItems = true)` in the link language.
fn linker_wrapper(query: &Query<'_, '_>, head: &Target, value: &str) -> String {
    crate::generator::flags::resolve_linker_wrapper_joined(query.ctx, head, query.language, value)
}

/// `FinalizeFeatureFormat`.
fn finalize_format(format: &str, active: &str, other: &str) -> String {
    let mut format = format.to_owned();
    if let Some(start) = format.find(other) {
        let end = format[start..]
            .find('}')
            .map_or(format.len(), |offset| start + offset + 1);
        format.replace_range(start..end, "");
    }
    if let Some(start) = format.find(active) {
        format.replace_range(start..start + active.len(), "");
        if let Some(offset) = format[start..].find('}') {
            format.remove(start + offset);
        }
    }
    format
}

/// `ComputeItemParserInfo`: the regular expressions that recognize library
/// file names.
struct LibraryNames {
    extensions: Vec<String>,
    remove_extension: Option<Regex>,
    any: Option<Regex>,
    fixed: Option<Regex>,
    shared: Option<Regex>,
}

impl LibraryNames {
    fn new(query: &Query<'_, '_>, head: &Target) -> Self {
        let var = |name: &str| query.var(head, name);
        let prefixes: BTreeSet<String> = [
            var("CMAKE_STATIC_LIBRARY_PREFIX"),
            var("CMAKE_SHARED_LIBRARY_PREFIX"),
        ]
        .into_iter()
        .filter(|prefix| !prefix.is_empty())
        .collect();
        let mut extensions: Vec<(String, LinkType)> = vec![
            // Import library names should be matched and treated as shared
            // libraries for the purposes of linking.
            (var("CMAKE_IMPORT_LIBRARY_SUFFIX"), LinkType::Shared),
            (var("CMAKE_STATIC_LIBRARY_SUFFIX"), LinkType::Static),
            (var("CMAKE_SHARED_LIBRARY_SUFFIX"), LinkType::Shared),
            (var("CMAKE_LINK_LIBRARY_SUFFIX"), LinkType::Unknown),
        ];
        for (name, kind) in [
            ("CMAKE_EXTRA_LINK_EXTENSIONS", LinkType::Unknown),
            ("CMAKE_EXTRA_SHARED_LIBRARY_SUFFIXES", LinkType::Shared),
        ] {
            extensions.extend(
                expand_list(&var(name), false)
                    .into_iter()
                    .map(|extension| (extension, kind)),
            );
        }
        extensions.retain(|(extension, _)| !extension.is_empty());
        let openbsd = is_on_opt(
            query
                .ctx
                .interp
                .global_props
                .get("FIND_LIBRARY_USE_OPENBSD_VERSIONING"),
        );
        // CreateExtensionRegex.
        let extension_regex = |kind: LinkType| {
            let choices: Vec<String> = extensions
                .iter()
                .filter(|(_, other)| kind == LinkType::Unknown || *other == kind)
                .map(|(extension, _)| format!("\\{extension}"))
                .collect();
            // An optional OpenBSD-style version or major.minor.version
            // component.
            let version = if openbsd || kind == LinkType::Shared {
                "(\\.[0-9]+)*"
            } else {
                ""
            };
            format!("({}){version}$", choices.join("|"))
        };
        let names: String = prefixes.iter().map(|prefix| format!("{prefix}|")).collect();
        let regex = |kind: LinkType| {
            (kind == LinkType::Unknown || extensions.iter().any(|(_, other)| *other == kind))
                .then(|| compiled(&format!("^({names})([^/:]*){}", extension_regex(kind))))
                .flatten()
        };
        Self {
            remove_extension: compiled(&format!("(.*){}", extension_regex(LinkType::Unknown))),
            any: regex(LinkType::Unknown),
            fixed: regex(LinkType::Static),
            shared: regex(LinkType::Shared),
            extensions: extensions
                .iter()
                .map(|(extension, _)| extension.clone())
                .collect(),
        }
    }

    fn name(regex: &Option<Regex>, text: &str) -> Option<String> {
        let found = regex.as_ref()?.find(text)?;
        Some(
            found.groups[2]
                .map(|(start, end)| text[start..end].to_owned())
                .unwrap_or_default(),
        )
    }

    fn shared_name(&self, text: &str) -> Option<String> {
        Self::name(&self.shared, text)
    }

    fn static_name(&self, text: &str) -> Option<String> {
        Self::name(&self.fixed, text)
    }

    fn any_name(&self, text: &str) -> Option<String> {
        Self::name(&self.any, text)
    }

    fn is_shared(&self, text: &str) -> bool {
        self.shared_name(text).is_some()
    }

    fn is_static(&self, text: &str) -> bool {
        self.static_name(text).is_some()
    }
}

/// Compile `pattern`, reusing earlier results: nearly every target has the
/// same library names.
fn compiled(pattern: &str) -> Option<Regex> {
    thread_local! {
        static CACHE: std::cell::RefCell<HashMap<String, Option<Regex>>> = Default::default();
    }
    CACHE.with(|cache| {
        cache
            .borrow_mut()
            .entry(pattern.to_owned())
            .or_insert_with(|| Regex::new(pattern).ok())
            .clone()
    })
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
