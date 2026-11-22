//! Ports of cmGeneratorTarget_TransitiveProperty.cxx, the link-item parts of
//! cmGeneratorTarget_Link.cxx, TargetPropertyNode, and the usage-requirement
//! collectors in cmGeneratorTarget_{IncludeDirectories,Options,LinkDirectories}.cxx
//! together with cmEvaluatedTargetProperty.cxx.

use super::dag::Check;
use super::*;

/// One value of a usage requirement, as cmGeneratorTarget::Get* returns it
/// (`BT<std::string>`), plus the entry's origin.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UsageItem {
    pub value: String,
    /// cmLinkImplItem::AsStr() of the entry that produced this value. `None`
    /// for the target's own property entries; otherwise the directly linked
    /// target whose (transitive) INTERFACE_ property supplied the value.
    pub link_item: Option<String>,
    /// The entry's backtrace: the property entry itself, or for linked
    /// targets the backtrace of the link item.
    pub backtrace: Option<Backtrace>,
    /// cmGeneratorTarget::IsSystemIncludeDirectory. Only set for
    /// INCLUDE_DIRECTORIES.
    pub system: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LinkDependency {
    pub target: String,
    pub backtrace: Option<Backtrace>,
}

/// cmGeneratorTarget::BuiltinTransitiveProperties.
const COMPILE_PROPERTIES: &[&str] = &[
    "AUTOMOC_MACRO_NAMES",
    "AUTOUIC_OPTIONS",
    "COMPILE_DEFINITIONS",
    "COMPILE_FEATURES",
    "COMPILE_OPTIONS",
    "INCLUDE_DIRECTORIES",
    "PRECOMPILE_HEADERS",
    "SOURCES",
    "SYSTEM_INCLUDE_DIRECTORIES",
];
const LINK_PROPERTIES: &[&str] = &[
    "LINK_DEPENDS",
    "LINK_DIRECTORIES",
    "LINK_LIBRARIES",
    "LINK_OPTIONS",
];

/// EvaluatedTargetPropertyEntry.
struct Entry {
    link_item: Option<String>,
    backtrace: Option<Backtrace>,
    values: Vec<String>,
}

#[derive(Default)]
struct DirectLinkItems {
    emitted: HashSet<String>,
    excluded: HashSet<String>,
    followed: HashSet<String>,
    items: Vec<LinkDependency>,
}

const DL_BEGIN: &str = "<DEVICE_LINK>";
const DL_END: &str = "</DEVICE_LINK>";

impl Evaluator<'_, '_> {
    /// cmGeneratorTarget::IsTransitiveProperty.
    pub(super) fn transitive(
        &mut self,
        target: &str,
        property: &str,
        computing_links: bool,
    ) -> Result<Option<Usage>, GenexError> {
        let interface = property.starts_with("INTERFACE_");
        let name = property.strip_prefix("INTERFACE_").unwrap_or(property);
        if COMPILE_PROPERTIES.contains(&name)
            || (LINK_PROPERTIES.contains(&name)
                && (name != "LINK_LIBRARIES"
                    || self.host.local_policy_status("CMP0189") == PolicyStatus::New))
        {
            if COMPILE_PROPERTIES.contains(&name) {
                return Ok(Some(Usage::Compile));
            }
            if name != "LINK_LIBRARIES"
                && self.host.local_policy_status("CMP0166") != PolicyStatus::New
            {
                return Ok(Some(Usage::Compile));
            }
            return Ok(Some(Usage::Link));
        }
        if computing_links {
            return Ok(None);
        }
        // GetCustomTransitiveProperties: TRANSITIVE_LINK_PROPERTIES entries
        // are added first and are never replaced by compile ones.
        for (variable, usage) in [
            ("TRANSITIVE_LINK_PROPERTIES", Usage::Link),
            ("TRANSITIVE_COMPILE_PROPERTIES", Usage::Compile),
        ] {
            if self.custom_property(target, variable, name) {
                return Ok(Some(usage));
            }
            for dependency in self.closure(target, usage, interface)? {
                if self.custom_property(&dependency, variable, name) {
                    return Ok(Some(usage));
                }
            }
        }
        Ok(None)
    }

    fn custom_property(&self, target: &str, variable: &str, name: &str) -> bool {
        self.host
            .target_property_raw(target, variable)
            .is_some_and(|value| {
                cm_list(&value, false)
                    .iter()
                    .any(|p| p.strip_prefix("INTERFACE_").unwrap_or(p) == name)
            })
    }

    /// cmGeneratorTarget::GetLinkImplementationClosure (`interface == false`)
    /// or GetLinkInterfaceClosure with the target as its own head.
    fn closure(
        &mut self,
        target: &str,
        usage: Usage,
        interface: bool,
    ) -> Result<Vec<String>, GenexError> {
        let scope = Scope::target(target);
        let mut result = Vec::new();
        let mut emitted = HashSet::new();
        let roots = self.links(target, scope, usage, interface)?;
        self.process_ilibs(roots, scope, usage, &mut emitted, &mut result)?;
        Ok(result)
    }

