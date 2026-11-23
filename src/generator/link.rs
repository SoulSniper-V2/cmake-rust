//! CMake's link implementation, link dependency ordering, and Ninja link line.

mod dependencies;
mod directories;
mod information;
pub(crate) mod target_graph;

pub(super) use directories::DirectoryContents;
pub(super) use information::{LinkInformation, link_information};

pub use target_graph::TargetDepend;
pub(crate) use target_graph::in_build_system;

#[cfg(test)]
mod tests;

use std::cell::RefCell;
use std::collections::HashSet;
use std::path::Path;
use std::rc::Rc;

use super::ctx::GenCtx;
#[cfg(test)]
use super::target::SourceInfo;
use super::target::{self, ArtifactType};
use crate::messages::{Frame, MessageType};
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
    /// The formatted framework search flags.
    pub framework_path: String,
    /// Files the link step depends on (implicit deps `|`).
    pub depends: Vec<String>,
    /// The LINK_DEPENDS of the target.
    pub link_depends: Vec<String>,
    /// Extra order-only dependencies, appended after target artifacts.
    pub shared_symlinks: Vec<String>,
    /// ELF rpath substitution for an installed target, before shell escaping.
    pub rpath_change: Option<(String, String)>,
    /// Codemodel "libraries" fragments: each emitted string paired with the
    /// link item's backtrace (empty for generated fragments such as rpath).
    pub lib_fragments: Vec<(String, Vec<Frame>)>,
    /// Codemodel "libraryPath" fragments: the flag and the search directory
    /// it names, if that is a link directory.
    pub path_fragments: Vec<(String, Option<String>)>,
}

pub fn link_line(ctx: &GenCtx, target: &Target, config: &str, language: &str) -> LinkLine {
    let query = Query::new(ctx, config, language);
    link_information(ctx, target, config, language)
        .map(|info| information::line(&query, target, &info))
        .unwrap_or_default()
}

/// The targets among the link items of `target`, and the providers of the
/// object files among them, whose modules are forwarded.
pub(super) fn module_link_targets<'i>(
    ctx: &GenCtx<'i>,
    target: &Target,
    language: &str,
) -> Vec<(&'i Target, bool)> {
    let Some(info) = link_information(ctx, target, &ctx.config, language) else {
        return Vec::new();
    };
    info.items
        .iter()
        .filter_map(|item| match &item.target {
            Some(key) => ctx.interp.ext.targets.get(key).map(|dep| (dep, false)),
            None => target::target_object_library(ctx, target.makefile, &item.value)
                .and_then(|name| ctx.target(&name))
                .map(|dep| (dep, true)),
        })
        .collect()
}

/// GetLinkImplementationClosure(config, UseTo::Compile).
pub(super) fn compile_implementation_closure<'i>(
    ctx: &GenCtx<'i>,
    head: &Target,
    config: &str,
) -> Vec<&'i Target> {
    if !target::can_compile_sources(head) || !head.props.contains("LINK_LIBRARIES") {
        return Vec::new();
    }
    fn visit<'i>(
        query: &Query<'_, 'i>,
        head: &Target,
        items: Vec<Item<'i>>,
        seen: &mut HashSet<*const Target>,
        closure: &mut Vec<&'i Target>,
    ) {
        for target in items.into_iter().filter_map(|item| item.target) {
            if seen.insert(std::ptr::from_ref(target)) {
                closure.push(target);
                visit(
                    query,
                    head,
                    interface(query, target, head).libraries,
                    seen,
                    closure,
                );
            }
        }
    }
    let query = Query {
        compile_usage: true,
        ..Query::new(ctx, config, "")
    };
    let mut closure = Vec::new();
    let mut seen = HashSet::from([std::ptr::from_ref(head)]);
    visit(
        &query,
        head,
        implementation(&query, head),
        &mut seen,
        &mut closure,
    );
    closure
}

/// The targets of `GetLinkImplementationLibraries(config, UseTo::Link)`.
pub(super) fn implementation_library_targets<'i>(
    ctx: &GenCtx<'i>,
    target: &Target,
    config: &str,
) -> Vec<&'i Target> {
    if !target::can_compile_sources(target) {
        return Vec::new();
    }
    implementation(&Query::new(ctx, config, ""), target)
        .into_iter()
        .filter_map(|item| item.target)
        .collect()
}

/// The targets of `ComputeLinkImplementationLibraries(config, UseTo::Link)`,
/// for any kind of head target.
pub(super) fn implementation_targets<'i>(
    ctx: &GenCtx<'i>,
    target: &Target,
    config: &str,
    language: &str,
) -> Vec<&'i Target> {
    implementation(&Query::new(ctx, config, language), target)
        .into_iter()
        .filter_map(|item| item.target)
        .collect()
}

