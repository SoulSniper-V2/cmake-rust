//! CMake's link implementation, link dependency ordering, and Ninja link line.

mod dependencies;
mod directories;
mod information;
mod target_graph;

#[cfg(test)]
mod tests;

use std::collections::HashSet;
use std::path::Path;

use super::ctx::GenCtx;
use super::target::{self, ArtifactType, SourceInfo};
use crate::policy::{PolicyStatus, index_of};
use crate::state::{Target, TargetKind};
use crate::util::{expand_list, is_on, is_on_opt};

const DEFAULT: &str = "DEFAULT";

/// Values for a Ninja link statement.
#[derive(Clone, Debug, Default)]
pub struct LinkLine {
    /// The formatted `LINK_LIBRARIES` value, including rpath flags.
    pub libraries: String,
    /// The formatted `LINK_PATH` value.
    pub link_path: String,
    /// Files the link step depends on (implicit deps `|`).
    pub depends: Vec<String>,
    /// Extra order-only dependencies, appended after target artifacts.
    #[allow(dead_code, reason = "The Ninja writer will append version symlinks")]
    pub shared_symlinks: Vec<String>,
}

pub fn link_line(ctx: &GenCtx, target: &Target, config: &str, language: &str) -> LinkLine {
    let query = Query::new(ctx, config, language);
    information::compute(&query, target)
}

/// Direct target-level dependencies after linearizing static-library cycles.
pub fn target_dependencies(ctx: &GenCtx, target: &Target, config: &str) -> Vec<String> {
    target_graph::dependencies(ctx, target, config)
}

trait GeneratorQueries {
    fn eval(
        &self,
        ctx: &GenCtx,
        text: &str,
        owner: &Target,
        head: &Target,
        property: &str,
        language: Option<&str>,
    ) -> String {
        ctx.genex_with_dag(text, owner, head, property, language)
    }

    fn artifact(&self, ctx: &GenCtx, target: &Target, config: &str, real: bool) -> String {
        target::full_path(ctx, target, config, ArtifactType::Runtime, real)
    }

    fn soname(&self, ctx: &GenCtx, target: &Target, config: &str) -> Option<String> {
        if target.imported {
            if is_on_opt(import_property(target, config, "IMPORTED_NO_SONAME")) {
                return import_property(target, config, "IMPORTED_LOCATION")
                    .map(crate::util::filename_name);
            }
            return import_property(target, config, "IMPORTED_SONAME")
                .filter(|value| !value.is_empty())
                .map(str::to_owned);
        }
        target::has_soname(ctx, target, config)
            .then(|| target::names(ctx, target, config).shared_object)
            .filter(|name| !name.is_empty())
    }

    fn sources(&self, ctx: &GenCtx, target: &Target, config: &str) -> Vec<SourceInfo> {
        target::sources(ctx, target, config)
    }

    fn object_dir(&self, ctx: &GenCtx, target: &Target, config: &str) -> String {
        target::object_directory(ctx, target, config)
    }

    fn language(&self, ctx: &GenCtx, target: &Target, config: &str) -> String {
        target::linker_language(ctx, target, config)
    }
}

struct Generator;
impl GeneratorQueries for Generator {}

struct Query<'a, 'i> {
    ctx: &'a GenCtx<'i>,
    config: &'a str,
    language: &'a str,
    generator: &'a dyn GeneratorQueries,
}

impl<'a, 'i> Query<'a, 'i> {
    fn new(ctx: &'a GenCtx<'i>, config: &'a str, language: &'a str) -> Self {
        Self {
            ctx,
            config,
            language,
            generator: &Generator,
        }
    }

    fn var(&self, target: &Target, name: &str) -> String {
        self.ctx.var_safe(target.makefile, name)
    }

    fn property(&self, owner: &Target, head: &Target, property: &str) -> String {
        let raw = owner.props.get(property).unwrap_or_default();
        self.eval(raw, owner, head, property)
    }

    fn eval(&self, raw: &str, owner: &Target, head: &Target, property: &str) -> String {
        if raw.contains("$<") {
            let language = (!self.language.is_empty()).then_some(self.language);
            self.generator.eval(self.ctx, raw, owner, head, property, language)
        } else {
            raw.to_owned()
        }
    }