    /// processILibs in cmGeneratorTarget_Link.cxx.
    fn process_ilibs(
        &mut self,
        libs: Vec<LinkDependency>,
        scope: Scope<'_>,
        usage: Usage,
        emitted: &mut HashSet<String>,
        result: &mut Vec<String>,
    ) -> Result<(), GenexError> {
        for lib in libs {
            if !emitted.insert(lib.target.clone()) {
                continue;
            }
            result.push(lib.target.clone());
            let next = self.links(&lib.target, scope, usage, true)?;
            self.process_ilibs(next, scope, usage, emitted, result)?;
        }
        Ok(())
    }

    /// cmTarget::CanCompileSources.
    pub(super) fn can_compile_sources(&self, target: &str) -> bool {
        !self.host.is_imported(target)
            && matches!(
                self.host.target_type(target),
                Some(
                    "EXECUTABLE"
                        | "STATIC_LIBRARY"
                        | "SHARED_LIBRARY"
                        | "MODULE_LIBRARY"
                        | "OBJECT_LIBRARY"
                )
            )
    }

    /// !cmGeneratorTarget::IsExecutableWithExports for an executable.
    fn executable_without_exports(&self, target: &str) -> bool {
        self.host.target_type(target) == Some("EXECUTABLE")
            && !self
                .host
                .target_property_raw(target, "ENABLE_EXPORTS")
                .as_deref()
                .is_some_and(util::is_on)
    }

    /// The cmGeneratorTarget::LinkerLanguage member: fixed at construction
    /// from HAS_CXX or LINKER_LANGUAGE (unlike GetLinkerLanguage).
    fn hard_coded_linker_language(&self, target: &str) -> String {
        if self.host.target_property_raw(target, "HAS_CXX").is_some() {
            return "CXX".to_owned();
        }
        self.host
            .target_property_raw(target, "LINKER_LANGUAGE")
            .unwrap_or_default()
    }

    /// GetLinkImplementationLibraries (`interface == false`) or
    /// GetLinkInterfaceLibraries for `scope.head`, reduced to the items that
    /// name targets. Hosts can override the computation entirely.
    pub(super) fn links(
        &mut self,
        target: &str,
        scope: Scope<'_>,
        usage: Usage,
        interface: bool,
    ) -> Result<Vec<LinkDependency>, GenexError> {
        let head = if interface {
            scope.head.unwrap_or(target)
        } else {
            target
        };
        let language = self.hard_coded_linker_language(head);
        if let Some(links) = self.host.link_dependencies(
            target,
            head,
            self.ctx.config,
            Some(&language),
            usage,
            interface,
        )? {
            return Ok(links);
        }
        let imported = self.host.is_imported(target);
        if interface {
            if !imported && self.executable_without_exports(target) {
                return Ok(Vec::new());
            }
        } else if !self.can_compile_sources(target) {
            return Ok(Vec::new());
        }
        let mut property = if interface {
            "INTERFACE_LINK_LIBRARIES".to_owned()
        } else {
            "LINK_LIBRARIES".to_owned()
        };
        let mut entries = self.host.property_entries(target, &property);
        if imported
            && self.host.target_type(target) != Some("INTERFACE_LIBRARY")
            && entries.is_empty()
        {
            if let Some(suffix) = self.host.mapped_imported_config(target, self.ctx.config) {
                let generic = "IMPORTED_LINK_INTERFACE_LIBRARIES";
                property = format!("{generic}{suffix}");
                entries = self.host.property_entries(target, &property);
                if entries.is_empty() && !suffix.is_empty() {
                    entries = self.host.property_entries(target, generic);
                    property = generic.to_owned();
                }
            }
        }
        let items = self.expand_link_entries(target, head, &property, usage, entries)?;
        if interface || items.is_empty() {
            return Ok(items);
        }
        // ComputeLinkImplTransitive injects direct interface requirements before
        // the dependency that introduced them and applies exclusions at the end.
        let mut direct = DirectLinkItems {
            emitted: items.iter().map(|item| item.target.clone()).collect(),
            ..DirectLinkItems::default()
        };
        for item in items {
            self.follow_direct_links(&item.target, head, usage, &mut direct)?;
            direct.items.push(item);
        }
        direct
            .items
            .retain(|item| !direct.excluded.contains(&item.target));
        Ok(direct.items)
    }

    fn follow_direct_links(
        &mut self,
        target: &str,
        head: &str,
        usage: Usage,
        direct: &mut DirectLinkItems,
    ) -> Result<(), GenexError> {
        if !direct.followed.insert(target.to_owned()) {
            return Ok(());
        }
        let include = "INTERFACE_LINK_LIBRARIES_DIRECT";
        let exclude = "INTERFACE_LINK_LIBRARIES_DIRECT_EXCLUDE";
        for item in self.expand_link_entries(
            target,
            head,
            include,
            usage,
            self.host.property_entries(target, include),
        )? {
            self.follow_direct_links(&item.target, head, usage, direct)?;
            if direct.emitted.insert(item.target.clone()) {
                direct.items.push(item);
            }
        }
        for item in self.links(target, Scope::target(head), usage, true)? {
            self.follow_direct_links(&item.target, head, usage, direct)?;
        }
        for item in self.expand_link_entries(
            target,
            head,
            exclude,
            usage,
            self.host.property_entries(target, exclude),
        )? {
            direct.excluded.insert(item.target);
        }
        Ok(())
    }

