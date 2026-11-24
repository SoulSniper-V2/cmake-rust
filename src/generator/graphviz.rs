//! `cmGraphVizWriter` (cmGraphVizWriter.cxx) over `cmLinkItemGraphVisitor`
//! (cmLinkItemGraphVisitor.cxx): `cmake --graphviz=<file>`.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use super::ctx::GenCtx;
use super::link::{GraphLinkItem, graph_link_libraries};
use crate::interp::{Interp, Mode};
use crate::state::{Target, TargetKind};

const EDGE_STYLE_PUBLIC: &str = "solid";
const EDGE_STYLE_INTERFACE: &str = "dashed";
const EDGE_STYLE_PRIVATE: &str = "dotted";

#[derive(Clone, Copy, PartialEq, Eq)]
enum DependencyType {
    LinkInterface,
    LinkPublic,
    LinkPrivate,
    Object,
    Utility,
}

/// A `cmLinkItem`: identity is the target (by storage key) or the string.
#[derive(Clone)]
struct Item<'i> {
    name: String,
    target: Option<&'i Target>,
}

impl Item<'_> {
    fn identity(&self) -> (Option<usize>, String) {
        match self.target {
            Some(target) => (Some(std::ptr::from_ref(target) as usize), String::new()),
            None => (None, self.name.clone()),
        }
    }
}

struct Connection<'i> {
    src: Item<'i>,
    dst: Item<'i>,
    scope: String,
}

struct Settings {
    graph_name: String,
    graph_header: String,
    node_prefix: String,
    executables: bool,
    static_libs: bool,
    shared_libs: bool,
    module_libs: bool,
    interface_libs: bool,
    object_libs: bool,
    unknown_libs: bool,
    custom_targets: bool,
    externals: bool,
    per_target: bool,
    dependers: bool,
    ignore: Vec<Option<crate::cmregex::Regex>>,
}

struct Writer<'c, 'i> {
    ctx: &'c GenCtx<'i>,
    file_name: String,
    global: String,
    settings: Settings,
    next_node_id: usize,
    node_names: HashMap<String, String>,
    visited_items: BTreeSet<String>,
    visited_links: BTreeSet<(String, String)>,
    per_target: Vec<ConnectionEntry<'i>>,
    dependers: Vec<ConnectionEntry<'i>>,
    aliases: BTreeMap<String, Vec<String>>,
}

/// `cmGlobalGenerator::IsReservedTarget`.
fn is_reserved_target(name: &str) -> bool {
    matches!(
        name,
        "all"
            | "ALL_BUILD"
            | "help"
            | "install"
            | "INSTALL"
            | "preinstall"
            | "clean"
            | "edit_cache"
            | "rebuild_cache"
            | "ZERO_CHECK"
    )
}

/// `cmSystemTools::EscapeChars(str, "\"")`.
fn escape_for_dot_file(text: &str) -> String {
    text.replace('"', "\\\"")
}

fn path_safe_string(text: &str) -> String {
    text.chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'))
        .collect()
}