/// Direct target-level dependencies after linearizing static-library cycles.
/// `cmGeneratorTarget::GetLinkInterfaceLibraries(config, target,
/// UseTo::Link)`: each item's string and resolved target. Upstream caches
/// this computation, so its diagnostics have already been issued.
pub(crate) fn interface_link_items<'i>(
    ctx: &GenCtx<'i>,
    target: &Target,
    config: &str,
) -> Vec<(String, Option<&'i Target>)> {
    let reported = ctx.errors.borrow().len();
    let query = Query::new(ctx, config, "");
    let items = interface(&query, target, target)
        .libraries
        .into_iter()
        .map(|item| (item.text, item.target))
        .collect();
    ctx.errors.borrow_mut().truncate(reported);
    items
}

/// `cmGlobalGenerator::GetTargetDirectDepends`.
pub fn target_depends(ctx: &GenCtx, target: &Target) -> std::rc::Rc<Vec<TargetDepend>> {
    target_graph::direct_depends(ctx, target)
}

/// `cmGlobalGenerator::CheckTargetLinkLibraries` +
/// `cmGeneratorTarget::CheckLinkLibraries`: verify the memoized link
/// structures at the end of generation. Implementation items are verified
/// when a target's link implementation is computed (`implementation`); the
/// remaining check is the forced `GetLinkInterfaceLibraries(config, this,
/// UseTo::Link)` evaluation for targets with LINK_LIBRARIES_ONLY_TARGETS
/// enabled, which verifies the interface even when nothing consumes it.
pub(crate) fn check_link_libraries(ctx: &GenCtx<'_>) {
    let keys = ctx.interp.ext.target_order.clone();
    for key in keys {
        let Some(target) = ctx.interp.ext.targets.get(&key) else {
            continue;
        };
        if target.kind == TargetKind::GlobalTarget || !on(target, "LINK_LIBRARIES_ONLY_TARGETS") {
            continue;
        }
        let query = Query::new(ctx, &ctx.config, "");
        interface(&query, target, target);
    }
}

/// A `cmLinkItem` for dependency-graph consumers: a target or a string.
pub struct GraphLinkItem<'i> {
    pub name: String,
    pub target: Option<&'i Target>,
}

/// `GetLinkImplementationLibraries(config, UseTo::Link)` and
/// `GetLinkInterfaceLibraries(config, target, UseTo::Compile)` as used by
/// cmLinkItemGraphVisitor; `None` where CMake has no such list.
#[allow(clippy::type_complexity)]
pub fn graph_link_libraries<'i>(
    ctx: &GenCtx<'i>,
    target: &Target,
    config: &str,
) -> (
    Option<Vec<GraphLinkItem<'i>>>,
    Option<Vec<GraphLinkItem<'i>>>,
) {
    let query = Query::new(ctx, config, "");
    let compile_query = Query {
        compile_usage: true,
        ..Query::new(ctx, config, "")
    };
    let convert = |items: Vec<Item<'i>>| -> Vec<GraphLinkItem<'i>> {
        items
            .into_iter()
            .map(|item| GraphLinkItem {
                name: item.text,
                target: item.target,
            })
            .collect()
    };
    // cmTarget::CanCompileSources
    let can_compile = !target.imported
        && matches!(
            target.kind,
            TargetKind::Executable
                | TargetKind::StaticLibrary
                | TargetKind::SharedLibrary
                | TargetKind::ModuleLibrary
                | TargetKind::ObjectLibrary
        );
    let implementation = can_compile.then(|| convert(implementation(&query, target)));
    let has_interface =
        target.imported || target.kind != TargetKind::Executable || on(target, "ENABLE_EXPORTS");
    let interface =
        has_interface.then(|| convert(interface(&compile_query, target, target).libraries));
    (implementation, interface)
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

    fn artifact(
        &self,
        ctx: &GenCtx,
        target: &Target,
        config: &str,
        artifact: ArtifactType,
        real: bool,
    ) -> String {
        target::full_path(&ctx.for_config(config), target, config, artifact, real)
    }

    fn has_import_library(&self, ctx: &GenCtx, target: &Target, config: &str) -> bool {
        target::has_import_library(ctx, target, config)
    }

    /// `cmGeneratorTarget::GetLinkClosure(config)->Languages`.
    fn closure_languages(&self, ctx: &GenCtx, target: &Target, config: &str) -> Vec<String> {
        target::closure_languages(ctx, target, config)
    }

    /// The files that the targets' rules create, by directory.
    fn directory_contents(&self, query: &Query<'_, '_>) -> Rc<RefCell<DirectoryContents>> {
        query
            .ctx
            .directory_contents
            .borrow_mut()
            .get_or_insert_with(|| Rc::new(RefCell::new(manifest(query.ctx))))
            .clone()
    }

    fn soname(&self, ctx: &GenCtx, target: &Target, config: &str) -> Option<String> {
        Some(target::soname(ctx, target, config, ArtifactType::Runtime))
            .filter(|name| !name.is_empty())
    }

    #[cfg(test)]
    fn sources(&self, ctx: &GenCtx, target: &Target, config: &str) -> Vec<SourceInfo> {
        target::sources(ctx, target, config).as_ref().clone()
    }

    #[cfg(test)]
    fn object_dir(&self, ctx: &GenCtx, target: &Target, config: &str) -> String {
        target::object_directory(ctx, target, config)
    }

    fn language(&self, ctx: &GenCtx, target: &Target, config: &str) -> String {
        target::linker_language(ctx, target, config)
    }
}

