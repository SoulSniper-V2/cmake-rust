//! cmGeneratorTarget_TransitiveProperty and the generator's usage entry points.

use super::dag::Check;
use super::*;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UsageItem {
    pub value: String,
    pub target: String,
    pub backtrace: Option<Backtrace>,
    pub system: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LinkDependency {
    pub target: String,
    pub backtrace: Option<Backtrace>,
}

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

impl Evaluator<'_, '_> {
    fn transitive(
        &mut self,
        target: &str,
        property: &str,
        computing_links: bool,
    ) -> Result<Option<Usage>, GenexError> {
        let interface = property.starts_with("INTERFACE_");
        let name = property.strip_prefix("INTERFACE_").unwrap_or(property);
        let result = if COMPILE_PROPERTIES.contains(&name) {
            Some(Usage::Compile)
        } else if LINK_PROPERTIES.contains(&name) {
            if name == "LINK_LIBRARIES" {
                (self.host.local_policy_status("CMP0189") == PolicyStatus::New)
                    .then_some(Usage::Link)
            } else if self.host.local_policy_status("CMP0166") == PolicyStatus::New {
                Some(Usage::Link)
            } else {
                Some(Usage::Compile)
            }
        } else if !computing_links {
            let scope = Scope::target(target);
            for (variable, usage) in [
                ("TRANSITIVE_LINK_PROPERTIES", Usage::Link),
                ("TRANSITIVE_COMPILE_PROPERTIES", Usage::Compile),
            ] {
                if self.custom_property(target, variable, name) {
                    return Ok(Some(usage));
                }
                let mut seen = HashSet::from([target.to_owned()]);
                let deps = self.links(target, scope, usage, interface)?;
                if self.closure_has_property(deps, scope, usage, variable, name, &mut seen)? {
                    return Ok(Some(usage));
                }
            }
            None
        } else {
            None
        };
        Ok(result)
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

    fn closure_has_property(
        &mut self,
        deps: Vec<LinkDependency>,
        scope: Scope<'_>,
        usage: Usage,
        variable: &str,
        name: &str,
        seen: &mut HashSet<String>,
    ) -> Result<bool, GenexError> {
        for dependency in deps {
            let target = dependency.target;
            if !seen.insert(target.clone()) {
                continue;
            }
            if self.custom_property(&target, variable, name) {
                return Ok(true);
            }
            let deps = self.links(&target, scope, usage, true)?;
            if self.closure_has_property(deps, scope, usage, variable, name, seen)? {
                return Ok(true);
            }
        }
        Ok(false)
    }

    fn links(
        &mut self,
        target: &str,
        scope: Scope<'_>,
        usage: Usage,
        interface: bool,
    ) -> Result<Vec<LinkDependency>, GenexError> {
        if let Some(links) = self.host.link_dependencies(
            target,
            scope.head.unwrap_or(target),
            self.ctx.config,
            self.ctx.language,
            usage,
            interface,
        )? {
            return Ok(links);
        }
        let property = if interface {
            "INTERFACE_LINK_LIBRARIES"
        } else {
            "LINK_LIBRARIES"
        };
        let mut root = DagChecker::new(target, property);
        root.computing_link_libraries = true;
        root.transitive_properties_only = usage == Usage::Compile;
        root.transitive_properties_only_cmp0131 = usage == Usage::Compile
            && (interface || self.host.policy_status(target, "CMP0131") == PolicyStatus::New);
        let mut result = Vec::new();
        for entry in self.host.property_entries(target, property) {
            let previous = self.ctx.backtrace;
            self.ctx.backtrace = entry.backtrace.or(previous);
            // Link implementation expressions use the implementation as the head.
            let head = if interface {
                scope.head.or(Some(target))
            } else {
                Some(target)
            };
            let evaluated = self.dependent(
                &entry.value,
                Scope {
                    head,
                    current: Some(target),
                },
                Some(&root),
            );
            self.ctx.backtrace = previous;
            for item in cm_list(&evaluated?, false) {
                let item = item.trim();
                if let Some(dependency) = self.host.resolve_target(item, Some(target)) {
                    if dependency != target {
                        result.push(LinkDependency {
                            target: dependency,
                            backtrace: entry.backtrace,
                        });
                    }
                }
            }
        }
        Ok(result)
    }

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

        if dag.is_none() {
            self.seen.clear();
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
            transitive: usage.is_some(),
            ..DagChecker::new(&target, property)
        };
        match self.check(&root) {
            Check::SelfReference => return Err(self.cycle_error(&root, expr, scope)),
            Check::Cycle => return Ok(String::new()),
            Check::Dag | Check::AlreadySeen => {}
        }
        let raw = self.host.target_property_raw(&target, property);
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
        for dep in self.links(&target, scope, Usage::Compile, false)? {
            let previous = self.ctx.backtrace;
            self.ctx.backtrace = dep.backtrace.or(previous);
            let value = self.interface_value(
                &dep.target,
                &format!("INTERFACE_{property}"),
                scope,
                Some(&root),
                usage,
            );
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
            transitive: true,
            ..DagChecker::new(target, property)
        }
    }

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
        let raw = self
            .host
            .target_property_raw(target, property)
            .unwrap_or_default();
        let mut result = self.dependent(&raw, scope, Some(&frame))?;
        for dep in self.links(target, scope, usage, true)? {
            let value = self.interface_value(&dep.target, property, scope, Some(&frame), usage)?;
            append_list(&mut result, &strip_empty_list_elements(&value));
        }
        Ok(result)
    }

    fn usage_items(
        &mut self,
        target: &str,
        scope: Scope<'_>,
        dag: &DagChecker<'_>,
        usage: Usage,
        inherited: Option<Backtrace>,
        items: &mut Vec<UsageItem>,
    ) -> Result<(), GenexError> {
        if !matches!(self.check(dag), Check::Dag) {
            return Ok(());
        }
        for entry in self.host.property_entries(target, dag.property) {
            let previous = self.ctx.backtrace;
            self.ctx.backtrace = entry.backtrace.or(inherited).or(previous);
            let value = self.dependent(&entry.value, scope, Some(dag));
            self.ctx.backtrace = previous;
            for value in cm_list(&value?, false) {
                items.push(UsageItem {
                    value,
                    target: target.to_owned(),
                    backtrace: entry.backtrace.or(inherited),
                    system: false,
                });
            }
        }
        for dep in self.links(target, scope, usage, dag.property.starts_with("INTERFACE_"))? {
            let prop = if dag.property.starts_with("INTERFACE_") {
                dag.property.to_owned()
            } else {
                format!("INTERFACE_{}", dag.property)
            };
            let frame = self.interface_frame(&dep.target, &prop, Some(dag));
            self.usage_items(
                &dep.target,
                Scope {
                    head: scope.head,
                    current: Some(&dep.target),
                },
                &frame,
                usage,
                dep.backtrace.or(inherited),
                items,
            )?;
        }
        Ok(())
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

/// Evaluate the specified build or interface property for a target. For
/// generator usage, prefer the build property (e.g. INCLUDE_DIRECTORIES).
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

/// Equivalent to the generator's Get*Options/Directories/Definitions/Features
/// collection, with the origin target and the declaration backtrace retained.
pub fn usage_requirements(
    host: &mut dyn GenexHost,
    target: &str,
    property: &str,
    ctx: &mut EvalContext<'_>,
) -> Result<Vec<UsageItem>, GenexError> {
    let old = ctx.evaluate_for_buildsystem;
    ctx.evaluate_for_buildsystem = true;
    let mut evaluator = Evaluator {
        host,
        ctx,
        seen: HashSet::new(),
    };
    let result = evaluator.collect_usage(target, property);
    ctx.evaluate_for_buildsystem = old;
    result
}

impl Evaluator<'_, '_> {
    fn collect_usage(
        &mut self,
        target: &str,
        property: &str,
    ) -> Result<Vec<UsageItem>, GenexError> {
        let scope = Scope::target(target);
        let base = property.strip_prefix("INTERFACE_").unwrap_or(property);
        let is_interface = property != base;
        let usage = if LINK_PROPERTIES.contains(&base) {
            if is_interface {
                self.transitive(target, property, false)?
                    .unwrap_or(Usage::Compile)
            } else if self.host.policy_status(target, "CMP0099") == PolicyStatus::New {
                Usage::Link
            } else {
                Usage::Compile
            }
        } else {
            Usage::Compile
        };
        let root = DagChecker {
            transitive: true,
            ..DagChecker::new(target, property)
        };
        let mut values = Vec::new();
        self.usage_items(target, scope, &root, usage, None, &mut values)?;
        let includes = base == "INCLUDE_DIRECTORIES";
        let directories = includes || base == "LINK_DIRECTORIES";
        let mut system = HashSet::new();
        if includes {
            let root = DagChecker {
                transitive: true,
                ..DagChecker::new(target, "SYSTEM_INCLUDE_DIRECTORIES")
            };
            let exclude_imported = self
                .host
                .target_property_raw(target, "NO_SYSTEM_FROM_IMPORTED")
                .as_deref()
                .is_some_and(util::is_on);
            let mut contributors = Vec::new();
            let mut seen = HashSet::from([target.to_owned()]);
            let deps = self.links(target, scope, Usage::Compile, is_interface)?;
            self.collect_system_dependencies(deps, scope, &mut seen, &mut contributors)?;
            for contributor in
                std::iter::once(target).chain(contributors.iter().map(String::as_str))
            {
                let own = contributor == target && !is_interface;
                let contributor_scope = Scope {
                    head: scope.head,
                    current: Some(contributor),
                };
                let system_prop = if own {
                    "SYSTEM_INCLUDE_DIRECTORIES"
                } else {
                    "INTERFACE_SYSTEM_INCLUDE_DIRECTORIES"
                };
                let mut properties = vec![system_prop];
                let imported = self.host.is_imported(contributor);
                let enabled = self
                    .host
                    .target_property_raw(contributor, "SYSTEM")
                    .map_or(imported, |s| util::is_on(&s));
                if !own && enabled && !(imported && exclude_imported) {
                    properties.push("INTERFACE_INCLUDE_DIRECTORIES");
                }
                for prop in properties {
                    for entry in self.host.property_entries(contributor, prop) {
                        let value = self.dependent(&entry.value, contributor_scope, Some(&root))?;
                        system.extend(cm_list(&value, false).into_iter().map(|v| unix_slashes(&v)));
                    }
                }
            }
        }
        let mut seen = HashSet::new();
        let mut result = Vec::new();
        for mut item in values {
            if directories {
                if includes
                    && item.target != target
                    && self.host.is_imported(&item.target)
                    && !self.host.path_exists(&item.value)
                {
                    return Err(self.usage_error(&item, format!(
                        "Imported target \"{}\" includes non-existent path\n  \"{}\"\nin its INTERFACE_INCLUDE_DIRECTORIES. Possible reasons include:\n* The path was deleted, renamed, or moved to another location.\n* An install or uninstall procedure did not complete successfully.\n* The installation package was faulty and references files it does not provide.\n",
                        item.target, item.value
                    )));
                }
                if !util::is_full_path(&item.value) {
                    if item.target != target || is_interface {
                        return Err(self.usage_error(&item, format!("Target \"{}\" contains relative path in its INTERFACE_{base}:\n  \"{}\"",
                            item.target, item.value)));
                    }
                    let kind = if includes { "include" } else { "link" };
                    let error = format!(
                        "Found relative path while evaluating {kind} directories of \"{target}\":\n  \"{}\"\n",
                        item.value
                    );
                    if includes {
                        return Err(self.usage_error(&item, error));
                    }
                    match self.host.policy_status(target, "CMP0081") {
                        PolicyStatus::New => return Err(self.usage_error(&item, error)),
                        PolicyStatus::Warn => self.policy_warning("CMP0081", &format!("\n{error}")),
                        PolicyStatus::Old => {}
                    }
                }
                if !includes || !util::is_off(&item.value) {
                    item.value = unix_slashes(&item.value);
                }
                item.system = includes && system.contains(&item.value);
            }
            if !seen.insert(item.value.clone()) {
                continue;
            }
            if matches!(base, "COMPILE_OPTIONS" | "LINK_OPTIONS") {
                if let Some(shell) = item.value.strip_prefix("SHELL:") {
                    for value in shell_words(shell) {
                        result.push(UsageItem {
                            value,
                            ..item.clone()
                        });
                    }
                    continue;
                }
            }
            result.push(item);
        }
        if base == "LINK_OPTIONS" {
            result = self.linker_options(result)?;
        }
        Ok(result)
    }

    fn usage_error(&self, item: &UsageItem, message: String) -> GenexError {
        GenexError {
            backtrace: item.backtrace.or(self.ctx.backtrace),
            ..self.error("", message)
        }
    }

    fn collect_system_dependencies(
        &mut self,
        deps: Vec<LinkDependency>,
        scope: Scope<'_>,
        seen: &mut HashSet<String>,
        output: &mut Vec<String>,
    ) -> Result<(), GenexError> {
        for dep in deps {
            if !seen.insert(dep.target.clone()) {
                continue;
            }
            let links = self.links(&dep.target, scope, Usage::Compile, true)?;
            output.push(dep.target);
            self.collect_system_dependencies(links, scope, seen, output)?;
        }
        Ok(())
    }

    fn linker_options(&self, items: Vec<UsageItem>) -> Result<Vec<UsageItem>, GenexError> {
        let language = self.ctx.language.unwrap_or_default();
        let mut out = Vec::new();
        let mut device = false;
        for item in items {
            if self.ctx.device_link {
                match item.value.as_str() {
                    "<DEVICE_LINK>" => {
                        device = true;
                        continue;
                    }
                    "</DEVICE_LINK>" => {
                        device = false;
                        continue;
                    }
                    _ => {}
                }
                if !device && !item.value.starts_with("LINKER:") {
                    let flags = shell_words(&item.value);
                    for value in self.wrap(flags, language, "DEVICE_COMPILER") {
                        out.push(UsageItem {
                            value,
                            ..item.clone()
                        });
                    }
                    continue;
                }
            }
            let Some(args) = item.value.strip_prefix("LINKER:") else {
                out.push(item);
                continue;
            };
            let flags = if let Some(shell) = args.strip_prefix("SHELL:") {
                shell_words(shell)
            } else if args.contains("SHELL:") {
                return Err(self.usage_error(
                    &item,
                    "'SHELL:' prefix is not supported as part of 'LINKER:' arguments.".to_owned(),
                ));
            } else {
                args.split(',')
                    .filter(|arg| !arg.is_empty())
                    .map(str::to_owned)
                    .collect()
            };
            let wrapper = if self.ctx.device_link {
                "DEVICE_LINKER"
            } else {
                "LINKER"
            };
            for value in self.wrap(flags, language, wrapper) {
                out.push(UsageItem {
                    value,
                    ..item.clone()
                });
            }
        }
        Ok(out)
    }

    fn wrap(&self, arguments: Vec<String>, lang: &str, kind: &str) -> Vec<String> {
        if arguments.is_empty() {
            return arguments;
        }
        let flag = self
            .host
            .variable(&format!("CMAKE_{lang}_{kind}_WRAPPER_FLAG"))
            .unwrap_or_default();
        let separator = self
            .host
            .variable(&format!("CMAKE_{lang}_{kind}_WRAPPER_FLAG_SEP"))
            .unwrap_or_default();
        let mut flags = cm_list(&flag, false);
        if flags.is_empty() {
            return arguments;
        }
        let concatenate = flags.last().is_some_and(|f| f != " ");
        if !concatenate {
            flags.pop();
        }
        let arguments = if separator.is_empty() {
            arguments
        } else {
            vec![arguments.join(&separator)]
        };
        let mut out = Vec::new();
        for arg in arguments {
            out.extend(flags.iter().cloned());
            if concatenate {
                out.last_mut().unwrap().push_str(&arg);
            } else {
                out.push(arg);
            }
        }
        out
    }
}

fn unix_slashes(value: &str) -> String {
    let value = value.replace('\\', "/");
    let keep = if value.starts_with("//") { 2 } else { 0 };
    let mut result = value[..keep].to_owned();
    for c in value[keep..].chars() {
        if c != '/' || !result.ends_with('/') {
            result.push(c);
        }
    }
    if result.len() > 1 && result.ends_with('/') && !result.ends_with(":/") {
        result.pop();
    }
    result
}

fn shell_words(value: &str) -> Vec<String> {
    shlex::split(value).unwrap_or_default()
}