fn shape(item: &Item<'_>) -> &'static str {
    let Some(target) = item.target else {
        return "septagon";
    };
    match target.kind {
        TargetKind::Executable => "egg",
        TargetKind::StaticLibrary => "octagon",
        TargetKind::SharedLibrary => "doubleoctagon",
        TargetKind::ModuleLibrary => "tripleoctagon",
        TargetKind::ObjectLibrary => "hexagon",
        TargetKind::Utility => "box",
        TargetKind::InterfaceLibrary => "pentagon",
        TargetKind::UnknownLibrary | TargetKind::GlobalTarget => "septagon",
    }
}

fn edge_style(kind: DependencyType) -> String {
    match kind {
        DependencyType::LinkPrivate => format!("[ style = {EDGE_STYLE_PRIVATE} ]"),
        DependencyType::LinkInterface => format!("[ style = {EDGE_STYLE_INTERFACE} ]"),
        _ => String::new(),
    }
}

/// `cmGraphVizWriter::ReadSettings`: a script-mode `cmMakefile` reads the
/// options file.
fn read_settings(interp: &Interp, settings: &mut Settings) {
    let primary = format!("{}/CMakeGraphVizOptions.cmake", interp.home_binary);
    let fallback = format!("{}/CMakeGraphVizOptions.cmake", interp.home_source);
    let file = if std::path::Path::new(&primary).exists() {
        primary
    } else if std::path::Path::new(&fallback).exists() {
        fallback
    } else {
        return;
    };
    let mut mf = Interp::new(Mode::Script, "", "", &interp.cmake_root);
    crate::configure::add_default_definitions(&mut mf);
    if !mf.read_list_file(&file) {
        crate::configure::system_error(
            None,
            &format!("Problem opening GraphViz options file: {file}"),
        );
        return;
    }
    println!("Reading GraphViz options file: {file}");
    let text = |name: &str, out: &mut String| {
        if let Some(value) = mf.get_def(name) {
            *out = value.to_owned();
        }
    };
    text("GRAPHVIZ_GRAPH_NAME", &mut settings.graph_name);
    text("GRAPHVIZ_GRAPH_HEADER", &mut settings.graph_header);
    text("GRAPHVIZ_NODE_PREFIX", &mut settings.node_prefix);
    let flag = |name: &str, out: &mut bool| {
        if let Some(value) = mf.get_def(name) {
            *out = crate::util::is_on(value);
        }
    };
    flag("GRAPHVIZ_EXECUTABLES", &mut settings.executables);
    flag("GRAPHVIZ_STATIC_LIBS", &mut settings.static_libs);
    flag("GRAPHVIZ_SHARED_LIBS", &mut settings.shared_libs);
    flag("GRAPHVIZ_MODULE_LIBS", &mut settings.module_libs);
    flag("GRAPHVIZ_INTERFACE_LIBS", &mut settings.interface_libs);
    flag("GRAPHVIZ_OBJECT_LIBS", &mut settings.object_libs);
    flag("GRAPHVIZ_UNKNOWN_LIBS", &mut settings.unknown_libs);
    flag("GRAPHVIZ_CUSTOM_TARGETS", &mut settings.custom_targets);
    flag("GRAPHVIZ_EXTERNAL_LIBS", &mut settings.externals);
    flag("GRAPHVIZ_GENERATE_PER_TARGET", &mut settings.per_target);
    flag("GRAPHVIZ_GENERATE_DEPENDERS", &mut settings.dependers);
    let mut ignore = String::new();
    text("GRAPHVIZ_IGNORE_TARGETS", &mut ignore);
    settings.ignore.clear();
    if !ignore.is_empty() {
        for pattern in crate::util::expand_list(&ignore, false) {
            let regex = crate::cmregex::Regex::new(&pattern).ok();
            if regex.is_none() {
                eprintln!("Could not compile bad regex \"{pattern}\"");
            }
            settings.ignore.push(regex);
        }
    }
}

/// `cmake::GenerateGraphViz`.
pub fn generate_graphviz(ctx: &GenCtx, file_name: &str) {
    let interp = ctx.interp;
    let mut settings = Settings {
        graph_name: crate::fileapi::top_definition(interp, "CMAKE_PROJECT_NAME")
            .or_else(|| interp.cache.value("CMAKE_PROJECT_NAME").map(str::to_owned))
            .unwrap_or_default(),
        graph_header: "node [\n  fontsize = \"12\"\n];".to_owned(),
        node_prefix: "node".to_owned(),
        executables: true,
        static_libs: true,
        shared_libs: true,
        module_libs: true,
        interface_libs: true,
        object_libs: true,
        unknown_libs: true,
        custom_targets: false,
        externals: true,
        per_target: true,
        dependers: true,
        ignore: Vec::new(),
    };
    read_settings(interp, &mut settings);
    // cmMakefile::GetAliasTargets over every directory.
    let mut aliases: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for (alias, real) in &interp.ext.aliases {
        aliases.entry(real.clone()).or_default().push(alias.clone());
    }
    for ((_, alias), real) in &interp.ext.scoped_aliases {
        aliases.entry(real.clone()).or_default().push(alias.clone());
    }
    for names in aliases.values_mut() {
        names.sort();
        names.dedup();
    }
    let mut writer = Writer {
        ctx,
        file_name: file_name.to_owned(),
        global: String::new(),
        settings,
        next_node_id: 0,
        node_names: HashMap::new(),
        visited_items: BTreeSet::new(),
        visited_links: BTreeSet::new(),
        per_target: Vec::new(),
        dependers: Vec::new(),
        aliases,
    };
    writer.write();
}

impl<'c, 'i> Writer<'c, 'i> {
    fn header(&self, name: &str) -> String {
        format!(
            "digraph \"{}\" {{\n{}\n",
            escape_for_dot_file(name),
            self.settings.graph_header
        )
    }

    fn legend() -> String {
        format!(
            "subgraph clusterLegend {{\n  label = \"Legend\";\n  color = black;\n  edge [ style = invis ];\n  legendNode0 [ label = \"Executable\", shape = egg ];\n  legendNode1 [ label = \"Static Library\", shape = octagon ];\n  legendNode2 [ label = \"Shared Library\", shape = doubleoctagon ];\n  legendNode3 [ label = \"Module Library\", shape = tripleoctagon ];\n  legendNode4 [ label = \"Interface Library\", shape = pentagon ];\n  legendNode5 [ label = \"Object Library\", shape = hexagon ];\n  legendNode6 [ label = \"Unknown Library\", shape = septagon ];\n  legendNode7 [ label = \"Custom Target\", shape = box ];\n  legendNode0 -> legendNode1 [ style = {EDGE_STYLE_PUBLIC} ];\n  legendNode0 -> legendNode2 [ style = {EDGE_STYLE_PUBLIC} ];\n  legendNode0 -> legendNode3;\n  legendNode1 -> legendNode4 [ label = \"Interface\", style = {EDGE_STYLE_INTERFACE} ];\n  legendNode2 -> legendNode5 [ label = \"Private\", style = {EDGE_STYLE_PRIVATE} ];\n  legendNode3 -> legendNode6 [ style = {EDGE_STYLE_PUBLIC} ];\n  legendNode0 -> legendNode7;\n}}\n"
        )
    }

    fn node_name(&self, name: &str) -> String {
        self.node_names.get(name).cloned().unwrap_or_default()
    }

    fn node(&self, item: &Item<'_>) -> String {
        let mut label = item.name.clone();
        if let Some(aliases) = self.aliases.get(&item.name) {
            for alias in aliases {
                label += &format!("\\n({alias})");
            }
        }
        format!(
            "    \"{}\" [ label = \"{}\", shape = {} ];\n",
            self.node_name(&item.name),
            escape_for_dot_file(&label),
            shape(item)
        )
    }

    fn connection(&self, depender: &Item<'_>, dependee: &Item<'_>, style: &str) -> String {
        format!(
            "    \"{}\" -> \"{}\" {style} // {} -> {}\n",
            self.node_name(&depender.name),
            self.node_name(&dependee.name),
            depender.name,
            dependee.name
        )
    }

    fn name_filtered_out(&self, name: &str) -> bool {
        if name == ">" || is_reserved_target(name) {
            return true;
        }
        self.settings
            .ignore
            .iter()
            .flatten()
            .any(|regex| regex.find(name).is_some())
    }

    fn excluded(&self, item: &Item<'_>) -> bool {
        if self.name_filtered_out(&item.name) {
            return true;
        }
        let Some(target) = item.target else {
            return !self.settings.externals;
        };
        if target.kind == TargetKind::Utility
            && (item.name.starts_with("Nightly")
                || item.name.starts_with("Continuous")
                || item.name.starts_with("Experimental"))
        {
            return true;
        }
        if target.imported && !self.settings.externals {
            return true;
        }
        let s = &self.settings;
        !match target.kind {
            TargetKind::Executable => s.executables,
            TargetKind::StaticLibrary => s.static_libs,
            TargetKind::SharedLibrary => s.shared_libs,
            TargetKind::ModuleLibrary => s.module_libs,
            TargetKind::InterfaceLibrary => s.interface_libs,
            TargetKind::ObjectLibrary => s.object_libs,
            TargetKind::UnknownLibrary => s.unknown_libs,
            TargetKind::Utility => s.custom_targets,
            TargetKind::GlobalTarget => false,
        }
    }

    fn write(&mut self) {
        let name = self.settings.graph_name.clone();
        self.global = self.header(&name);
        self.global += &Self::legend();
        // std::set<cmGeneratorTarget const*, StrictTargetComparison>
        let interp = self.ctx.interp;
        let mut roots: Vec<&'i Target> = Vec::new();
        for mf in &interp.makefiles {
            for key in &mf.targets {
                if let Some(target) = interp.ext.targets.get(key)
                    && !target.imported
                    && !is_reserved_target(&target.name)
                    && !target.name.starts_with("__cmake_")
                {
                    roots.push(target);
                }
            }
        }
        roots.sort_by(|a, b| {
            a.name.cmp(&b.name).then_with(|| {
                interp.makefiles[a.makefile]
                    .binary_dir
                    .cmp(&interp.makefiles[b.makefile].binary_dir)
            })
        });
        roots.dedup_by(|a, b| std::ptr::eq(*a, *b));
        for target in roots {
            let item = Item {
                name: target.name.clone(),
                target: Some(target),
            };
            self.visit_item(&item);
        }
        if self.settings.per_target {
            let connections = std::mem::take(&mut self.per_target);
            self.write_per_target(&connections, "", false);
        }
        if self.settings.dependers {
            let connections = std::mem::take(&mut self.dependers);
            self.write_per_target(&connections, ".dependers", true);
        }
        self.global += "}\n";
        // cmGeneratedFileStream creates the parent directory.
        let _ = crate::fsutil::write_generated(
            std::path::Path::new(&self.file_name),
            self.global.as_bytes(),
        );
    }

    fn visit_item(&mut self, item: &Item<'i>) {
        if !self.visited_items.insert(item.name.clone()) {
            return;
        }
        // OnItem
        if !self.excluded(item) {
            let node = format!("{}{}", self.settings.node_prefix, self.next_node_id);
            self.node_names.insert(item.name.clone(), node);
            self.next_node_id += 1;
            let text = self.node(item);
            self.global += &text;
        }
        self.visit_links(item, item);
    }

    /// `cmLinkItemGraphVisitor::GetDependencies`.
    fn dependencies(&self, target: &'i Target) -> BTreeMap<String, (DependencyType, Item<'i>)> {
        let ctx = self.ctx;
        let mut deps: BTreeMap<String, (DependencyType, Item<'i>)> = BTreeMap::new();
        let convert = |item: GraphLinkItem<'i>| Item {
            name: item.name,
            target: item.target,
        };
        let (implementation, interface) = graph_link_libraries(ctx, target, "");
        for lib in implementation.into_iter().flatten() {
            deps.insert(
                lib.name.clone(),
                (DependencyType::LinkPrivate, convert(lib)),
            );
        }
        for lib in interface.into_iter().flatten() {
            let kind = if deps.contains_key(&lib.name) {
                DependencyType::LinkPublic
            } else {
                DependencyType::LinkInterface
            };
            deps.insert(lib.name.clone(), (kind, convert(lib)));
        }
        // GetObjectLibrariesInSources
        let sources = target.props.get("SOURCES").unwrap_or_default();
        for entry in crate::util::expand_list(sources, false) {
            if let Some(name) = entry
                .strip_prefix("$<TARGET_OBJECTS:")
                .and_then(|rest| rest.strip_suffix('>'))
                && !name.contains("$<")
                && let Some(lib) = ctx.target_at(target.makefile, name)
            {
                deps.entry(lib.name.clone()).or_insert((
                    DependencyType::Object,
                    Item {
                        name: lib.name.clone(),
                        target: Some(lib),
                    },
                ));
            }
        }
        // GetUtilityItems
        let mut utilities: Vec<String> = target
            .props
            .get("MANUALLY_ADDED_DEPENDENCIES")
            .map(|value| crate::util::expand_list(value, false))
            .unwrap_or_default();
        utilities.extend(target.traced_utilities.iter().map(|(name, _)| name.clone()));
        for name in utilities {
            let found = ctx.target_at(target.makefile, &name);
            let item = Item {
                name: found.map_or_else(|| name.clone(), |t| t.name.clone()),
                target: found,
            };
            deps.entry(item.name.clone())
                .or_insert((DependencyType::Utility, item));
        }
        deps
    }

    fn visit_links(&mut self, item: &Item<'i>, _root: &Item<'i>) {
        let Some(target) = item.target else {
            return;
        };
        for (_, (kind, dependee)) in self.dependencies(target) {
            self.visit_item(&dependee);
            if !self
                .visited_links
                .insert((item.name.clone(), dependee.name.clone()))
            {
                continue;
            }
            self.visit_link(item, &dependee, true, &edge_style(kind));
            // OnIndirectLink writes nothing.
            self.visit_links(&dependee, &dependee);
            self.visit_links(&dependee, item);
            self.visit_links(&dependee, _root);
        }
    }

    fn visit_link(&mut self, depender: &Item<'i>, dependee: &Item<'i>, direct: bool, scope: &str) {
        if self.excluded(depender) || self.excluded(dependee) || !direct {
            return;
        }
        let text = self.connection(depender, dependee, scope);
        self.global += &text;
        if self.settings.per_target {
            push_connection(&mut self.per_target, depender, dependee, scope);
        }
        if self.settings.dependers {
            push_connection(&mut self.dependers, dependee, depender, scope);
        }
    }

    /// `FindAllConnections`.
    fn find_all(
        map: &[ConnectionEntry<'i>],
        root: &Item<'i>,
        out: &mut Vec<(Item<'i>, Item<'i>, String)>,
        visited: &mut BTreeSet<(Option<usize>, String)>,
    ) {
        let Some((_, _, cons)) = map.iter().find(|(id, _, _)| *id == root.identity()) else {
            return;
        };
        for con in cons {
            out.push((con.src.clone(), con.dst.clone(), con.scope.clone()));
            if visited.insert(con.dst.identity()) {
                Self::find_all(map, &con.dst, out, visited);
            }
        }
    }

    fn write_per_target(&self, connections: &[ConnectionEntry<'i>], suffix: &str, dependers: bool) {
        for (_, root, _) in connections {
            let mut extended = Vec::new();
            let mut visited = BTreeSet::from([root.identity()]);
            Self::find_all(connections, root, &mut extended, &mut visited);
            if self.excluded(root) {
                continue;
            }
            let file = format!(
                "{}.{}{suffix}",
                self.file_name,
                path_safe_string(&root.name)
            );
            let mut text = self.header(&root.name);
            text += &self.node(root);
            for (src, dst, scope) in &extended {
                text += &self.node(dst);
                let (from, to) = if dependers { (dst, src) } else { (src, dst) };
                text += &self.connection(from, to, scope);
            }
            text += "}\n";
            let _ = crate::fsutil::write_generated(std::path::Path::new(&file), text.as_bytes());
        }
    }
}

/// A per-target connection list: (target identity, item, connections).
type ConnectionEntry<'i> = ((Option<usize>, String), Item<'i>, Vec<Connection<'i>>);

fn push_connection<'i>(
    map: &mut Vec<ConnectionEntry<'i>>,
    src: &Item<'i>,
    dst: &Item<'i>,
    scope: &str,
) {
    let id = src.identity();
    let connection = Connection {
        src: src.clone(),
        dst: dst.clone(),
        scope: scope.to_owned(),
    };
    if let Some((_, _, cons)) = map.iter_mut().find(|(key, _, _)| *key == id) {
        cons.push(connection);
    } else {
        map.push((id, src.clone(), vec![connection]));
    }
}