    fn resolve(&self, directory: usize, name: &str) -> Option<&'i Target> {
        // Local imported names can have more than one internal storage key.
        let targets = &self.ctx.interp.ext.targets;
        // INTEGRATION: Resolve directory-scoped aliases through the target
        // model's directory-aware target lookup when integrated.
        let mut scope = Some(directory);
        let mut resolved = self.ctx.target(name);
        while let Some(index) = scope {
            if let Some(local) = targets.get(&format!("[{index}]{name}")) {
                return Some(local);
            }
            if let Some(local) = resolved
                .filter(|target| target.imported && target.makefile == index)
                .or_else(|| {
                    // Directory-scoped imported targets are registered under
                    // scoped keys (indexed by name).
                    self.ctx
                        .interp
                        .ext
                        .scoped_keys_by_name
                        .get(name)?
                        .iter()
                        .filter_map(|key| targets.get(key))
                        .find(|target| {
                            target.imported && !target.imported_global && target.makefile == index
                        })
                })
            {
                return Some(local);
            }
            scope = self
                .ctx
                .interp
                .makefiles
                .get(index)
                .and_then(|mf| mf.parent);
        }
        if let Some(alias) = resolved.and_then(|target| target.alias_of.as_deref()) {
            resolved = self.ctx.target(alias);
        }
        resolved.filter(|target| !target.imported || target.imported_global)
    }

    fn error(&self, message: impl Into<String>) {
        let message = message.into();
        if !self
            .ctx
            .errors
            .borrow()
            .iter()
            .any(|(_, existing)| existing == &message)
        {
            self.ctx.error(Vec::new(), message);
        }
    }

    fn policy(&self, target: &Target, id: &str) -> PolicyStatus {
        let Some(index) = index_of(id) else {
            return PolicyStatus::Warn;
        };
        target::target_policy(self.ctx, target, index)
    }

    fn top_binary(&self) -> &str {
        &self.ctx.interp.makefiles[0].binary_dir
    }

    fn ninja_path(&self, path: &str) -> String {
        let path = Path::new(path);
        match path.strip_prefix(self.top_binary()) {
            Ok(relative) if relative.as_os_str().is_empty() => ".".into(),
            Ok(relative) => relative.to_string_lossy().into_owned(),
            Err(_) => path.to_string_lossy().into_owned(),
        }
    }
}

#[derive(Clone, Debug)]
struct Item<'a> {
    text: String,
    target: Option<&'a Target>,
    feature: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum Identity {
    Target(usize),
    Text(String),
}

impl<'a> Item<'a> {
    fn new(text: impl Into<String>, target: Option<&'a Target>, feature: &str) -> Self {
        let text = target.map_or_else(|| text.into(), |target| target.name.clone());
        Self {
            text,
            target,
            feature: feature.to_owned(),
        }
    }

    fn identity(&self) -> Identity {
        self.target.map_or_else(
            || Identity::Text(self.text.clone()),
            |target| Identity::Target(std::ptr::from_ref(target) as usize),
        )
    }

    fn group(&self) -> Option<&str> {
        self.text
            .strip_prefix("<LINK_GROUP:")?
            .strip_suffix('>')?
            .split(':')
            .next()
    }
}

#[derive(Default)]
struct Interface<'a> {
    libraries: Vec<Item<'a>>,
    include: Vec<Item<'a>>,
    exclude: Vec<Item<'a>>,
}