struct Generator;
impl GeneratorQueries for Generator {}

/// `cmGeneratorTarget::ComputeTargetManifest` of every target.
fn manifest(ctx: &GenCtx) -> DirectoryContents {
    let mut contents = DirectoryContents::default();
    for target in ctx.targets_in_order() {
        if target.imported
            || !matches!(
                target.kind,
                TargetKind::Executable
                    | TargetKind::StaticLibrary
                    | TargetKind::SharedLibrary
                    | TargetKind::ModuleLibrary
            )
        {
            continue;
        }
        for config in &ctx.configs {
            let ctx = ctx.for_config(config);
            let names = target::names(&ctx, target, config);
            let dir = target::directory(&ctx, target, config, ArtifactType::Runtime);
            for name in [names.output, names.shared_object, names.real, names.pdb] {
                contents.add(&dir, name);
            }
            let imports = [names.import_output, names.import_library, names.import_real];
            if imports.iter().any(|name| !name.is_empty()) {
                let dir = target::directory(&ctx, target, config, ArtifactType::Import);
                for name in imports {
                    contents.add(&dir, name);
                }
            }
        }
    }
    contents
}

pub(crate) struct Query<'a, 'i> {
    ctx: &'a GenCtx<'i>,
    config: &'a str,
    language: &'a str,
    generator: &'a dyn GeneratorQueries,
    /// Evaluate for `UseTo::Compile` (usage requirements only).
    compile_usage: bool,
}