    fn expand_link_entries(
        &mut self,
        target: &str,
        head: &str,
        property: &str,
        usage: Usage,
        entries: Vec<PropertyEntry>,
    ) -> Result<Vec<LinkDependency>, GenexError> {
        if entries.is_empty() {
            return Ok(Vec::new());
        }
        let interface = property != "LINK_LIBRARIES";
        let direct = property.starts_with("INTERFACE_LINK_LIBRARIES_DIRECT");
        let root = DagChecker {
            computing_link_libraries: true,
            transitive: !direct && self.host.local_policy_status("CMP0189") == PolicyStatus::New,
            transitive_properties_only: usage == Usage::Compile,
            transitive_properties_only_cmp0131: usage == Usage::Compile
                && (interface || self.host.policy_status(target, "CMP0131") == PolicyStatus::New),
            ..DagChecker::new(target, property)
        };
        let resolve_aliases = self.host.policy_status(target, "CMP0108") == PolicyStatus::New;
        let language = self.hard_coded_linker_language(head);
        let previous_language = std::mem::replace(&mut self.language, language);
        let previous_mode = std::mem::replace(&mut self.ctx.evaluate_for_buildsystem, true);
        let result = (|| {
            let mut result = Vec::new();
            for entry in entries {
                let previous = self.ctx.backtrace;
                self.ctx.backtrace = entry.backtrace.or(previous);
                let evaluated = self.dependent(
                    &entry.value,
                    Scope {
                        head: Some(head),
                        current: Some(target),
                    },
                    Some(&root),
                );
                self.ctx.backtrace = previous;
                for item in cm_list(&evaluated?, false) {
                    if item.starts_with("<LINK_LIBRARY:")
                        || item.starts_with("</LINK_LIBRARY:")
                        || item.starts_with("::@")
                    {
                        // ParseLinkFeature / IsLinkLookupScope.
                        continue;
                    }
                    let name = item.trim_matches([' ', '\t', '\r', '\n']);
                    if name != item {
                        return Err(GenexError {
                            backtrace: entry.backtrace,
                            ..self.error("", format!("Target \"{target}\" links to item \"{item}\" which has leading or trailing whitespace.  This is now an error according to policy CMP0004."))
                        });
                    }
                    if name.is_empty() {
                        continue;
                    }
                    let resolved = self.host.resolve_target(name, Some(target));
                    if !interface {
                        let canonical = if resolve_aliases {
                            resolved.as_deref().unwrap_or(name)
                        } else {
                            name
                        };
                        if canonical == target {
                            return Err(
                                self.error("", format!("Target \"{target}\" links to itself."))
                            );
                        }
                    } else if !direct && name == target {
                        continue;
                    }
                    if let Some(dependency) = resolved {
                        // ResolveLinkItem: executables without exports are
                        // linked by name, not as targets. EvaluateInterfaceProperty
                        // and the closures never follow a target listed in
                        // its own interface.
                        if (direct || dependency != target)
                            && !self.executable_without_exports(&dependency)
                        {
                            result.push(LinkDependency {
                                target: dependency,
                                backtrace: entry.backtrace,
                            });
                        }
                    }
                }
            }
            Ok(result)
        })();
        self.language = previous_language;
        self.ctx.evaluate_for_buildsystem = previous_mode;
        result
    }