fn plain_items<'i>(query: &Query<'_, 'i>, owner: &Target, value: &str) -> Vec<Item<'i>> {
    let mut directory = owner.makefile;
    let mut feature = DEFAULT.to_owned();
    let mut items = Vec::new();
    for entry in expand_list(value, false) {
        if entry == "::@" {
            directory = owner.makefile;
            continue;
        }
        if let Some(id) = entry
            .strip_prefix("::@(0x")
            .and_then(|id| id.strip_suffix(')'))
            && let Ok(index) = usize::from_str_radix(id, 16)
            && query.ctx.interp.makefiles.get(index).is_some()
        {
            directory = index;
            continue;
        }
        if let Some(name) = entry
            .strip_prefix("<LINK_LIBRARY:")
            .and_then(|tag| tag.strip_suffix('>'))
        {
            feature = name.to_owned();
            continue;
        }
        if entry.starts_with("</LINK_LIBRARY:") && entry.ends_with('>') {
            feature = DEFAULT.to_owned();
            continue;
        }

        let name = entry.trim();
        if name != entry {
            query.error(format!("Target \"{}\" links to item \"{}\" which has leading or trailing whitespace.  This is now an error according to policy CMP0004.", owner.name, entry));
        }
        if !name.is_empty() {
            let target = query.resolve(directory, name).filter(|target| {
                target.kind != TargetKind::Executable || on(target, "ENABLE_EXPORTS")
            });
            items.push(Item::new(name, target, &feature));
        }
    }
    items
}

fn validate_items(query: &Query<'_, '_>, owner: &Target, items: &[Item<'_>], interface: bool) {
    const REASONS: &str = "Possible reasons include:\n  * There is a typo in the target name.\n  * A find_package call is missing for an IMPORTED target.\n  * An ALIAS target is missing.\n";
    let only_targets = on(owner, "LINK_LIBRARIES_ONLY_TARGETS");
    for item in items.iter().filter(|item| item.target.is_none()) {
        let name = &item.text;
        if name.starts_with("<LINK_GROUP:") || name.starts_with("</LINK_GROUP:") {
            continue;
        }
        if name.contains("::") {
            let role = if interface {
                format!("The link interface of target \"{}\" contains", owner.name)
            } else {
                format!("Target \"{}\" links to", owner.name)
            };
            query.error(format!(
                "{role}:\n  {name}\nbut the target was not found.  {REASONS}"
            ));
            return;
        }
        if only_targets && !name.starts_with(['-', '$', '`']) && !name.contains(['/', '\\']) {
            let role = if interface {
                "its link interface contains"
            } else {
                "it links to"
            };
            query.error(format!("Target \"{}\" has LINK_LIBRARIES_ONLY_TARGETS enabled, but {role}:\n  {name}\nwhich is not a target.  {REASONS}", owner.name));
            return;
        }
    }
}

fn interface<'i>(query: &Query<'_, 'i>, owner: &Target, head: &Target) -> Interface<'i> {
    let (value, property) = match owner.props.get("INTERFACE_LINK_LIBRARIES") {
        Some(value) => (Some(value), "INTERFACE_LINK_LIBRARIES"),
        None => (
            (owner.imported && owner.kind != TargetKind::InterfaceLibrary)
                .then(|| import_property(owner, query.config, "IMPORTED_LINK_INTERFACE_LIBRARIES"))
                .flatten(),
            "IMPORTED_LINK_INTERFACE_LIBRARIES",
        ),
    };
    let mut libraries = plain_items(query, owner, &query.eval(value.unwrap_or_default(), owner, head, property));
    libraries.retain(|item| {
        item.target
            .is_none_or(|target| !std::ptr::eq(target, owner))
    });
    validate_items(query, owner, &libraries, true);
    Interface {
        libraries,
        include: plain_items(
            query,
            owner,
            &query.property(owner, head, "INTERFACE_LINK_LIBRARIES_DIRECT"),
        ),
        exclude: plain_items(
            query,
            owner,
            &query.property(owner, head, "INTERFACE_LINK_LIBRARIES_DIRECT_EXCLUDE"),
        ),
    }
}

fn implementation<'i>(query: &Query<'_, 'i>, head: &Target) -> Vec<Item<'i>> {
    let value = query.property(head, head, "LINK_LIBRARIES");
    let mut original = plain_items(query, head, &value);
    original.retain(|item| {
        if item.target.is_some_and(|target| std::ptr::eq(target, head)) || item.text == head.name {
            query.error(format!("Target \"{}\" links to itself.", head.name));
            false
        } else {
            true
        }
    });
    let mut direct = DirectLinks {
        query,
        head,
        emitted: original.iter().map(Item::identity).collect(),
        followed: HashSet::new(),
        excluded: HashSet::new(),
        items: Vec::new(),
    };
    for item in original {
        direct.follow(item.target);
        direct.items.push(item);
    }
    direct
        .items
        .retain(|item| !direct.excluded.contains(&item.identity()));
    validate_items(query, head, &direct.items, false);
    direct.items
}

struct DirectLinks<'a, 'q, 'i> {
    query: &'a Query<'q, 'i>,
    head: &'a Target,
    emitted: HashSet<Identity>,
    followed: HashSet<usize>,
    excluded: HashSet<Identity>,
    items: Vec<Item<'i>>,
}