impl<'a, 'i> Query<'a, 'i> {
    fn new(ctx: &'a GenCtx<'i>, config: &'a str, language: &'a str) -> Self {
        Self {
            ctx,
            config,
            language,
            generator: &Generator,
            compile_usage: false,
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
        self.eval_entry(raw, owner, head, property, &[])
    }

    fn eval_entry(
        &self,
        raw: &str,
        owner: &Target,
        head: &Target,
        property: &str,
        backtrace: &[crate::messages::Frame],
    ) -> String {
        if raw.contains("$<") {
            let language = (!self.language.is_empty()).then_some(self.language);
            if self.compile_usage || !backtrace.is_empty() {
                return self.ctx.genex_with_dag_usage(
                    (raw, backtrace),
                    owner,
                    head,
                    property,
                    language,
                    self.compile_usage,
                );
            }
            self.generator
                .eval(self.ctx, raw, owner, head, property, language)
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
        self.error_at(Vec::new(), message);
    }

    /// `AUTHOR_WARNING` diagnostic carrying a list-file backtrace
    /// (cmComputeLinkDepends `IssueMessage(AUTHOR_WARNING, ...)` sites).
    fn warn(&self, backtrace: Vec<crate::messages::Frame>, message: impl Into<String>) {
        self.ctx.message(
            crate::messages::MessageType::AuthorWarning,
            backtrace,
            message,
        );
    }

    /// The diagnostic deduplication of `error` with an explicit
    /// `cmListFileBacktrace` (target-creation or property-entry origin).
    fn error_at(&self, backtrace: Vec<Frame>, message: impl Into<String>) {
        let message = message.into();
        if !self
            .ctx
            .errors
            .borrow()
            .iter()
            .any(|existing| existing.text() == message)
        {
            self.ctx.error(backtrace, message);
        }
    }

    /// `cmSystemTools::Error` (once per message, as the C++ computes each
    /// target's link information once).
    fn system_error(&self, message: impl Into<String>) {
        let message = message.into();
        if !self
            .ctx
            .errors
            .borrow()
            .iter()
            .any(|existing| existing.text() == message)
        {
            self.ctx.system_error(message);
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
        let relative = match path.strip_prefix(self.top_binary()) {
            Ok(relative) if relative.as_os_str().is_empty() => ".".into(),
            Ok(relative) => relative.to_string_lossy().into_owned(),
            Err(_) => path.to_string_lossy().into_owned(),
        };
        // cmGlobalNinjaGenerator::NinjaOutputPath (via ConvertToNinjaPath).
        let mut prefix = self
            .ctx
            .var(0, "CMAKE_NINJA_OUTPUT_PATH_PREFIX")
            .unwrap_or_default();
        if !prefix.is_empty() && !prefix.ends_with('/') {
            prefix.push('/');
        }
        if !prefix.is_empty() && !crate::util::is_full_path(&relative) {
            format!("{prefix}{relative}")
        } else {
            relative
        }
    }
}

#[derive(Clone, Debug)]
struct Item<'a> {
    text: String,
    target: Option<&'a Target>,
    feature: String,
    /// `cmLinkItem::ObjectSource`: an explicitly linked object file.
    object: bool,
    /// `cmLinkItem::Backtrace` (the LINK_LIBRARIES-style entry's backtrace);
    /// consumed by the file API codemodel.
    backtrace: Vec<crate::messages::Frame>,
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
            object: false,
            backtrace: Vec::new(),
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
    plain_items_bt(query, owner, value, &[], &mut directory, None)
}

/// `plain_items` tagging each produced item with `backtrace`; `directory`
/// persists across calls so `::@` markers in earlier entries keep applying.
/// `self_check` is `Some(head)` while scanning head's own LINK_LIBRARIES
/// entries, where cmGeneratorTarget::ComputeLinkImplementationLibraries
/// raises the "links to itself" diagnostic.
fn plain_items_bt<'i>(
    query: &Query<'_, 'i>,
    owner: &Target,
    value: &str,
    backtrace: &[crate::messages::Frame],
    directory: &mut usize,
    self_check: Option<&Target>,
) -> Vec<Item<'i>> {
    let mut feature = DEFAULT.to_owned();
    let mut items = Vec::new();
    for entry in expand_list(value, false) {
        if entry == "::@" {
            *directory = owner.makefile;
            continue;
        }
        if let Some(id) = entry
            .strip_prefix("::@(0x")
            .and_then(|id| id.strip_suffix(')'))
            && let Ok(index) = usize::from_str_radix(id, 16)
            && query.ctx.interp.makefiles.get(index).is_some()
        {
            *directory = index;
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
            query.error_at(owner.backtrace.clone(), format!("Target \"{}\" links to item \"{}\" which has leading or trailing whitespace.  This is now an error according to policy CMP0004.", owner.name, entry));
        }
        if !name.is_empty() {
            let target = query.resolve(*directory, name);
            if let Some(head) = self_check {
                // ComputeLinkImplementationLibraries: under CMP0108 NEW the
                // name is alias-resolved (FindTargetToUse) before comparing
                // to the target's own name. The diagnostic carries the
                // target's backtrace and aborts the computation.
                let self_link = if query.policy(head, "CMP0108") == PolicyStatus::New {
                    target.is_some_and(|target| std::ptr::eq(target, head)) || name == head.name
                } else {
                    name == head.name
                };
                if self_link {
                    query.error_at(
                        head.backtrace.clone(),
                        format!("Target \"{}\" links to itself.", head.name),
                    );
                    return items;
                }
            }
            let target = target.filter(|target| {
                target.kind != TargetKind::Executable || on(target, "ENABLE_EXPORTS")
            });
            // cmGeneratorTarget::ResolveLinkItem: deprecation warning at
            // the link item's backtrace.
            if let Some(deprecation) = target
                .and_then(|target| target.props.get("DEPRECATION"))
                .filter(|value| !value.is_empty())
            {
                let name = target.map_or(name.to_owned(), |target| target.name.clone());
                query.ctx.message(
                    MessageType::AuthorWarning,
                    backtrace.to_vec(),
                    format!("The library that is being linked to, {name}, is marked as being deprecated by the owner.  The message provided by the developer is: \n{deprecation}\n"),
                );
            }
            let mut item = Item::new(name, target, &feature);
            item.backtrace = backtrace.to_vec();
            item.object =
                target.is_none() && target::is_external_object(query.ctx, owner.makefile, name);
            items.push(item);
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
        // cmGeneratorTarget::VerifyLinkItem*: the item's own backtrace,
        // falling back to the target's creation backtrace.
        let backtrace = if item.backtrace.is_empty() {
            owner.backtrace.clone()
        } else {
            item.backtrace.clone()
        };
        if name.contains("::") {
            let role = if interface {
                format!("The link interface of target \"{}\" contains", owner.name)
            } else {
                format!("Target \"{}\" links to", owner.name)
            };
            query.error_at(
                backtrace,
                format!("{role}:\n  {name}\nbut the target was not found.  {REASONS}"),
            );
            return;
        }
        if only_targets && !name.starts_with(['-', '$', '`']) && !name.contains(['/', '\\']) {
            let role = if interface {
                "its link interface contains"
            } else {
                "it links to"
            };
            query.error_at(backtrace, format!("Target \"{}\" has LINK_LIBRARIES_ONLY_TARGETS enabled, but {role}:\n  {name}\nwhich is not a target.  {REASONS}", owner.name));
            return;
        }
    }
}

/// Provenance-tracked link items of `property` on `owner`: one evaluated
/// item list per recorded property entry, each tagged with the entry's
/// backtrace (`EvaluateTargetPropertyEntries` semantics).
fn provenanced_items<'i>(
    query: &Query<'_, 'i>,
    owner: &Target,
    head: &Target,
    property: &str,
) -> Vec<Item<'i>> {
    let entries: Vec<(String, Vec<crate::messages::Frame>)> = match owner.provenance.get(property) {
        Some(entries) if !entries.is_empty() => entries.clone(),
        _ => match owner.props.get(property) {
            Some(value) => vec![(value.to_owned(), Vec::new())],
            None => Vec::new(),
        },
    };
    let mut items = Vec::new();
    let mut directory = owner.makefile;
    let self_check = (property == "LINK_LIBRARIES" && std::ptr::eq(owner, head)).then_some(head);
    for (value, backtrace) in entries {
        let evaluated = query.eval_entry(&value, owner, head, property, &backtrace);
        items.extend(plain_items_bt(
            query,
            owner,
            &evaluated,
            &backtrace,
            &mut directory,
            self_check,
        ));
    }
    items
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
    let mut libraries = if property == "INTERFACE_LINK_LIBRARIES" {
        provenanced_items(query, owner, head, property)
    } else {
        plain_items(
            query,
            owner,
            &query.eval(value.unwrap_or_default(), owner, head, property),
        )
    };
    libraries.retain(|item| {
        item.target
            .is_none_or(|target| !std::ptr::eq(target, owner))
    });
    validate_items(query, owner, &libraries, true);
    Interface {
        libraries,
        include: provenanced_items(query, owner, head, "INTERFACE_LINK_LIBRARIES_DIRECT"),
        exclude: provenanced_items(
            query,
            owner,
            head,
            "INTERFACE_LINK_LIBRARIES_DIRECT_EXCLUDE",
        ),
    }
}