    /// TargetPropertyNode::Evaluate.
    pub(super) fn target_property(
        &mut self,
        args: &[String],
        expr: &str,
        scope: Scope<'_>,
        dag: Option<&DagChecker>,
    ) -> Result<String, GenexError> {
        let (target, property) = match args {
            [name, property] => {
                if name.is_empty() {
                    let rest = if property.is_empty() {
                        "target name and property name."
                    } else {
                        "target name."
                    };
                    return Err(self.error(
                        expr,
                        format!(
                            "$<TARGET_PROPERTY:tgt,prop> expression requires a non-empty {rest}"
                        ),
                    ));
                }
                if !is_valid_target_name(name) {
                    return Err(self.error(
                        expr,
                        if property.is_empty() || !identifier(property) {
                            "Target name and property name not supported."
                        } else {
                            "Target name not supported."
                        },
                    ));
                }
                if property == "ALIASED_TARGET" {
                    return Ok(self.host.resolve_alias(name).unwrap_or_default());
                }
                if property == "ALIAS_GLOBAL" {
                    return Ok(if self.host.resolve_alias(name).is_none() {
                        String::new()
                    } else if self.host.alias_global(name) {
                        "TRUE".to_owned()
                    } else {
                        "FALSE".to_owned()
                    });
                }
                let target = self
                    .host
                    .resolve_target(name, scope.current)
                    .ok_or_else(|| self.error(expr, format!("Target \"{name}\" not found.")))?;
                self.ctx.referenced_targets.insert(target.clone());
                (target, property)
            }
            [property] => {
                self.ctx.seen_target_properties.insert(property.clone());
                self.ctx.head_sensitive = true;
                let name = scope.head.ok_or_else(|| self.error(expr,
                    "$<TARGET_PROPERTY:prop>  may only be used with binary targets.  It may not be used with add_custom_command or add_custom_target.   Specify the target to read a property from using the $<TARGET_PROPERTY:tgt,prop> signature instead."))?;
                (
                    self.host
                        .resolve_alias(name)
                        .unwrap_or_else(|| name.to_owned()),
                    property,
                )
            }
            _ => {
                return Err(self.error(
                    expr,
                    "$<TARGET_PROPERTY:...> expression requires one or two parameters",
                ));
            }
        };
        if property == "SOURCES" {
            self.ctx.source_sensitive_targets.insert(target.clone());
        }
        if property.is_empty() {
            return Err(self.error(
                expr,
                "$<TARGET_PROPERTY:...> expression requires a non-empty property name.",
            ));
        }
        if !identifier(property) {
            return Err(self.error(expr, "Property name not supported."));
        }
        let evaluating_links = dag.is_some_and(|dag| dag.link_libraries(false));
        if property == "LINKER_LANGUAGE" {
            if self.host.link_language_propagates(&target)
                && (evaluating_links || dag.is_some_and(DagChecker::sources))
            {
                return Err(self.error(expr, "LINKER_LANGUAGE target property can not be used while evaluating link libraries for a static library"));
            }
            return Ok(self.host.linker_language(&target, self.ctx.config));
        }
        let usage = self.transitive(
            &target,
            property,
            dag.is_some_and(|d| d.root().computing_link_libraries),
        )?;
        if usage.is_some()
            && evaluating_links
            && dag.is_some_and(|d| {
                !d.genex()
                    && !d.linker_launcher()
                    && d.root().property != "INTERFACE_POSITION_INDEPENDENT_CODE"
            })
        {
            return Err(self.error(expr, "$<TARGET_PROPERTY:...> expression in link libraries evaluation depends on target property which is transitive over the link libraries, creating a recursion."));
        }

        if let Some(usage) = usage.filter(|_| property.starts_with("INTERFACE_")) {
            return self
                .interface_value(&target, property, scope, dag, usage)
                .map(|value| strip_empty_list_elements(&value));
        }
        let root = DagChecker {
            expression: Some(expr),
            parent: dag,
            backtrace: self.ctx.backtrace,
            transitive: dag.map_or(usage.is_some(), |d| d.root().transitive),
            ..DagChecker::new(&target, property)
        };
        match self.check(&root) {
            Check::SelfReference => return Err(self.cycle_error(&root, expr, scope)),
            Check::Cycle => return Ok(String::new()),
            Check::Dag | Check::AlreadySeen => {}
        }
        let raw = self.host.target_property_raw(&target, property);
        if raw.is_none() && evaluating_links {
            return Ok(String::new());
        }
        if !self.host.is_imported(&target) {
            if let Some(value) = self
                .host
                .compatible_interface_property(
                    &target,
                    property,
                    self.ctx.config,
                    raw.as_deref(),
                    evaluating_links,
                )
                .map_err(|message| self.error(expr, message))?
            {
                self.ctx.context_sensitive = true;
                return Ok(value);
            }
        }
        let raw = raw.unwrap_or_default();
        let Some(usage) = usage else {
            return Ok(raw);
        };
        let scope = Scope::target(&target);
        let value = self.dependent(&raw, scope, Some(&root))?;
        let mut result = strip_empty_list_elements(&value);
        // getLinkedTargetsContent.
        let interface_property = format!("INTERFACE_{property}");
        for dep in self.links(&target, scope, Usage::Compile, false)? {
            let previous = self.ctx.backtrace;
            self.ctx.backtrace = dep.backtrace.or(previous);
            let value =
                self.interface_value(&dep.target, &interface_property, scope, Some(&root), usage);
            self.ctx.backtrace = previous;
            append_list(&mut result, &value?);
        }
        Ok(result)
    }

    fn interface_frame<'a>(
        &self,
        target: &'a str,
        property: &'a str,
        dag: Option<&'a DagChecker<'a>>,
    ) -> DagChecker<'a> {
        DagChecker {
            parent: dag,
            backtrace: self.ctx.backtrace,
            transitive: dag.is_none_or(|d| d.root().transitive),
            ..DagChecker::new(target, property)
        }
    }

    /// cmGeneratorTarget::EvaluateInterfaceProperty.
    fn interface_value(
        &mut self,
        target: &str,
        property: &str,
        scope: Scope<'_>,
        dag: Option<&DagChecker>,
        usage: Usage,
    ) -> Result<String, GenexError> {
        let frame = self.interface_frame(target, property, dag);
        let scope = Scope {
            head: scope.head.or(Some(target)),
            current: Some(target),
        };
        match self.check(&frame) {
            Check::SelfReference => {
                return Err(self.cycle_error(
                    &frame,
                    &format!("$<TARGET_PROPERTY:{target},{property}>"),
                    scope,
                ));
            }
            Check::Cycle | Check::AlreadySeen => return Ok(String::new()),
            Check::Dag => {}
        }
        let mut result = match self.host.target_property_raw(target, property) {
            Some(raw) => self.dependent(&raw, scope, Some(&frame))?,
            None => String::new(),
        };
        for dep in self.links(target, scope, usage, true)? {
            let value = self.interface_value(&dep.target, property, scope, Some(&frame), usage)?;
            append_list(&mut result, &strip_empty_list_elements(&value));
        }
        Ok(result)
    }
}

fn append_list(result: &mut String, more: &str) {
    if !more.is_empty() {
        if !result.is_empty() {
            result.push(';');
        }
        result.push_str(more);
    }
}