impl<'i> DirectLinks<'_, '_, 'i> {
    fn follow(&mut self, owner: Option<&'i Target>) {
        let Some(owner) = owner else { return };
        if !self.followed.insert(std::ptr::from_ref(owner) as usize) {
            return;
        }
        let iface = interface(self.query, owner, self.head);
        for item in iface.include {
            self.follow(item.target);
            if self.emitted.insert(item.identity()) {
                self.items.push(item);
            }
        }
        for item in iface.libraries {
            self.follow(item.target);
        }
        self.excluded
            .extend(iface.exclude.iter().map(Item::identity));
    }
}

fn imported_suffix(target: &Target, config: &str) -> String {
    let config = config_suffix(config);
    let base = match target.kind {
        TargetKind::InterfaceLibrary => "IMPORTED_LIBNAME",
        TargetKind::ObjectLibrary => "IMPORTED_OBJECTS",
        _ => "IMPORTED_LOCATION",
    };
    let has_location = |suffix: &str| {
        target.props.contains(&format!("{base}{suffix}"))
            || ((target.kind == TargetKind::SharedLibrary || on(target, "ENABLE_EXPORTS"))
                && target.props.contains(&format!("IMPORTED_IMPLIB{suffix}")))
    };
    if let Some(mapping) = target.props.get(&format!("MAP_IMPORTED_CONFIG{config}")) {
        return expand_list(mapping, true)
            .iter()
            .map(|name| {
                if name.is_empty() {
                    String::new()
                } else {
                    config_suffix(name)
                }
            })
            .find(|suffix| has_location(suffix))
            .unwrap_or(config);
    }
    if has_location(&config) {
        return config;
    }
    if has_location("") {
        return String::new();
    }
    expand_list(
        target
            .props
            .get("IMPORTED_CONFIGURATIONS")
            .unwrap_or_default(),
        false,
    )
    .iter()
    .map(|name| config_suffix(name))
    .find(|suffix| has_location(suffix))
    .unwrap_or_default()
}

fn import_property<'a>(target: &'a Target, config: &str, property: &str) -> Option<&'a str> {
    let suffix = imported_suffix(target, config);
    target
        .props
        .get(&format!("{property}{suffix}"))
        .or_else(|| target.props.get(property))
}

fn config_suffix(config: &str) -> String {
    if config.is_empty() {
        "_NOCONFIG".into()
    } else {
        format!("_{}", config.to_ascii_uppercase())
    }
}

fn on(target: &Target, name: &str) -> bool {
    is_on_opt(target.props.get(name))
}

fn dependency_languages(target: &Target, query: &Query<'_, '_>) -> Vec<String> {
    if target.imported {
        return expand_list(
            import_property(target, query.config, "IMPORTED_LINK_INTERFACE_LANGUAGES")
                .unwrap_or_default(),
            false,
        );
    }
    if !matches!(
        target.kind,
        TargetKind::Executable
            | TargetKind::StaticLibrary
            | TargetKind::SharedLibrary
            | TargetKind::ModuleLibrary
            | TargetKind::ObjectLibrary
    ) {
        return Vec::new();
    }
    query
        .generator
        .sources(query.ctx, target, query.config)
        .into_iter()
        .filter(|source| {
            source.kind == target::SourceKind::ObjectSource && !source.language.is_empty()
        })
        .map(|source| source.language)
        .collect()
}

fn unique(values: impl IntoIterator<Item = String>) -> Vec<String> {
    let mut seen = HashSet::new();
    values
        .into_iter()
        .filter(|value| seen.insert(value.clone()))
        .collect()
}