fn implementation<'i>(query: &Query<'_, 'i>, head: &Target) -> Vec<Item<'i>> {
    let original = provenanced_items(query, head, head, "LINK_LIBRARIES");
    let mut direct = DirectLinks {
        query,
        head,
        emitted: original.iter().map(Item::identity).collect(),
        followed: HashSet::new(),
        excluded: HashSet::new(),
        items: Vec::new(),
    };
    let synthetic = &query.ctx.interp.ext.synthetic_deps;
    let synthetic = if synthetic.is_empty() {
        None
    } else {
        synthetic.get(&(query.ctx.target_key(head), query.config.to_owned()))
    };
    for item in original {
        // cmGeneratorTarget::ComputeLinkImplementationLibraries: each
        // imported target with discovered synthetic C++ module targets
        // contributes them as direct link items (before the item itself).
        if let Some(target) = item.target
            && let Some(deps) =
                synthetic.and_then(|synthetic| synthetic.get(&query.ctx.target_key(target)))
        {
            for dep_key in deps {
                if let Some(dep) = query.ctx.interp.ext.targets.get(dep_key) {
                    let mut synth = Item::new(dep.name.clone(), Some(dep), &item.feature);
                    synth.backtrace = item.backtrace.clone();
                    direct.follow(item.target);
                    direct.items.push(synth);
                }
            }
        }
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

fn unique(values: impl IntoIterator<Item = String>) -> Vec<String> {
    let mut seen = HashSet::new();
    values
        .into_iter()
        .filter(|value| seen.insert(value.clone()))
        .collect()
}