/// Collects a build usage requirement of `target`, as
/// cmGeneratorTarget::GetIncludeDirectories, GetCompileDefinitions,
/// GetCompileOptions, GetCompileFeatures, GetPrecompileHeaders,
/// GetLinkOptions, GetLinkDirectories and GetLinkDepends do. `property` is
/// a build property name (INCLUDE_DIRECTORIES, COMPILE_OPTIONS, ...) or its
/// INTERFACE_ counterpart to collect the requirements exported to consumers.
pub fn target_usage(
    host: &mut dyn GenexHost,
    target: &str,
    property: &str,
    config: &str,
    language: &str,
) -> Result<Vec<UsageItem>, GenexError> {
    let mut ctx = EvalContext {
        config,
        language: Some(language),
        ..EvalContext::default()
    };
    usage_requirements(host, target, property, &mut ctx)
}

/// Like [`target_usage`], reading the configuration, language, device-link
/// mode and output flags from `ctx`. Warnings (e.g. CMP0081) are appended to
/// `ctx.warnings`.
pub fn usage_requirements(
    host: &mut dyn GenexHost,
    target: &str,
    property: &str,
    ctx: &mut EvalContext<'_>,
) -> Result<Vec<UsageItem>, GenexError> {
    let language = ctx.language.unwrap_or_default().to_owned();
    let mut evaluator = Evaluator {
        host,
        ctx,
        language,
    };
    let previous = evaluator.ctx.evaluate_for_buildsystem;
    evaluator.ctx.evaluate_for_buildsystem = true;
    let result = evaluator.collect_usage(target, property);
    evaluator.ctx.evaluate_for_buildsystem = previous;
    result
}

/// cmGeneratorTarget::IsSystemIncludeDirectory's cache: the sorted, unique
/// list of directories that `target` treats as system include directories.
pub fn system_include_directories(
    host: &mut dyn GenexHost,
    target: &str,
    config: &str,
    language: &str,
) -> Result<Vec<String>, GenexError> {
    let mut ctx = EvalContext {
        config,
        language: Some(language),
        ..EvalContext::default()
    };
    let mut evaluator = Evaluator {
        host,
        ctx: &mut ctx,
        language: language.to_owned(),
    };
    evaluator.system_includes(target, false)
}

impl Evaluator<'_, '_> {
    fn collect_usage(
        &mut self,
        target: &str,
        property: &str,
    ) -> Result<Vec<UsageItem>, GenexError> {
        let base = property.strip_prefix("INTERFACE_").unwrap_or(property);
        let interface = base != property;
        let usage = if interface {
            self.transitive(target, property, false)?
                .unwrap_or(Usage::Compile)
        } else if matches!(base, "LINK_OPTIONS" | "LINK_DIRECTORIES" | "LINK_DEPENDS")
            && self.host.policy_status(target, "CMP0099") == PolicyStatus::New
        {
            Usage::Link
        } else {
            Usage::Compile
        };
        // GetCompileFeatures evaluates without a language.
        let saved_language = if !interface && base == "COMPILE_FEATURES" {
            Some(std::mem::take(&mut self.language))
        } else {
            None
        };
        let entries = self.usage_entries(target, property, usage, interface);
        if let Some(language) = saved_language {
            self.language = language;
        }
        let (mut entries, front) = entries?;
        let mut result = front;
        match base {
            "INCLUDE_DIRECTORIES" => {
                self.process_include_directories(target, &mut entries, &mut result)?;
                let system = self.system_includes(target, interface)?;
                for item in &mut result {
                    item.system = system.binary_search(&item.value).is_ok();
                }
            }
            "LINK_DIRECTORIES" => {
                self.process_link_directories(target, &mut entries, &mut result)?
            }
            "COMPILE_OPTIONS" => process_options(&entries, &mut result, true, false),
            "LINK_OPTIONS" => {
                let device = self.ctx.device_link;
                process_options(&entries, &mut result, true, device);
                if device {
                    result = self.wrap_device_host_options(result);
                }
                result = self.resolve_linker_wrapper(target, result)?;
            }
            _ => process_options(&entries, &mut result, false, false),
        }
        Ok(result)
    }

    /// EvaluateTargetPropertyEntries followed by AddInterfaceEntries (and,
    /// for include directories, the implicit ISPC header directories).
    /// Returns the entries and any values that bypass entry processing.
    fn usage_entries(
        &mut self,
        target: &str,
        property: &str,
        usage: Usage,
        interface: bool,
    ) -> Result<(Vec<Entry>, Vec<UsageItem>), GenexError> {
        let root = DagChecker {
            transitive: true,
            backtrace: self.ctx.backtrace,
            ..DagChecker::new(target, property)
        };
        let scope = Scope::target(target);
        let mut entries = Vec::new();
        let mut front = Vec::new();
        for entry in self.host.property_entries(target, property) {
            let previous = self.ctx.backtrace;
            self.ctx.backtrace = entry.backtrace;
            let value = self.dependent(&entry.value, scope, Some(&root));
            self.ctx.backtrace = previous;
            entries.push(Entry {
                link_item: interface.then(|| target.to_owned()),
                backtrace: entry.backtrace,
                values: cm_list(&value?, false),
            });
        }
        if property == "INCLUDE_DIRECTORIES"
            && self.can_compile_sources(target)
            && !matches!(self.language.as_str(), "Swift" | "Fortran")
        {
            let config = self.ctx.config;
            if self
                .host
                .compile_languages(target, config)
                .iter()
                .any(|l| l == "ISPC")
            {
                let value = self
                    .host
                    .target_property_raw(target, "ISPC_HEADER_DIRECTORY")
                    .unwrap_or_else(|| self.host.object_directory(target, config));
                front.push(UsageItem {
                    value,
                    link_item: None,
                    backtrace: None,
                    system: false,
                });
            }
            self.ispc_implicit_includes(target, &mut entries)?;
        }
        let interface_property = if interface {
            property.to_owned()
        } else {
            format!("INTERFACE_{property}")
        };
        let mut links = if interface {
            Vec::new()
        } else {
            self.host
                .language_runtime_dependencies(target, self.ctx.config, &self.language, usage)
        };
        links.extend(self.links(target, scope, usage, interface)?);
        for lib in links {
            let previous = self.ctx.backtrace;
            self.ctx.backtrace = lib.backtrace;
            let value =
                self.interface_value(&lib.target, &interface_property, scope, Some(&root), usage);
            self.ctx.backtrace = previous;
            entries.push(Entry {
                link_item: Some(lib.target),
                backtrace: lib.backtrace,
                values: cm_list(&value?, false),
            });
        }
        Ok((entries, front))
    }

    /// AddLangSpecificImplicitIncludeDirectories for ISPC.
    fn ispc_implicit_includes(
        &mut self,
        target: &str,
        entries: &mut Vec<Entry>,
    ) -> Result<(), GenexError> {
        const PROP: &str = "ISPC_HEADER_DIRECTORY";
        let config = self.ctx.config;
        let has_ispc = |host: &dyn GenexHost, t: &str| {
            host.compile_languages(t, config)
                .iter()
                .any(|l| l == "ISPC")
        };
        for lib in self.links(target, Scope::target(target), Usage::Compile, false)? {
            if !self.host.in_build_system(&lib.target) || !has_ispc(&*self.host, &lib.target) {
                continue;
            }
            let mut values = vec![
                self.host
                    .target_property_raw(&lib.target, PROP)
                    .unwrap_or_else(|| self.host.object_directory(&lib.target, config)),
            ];
            // AddLangSpecificInterfaceIncludeDirectories.
            let scope = Scope {
                head: Some(target),
                current: Some(&lib.target),
            };
            let mut directories = String::new();
            for dep in self.links(&lib.target, scope, Usage::Compile, true)? {
                if has_ispc(&*self.host, &dep.target) {
                    let value = self
                        .host
                        .target_property_raw(&dep.target, PROP)
                        .filter(|v| !v.is_empty())
                        .unwrap_or_else(|| self.host.support_directory(&dep.target));
                    if !directories.is_empty() {
                        directories.push(';');
                    }
                    directories.push_str(&value);
                }
            }
            values.extend(cm_list(&directories, false));
            entries.push(Entry {
                link_item: Some(lib.target),
                backtrace: lib.backtrace,
                values,
            });
        }
        Ok(())
    }

    /// processIncludeDirectories.
    fn process_include_directories(
        &mut self,
        target: &str,
        entries: &mut [Entry],
        result: &mut Vec<UsageItem>,
    ) -> Result<(), GenexError> {
        let mut unique = HashSet::new();
        for entry in entries {
            let from_imported = entry
                .link_item
                .as_deref()
                .is_some_and(|item| self.host.is_imported(item));
            for value in &mut entry.values {
                if from_imported && !self.host.path_exists(value) {
                    return Err(self.usage_error(format!(
                        "Imported target \"{}\" includes non-existent path\n  \"{value}\"\nin its INTERFACE_INCLUDE_DIRECTORIES. Possible reasons include:\n* The path was deleted, renamed, or moved to another location.\n* An install or uninstall procedure did not complete successfully.\n* The installation package was faulty and references files it does not provide.\n",
                        entry.link_item.as_deref().unwrap_or_default()
                    )));
                }
                if !path::file_is_full_path(value) {
                    return Err(self.usage_error(match &entry.link_item {
                        Some(item) => format!(
                            "Target \"{item}\" contains relative path in its INTERFACE_INCLUDE_DIRECTORIES:\n  \"{value}\""
                        ),
                        None => format!(
                            "Found relative path while evaluating include directories of \"{target}\":\n  \"{value}\"\n"
                        ),
                    }));
                }
                if !util::is_off(value) {
                    *value = path::unix_slashes(value);
                }
                if unique.insert(value.clone()) {
                    result.push(UsageItem {
                        value: value.clone(),
                        link_item: entry.link_item.clone(),
                        backtrace: entry.backtrace,
                        system: false,
                    });
                }
            }
        }
        Ok(())
    }

    /// processLinkDirectories.
    fn process_link_directories(
        &mut self,
        target: &str,
        entries: &mut [Entry],
        result: &mut Vec<UsageItem>,
    ) -> Result<(), GenexError> {
        let mut unique = HashSet::new();
        for entry in entries {
            for value in &mut entry.values {
                if !path::file_is_full_path(value) {
                    match &entry.link_item {
                        Some(item) => {
                            return Err(self.usage_error(format!(
                                "Target \"{item}\" contains relative path in its INTERFACE_LINK_DIRECTORIES:\n  \"{value}\""
                            )));
                        }
                        None => {
                            let message = format!(
                                "Found relative path while evaluating link directories of \"{target}\":\n  \"{value}\"\n"
                            );
                            match self.host.policy_status(target, "CMP0081") {
                                PolicyStatus::New => return Err(self.usage_error(message)),
                                PolicyStatus::Warn => {
                                    self.policy_warning("CMP0081", &format!("\n{message}"));
                                }
                                PolicyStatus::Old => {}
                            }
                        }
                    }
                }
                *value = path::unix_slashes(value);
                if unique.insert(value.clone()) {
                    result.push(UsageItem {
                        value: value.clone(),
                        link_item: entry.link_item.clone(),
                        backtrace: entry.backtrace,
                        system: false,
                    });
                }
            }
        }
        Ok(())
    }

    /// cmGeneratorTarget::AddSystemIncludeCacheKey.
    fn system_includes(
        &mut self,
        target: &str,
        interface: bool,
    ) -> Result<Vec<String>, GenexError> {
        let root = DagChecker {
            transitive: true,
            ..DagChecker::new(target, "SYSTEM_INCLUDE_DIRECTORIES")
        };
        let exclude_imported = self.bool_property(target, "NO_SYSTEM_FROM_IMPORTED");
        let mut result = Vec::new();
        let scope = Scope::target(target);
        let mut deps = if interface {
            let mut deps = vec![target.to_owned()];
            deps.extend(self.closure(target, Usage::Compile, true)?);
            deps
        } else {
            for entry in self.host.system_include_directories(target) {
                result.extend(cm_list(&self.dependent(&entry, scope, Some(&root))?, false));
            }
            self.closure(target, Usage::Compile, false)?
        };
        if !interface {
            deps.extend(
                self.host
                    .language_runtime_dependencies(
                        target,
                        self.ctx.config,
                        &self.language,
                        Usage::Compile,
                    )
                    .into_iter()
                    .map(|lib| lib.target),
            );
        }
        for dep in deps {
            let scope = Scope {
                head: Some(target),
                current: Some(&dep),
            };
            if let Some(dirs) = self
                .host
                .target_property_raw(&dep, "INTERFACE_SYSTEM_INCLUDE_DIRECTORIES")
            {
                result.extend(cm_list(&self.dependent(&dirs, scope, Some(&root))?, false));
            }
            let system = self
                .host
                .target_property_raw(&dep, "SYSTEM")
                .map_or_else(|| self.host.is_imported(&dep), |value| util::is_on(&value));
            if !system {
                continue;
            }
            if self.host.is_imported(&dep)
                && (exclude_imported || self.bool_property(&dep, "IMPORTED_NO_SYSTEM"))
            {
                continue;
            }
            if let Some(dirs) = self
                .host
                .target_property_raw(&dep, "INTERFACE_INCLUDE_DIRECTORIES")
            {
                result.extend(cm_list(&self.dependent(&dirs, scope, Some(&root))?, false));
            }
        }
        for value in &mut result {
            *value = path::unix_slashes(value);
        }
        result.sort();
        result.dedup();
        Ok(result)
    }

    fn bool_property(&self, target: &str, property: &str) -> bool {
        self.host
            .target_property_raw(target, property)
            .as_deref()
            .is_some_and(util::is_on)
    }

    fn usage_error(&self, message: String) -> GenexError {
        self.error("", message)
    }

    /// The host-option wrapping block of cmGeneratorTarget::GetLinkOptions.
    fn wrap_device_host_options(&self, items: Vec<UsageItem>) -> Vec<UsageItem> {
        let (flags, separator, concatenate) = self.wrapper_flag("_DEVICE_COMPILER");
        let mut out = Vec::new();
        let mut items = items.into_iter();
        while let Some(item) = items.next() {
            if item.value == DL_BEGIN {
                for inner in items.by_ref() {
                    if inner.value == DL_END {
                        break;
                    }
                    out.push(inner);
                }
                continue;
            }
            let options = parse_unix_command_line(&item.value);
            for value in wrap_options(options, &flags, &separator, concatenate, true) {
                out.push(UsageItem {
                    value,
                    ..item.clone()
                });
            }
        }
        out
    }

    /// Reads CMAKE_<LANG><kind>_WRAPPER_FLAG and its _SEP companion.
    fn wrapper_flag(&self, kind: &str) -> (Vec<String>, String, bool) {
        let lang = &self.language;
        let mut flags = cm_list(
            &self
                .host
                .variable(&format!("CMAKE_{lang}{kind}_WRAPPER_FLAG"))
                .unwrap_or_default(),
            false,
        );
        let separator = self
            .host
            .variable(&format!("CMAKE_{lang}{kind}_WRAPPER_FLAG_SEP"))
            .unwrap_or_default();
        let mut concatenate = true;
        if flags.last().is_some_and(|f| f == " ") {
            concatenate = false;
            flags.pop();
        }
        (flags, separator, concatenate)
    }

    /// cmGeneratorTarget::ResolvePrefixWrapper for "LINKER", without joining.
    fn resolve_linker_wrapper(
        &self,
        target: &str,
        items: Vec<UsageItem>,
    ) -> Result<Vec<UsageItem>, GenexError> {
        let kind = if self.ctx.device_link {
            "_DEVICE_LINKER"
        } else {
            "_LINKER"
        };
        let (flags, separator, concatenate) = self.wrapper_flag(kind);
        let mut out = Vec::new();
        for item in items {
            let Some(arguments) = item.value.strip_prefix("LINKER:") else {
                out.push(item);
                continue;
            };
            let options: Vec<String> = if let Some(shell) = arguments.strip_prefix("SHELL:") {
                parse_unix_command_line(shell)
            } else {
                arguments
                    .split(',')
                    .filter(|arg| !arg.is_empty())
                    .map(str::to_owned)
                    .collect()
            };
            if options.is_empty() {
                continue;
            }
            if options.iter().any(|option| option.contains("SHELL:")) {
                // cmGeneratorTarget::ResolveLinkerWrapper issues the
                // diagnostic at the target's own backtrace.
                let mut error = self.usage_error(
                    "'SHELL:' prefix is not supported as part of 'LINKER:' arguments.".to_owned(),
                );
                error.backtrace = self.host.target_backtrace(target).or(error.backtrace);
                return Err(error);
            }
            for value in wrap_options(options, &flags, &separator, concatenate, false) {
                out.push(UsageItem {
                    value,
                    ..item.clone()
                });
            }
        }
        Ok(out)
    }
}

/// processOptions.
fn process_options(entries: &[Entry], result: &mut Vec<UsageItem>, shell: bool, device: bool) {
    let mut unique = HashSet::new();
    let mut split = !device;
    for entry in entries {
        let item = |value: String| UsageItem {
            value,
            link_item: entry.link_item.clone(),
            backtrace: entry.backtrace,
            system: false,
        };
        for option in &entry.values {
            if device && (option == DL_BEGIN || option == DL_END) {
                result.push(item(option.clone()));
                split = option == DL_BEGIN;
                continue;
            }
            if !unique.insert(option.clone()) {
                continue;
            }
            match option.strip_prefix("SHELL:").filter(|_| shell) {
                Some(rest) if split => {
                    result.extend(parse_unix_command_line(rest).into_iter().map(item));
                }
                Some(rest) => result.push(item(rest.to_owned())),
                None => result.push(item(option.clone())),
            }
        }
    }
}

/// wrapOptions in cmGeneratorTarget_Options.cxx. `normalize` selects
/// NestedLinkerFlags::Normalize.
fn wrap_options(
    mut options: Vec<String>,
    flags: &[String],
    separator: &str,
    concatenate: bool,
    normalize: bool,
) -> Vec<String> {
    let mut result = Vec::new();
    if options.is_empty() {
        return result;
    }
    if flags.is_empty() {
        return options;
    }
    let insert_wrapped = |result: &mut Vec<String>, opts: Vec<String>| {
        if !separator.is_empty() {
            if concatenate {
                result.extend(flags[..flags.len() - 1].iter().cloned());
                result.push(format!(
                    "{}{}",
                    flags[flags.len() - 1],
                    opts.join(separator)
                ));
            } else {
                result.extend(flags.iter().cloned());
                result.push(opts.join(separator));
            }
        } else {
            let prefix = if concatenate {
                &flags[..flags.len() - 1]
            } else {
                flags
            };
            for option in opts {
                result.extend(prefix.iter().cloned());
                result.push(if concatenate {
                    format!("{}{option}", flags[flags.len() - 1])
                } else {
                    option
                });
            }
        }
    };
    if !normalize {
        insert_wrapped(&mut result, options);
        return result;
    }
    let mut index = 0;
    while index < options.len() {
        let option = &options[index];
        if option.starts_with("LINKER:") {
            result.push(option.clone());
        } else if let Some(rest) = option.strip_prefix("-Wl,") {
            result.push(format!("LINKER:{rest}"));
        } else if let Some(rest) = option.strip_prefix("-Xlinker=") {
            result.push(format!("LINKER:{rest}"));
        } else if option == "-Xlinker" {
            if index + 1 < options.len() {
                index += 1;
                result.push(format!("LINKER:{}", options[index]));
            } else {
                result.push(option.clone());
            }
        } else {
            let mut opts = Vec::new();
            while index < options.len()
                && !options[index].starts_with("LINKER:")
                && !options[index].starts_with("-Wl,")
                && !options[index].starts_with("-Xlinker=")
                && options[index] != "-Xlinker"
            {
                opts.push(std::mem::take(&mut options[index]));
                index += 1;
            }
            insert_wrapped(&mut result, opts);
            continue;
        }
        index += 1;
    }
    result
}

/// cmSystemTools::ParseUnixCommandLine (kwsysSystem__ParseUnixCommand).
pub(super) fn parse_unix_command_line(command: &str) -> Vec<String> {
    let mut args = Vec::new();
    let mut arg = Vec::new();
    let mut in_argument = false;
    let mut in_escape = false;
    let mut in_single = false;
    let mut in_double = false;
    let push = |arg: &mut Vec<u8>, args: &mut Vec<String>| {
        args.push(String::from_utf8_lossy(&std::mem::take(arg)).into_owned());
    };
    for &c in command.as_bytes() {
        if in_escape {
            in_argument = true;
            arg.push(c);
            in_escape = false;
        } else if c == b'\\' {
            in_escape = true;
        } else if c == b'\'' && !in_double {
            in_single = !in_single;
            in_argument = true;
        } else if c == b'"' && !in_single {
            in_double = !in_double;
            in_argument = true;
        } else if c.is_ascii_whitespace() || c == 0x0b {
            if in_argument {
                if in_single || in_double {
                    arg.push(c);
                } else {
                    push(&mut arg, &mut args);
                    in_argument = false;
                }
            }
        } else {
            in_argument = true;
            arg.push(c);
        }
    }
    if in_argument {
        push(&mut arg, &mut args);
    }
    args
}
