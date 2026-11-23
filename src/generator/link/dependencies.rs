use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};

use super::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Kind {
    Library,
    Flag,
    Group,
    GroupEnd,
    Object,
    SharedDep,
}

#[derive(Clone)]
pub(super) struct Entry<'a> {
    pub item: Item<'a>,
    pub kind: Kind,
}

pub(super) struct Ordered<'a> {
    pub entries: Vec<Entry<'a>>,
    pub has_implementation: bool,
    pub languages: BTreeSet<String>,
}

struct LinkGraph<'a, 'q, 'i> {
    query: &'a Query<'q, 'i>,
    head: &'a Target,
    entries: Vec<Entry<'i>>,
    index: HashMap<Identity, usize>,
    edges: Vec<Vec<usize>>,
    direct: Vec<usize>,
    objects: Vec<usize>,
    groups: BTreeMap<usize, Vec<usize>>,
    queue: VecDeque<(usize, Option<usize>)>,
    shared_queue: VecDeque<(usize, Item<'i>)>,
    shared_followed: HashSet<usize>,
    inferred: Vec<Option<BTreeSet<usize>>>,
    overrides: HashMap<String, String>,
    languages: BTreeSet<String>,
}

pub(super) fn compute<'i>(query: &Query<'_, 'i>, head: &Target) -> Ordered<'i> {
    let mut overrides = HashMap::new();
    for (key, value) in head.props.iter() {
        if let Some(item) = key.strip_prefix("LINK_LIBRARY_OVERRIDE_")
            && !item.is_empty()
            && !value.is_empty()
        {
            overrides.insert(item.to_owned(), query.eval(value, head, head, key));
        }
    }
    let combined = query.property(head, head, "LINK_LIBRARY_OVERRIDE");
    let mut combined = combined.split(',').filter(|item| !item.is_empty());
    if let Some(feature) = combined.next() {
        for name in combined {
            overrides
                .entry(name.to_owned())
                .or_insert_with(|| feature.to_owned());
        }
    }

    let mut graph = LinkGraph {
        query,
        head,
        overrides,
        entries: Vec::new(),
        index: HashMap::new(),
        edges: Vec::new(),
        direct: Vec::new(),
        objects: Vec::new(),
        groups: BTreeMap::new(),
        queue: VecDeque::new(),
        shared_queue: VecDeque::new(),
        shared_followed: HashSet::new(),
        inferred: Vec::new(),
        languages: dependency_languages(head, query).into_iter().collect(),
    };
    let direct = implementation(query, head);
    let has_implementation = direct.iter().any(|item| !linked_object(query, item));
    graph.add_list(None, direct);
    if !query.language.is_empty() {
        graph.languages.insert(query.language.to_owned());
    }
    graph.follow();
    let entries = graph.finish();
    Ordered {
        entries,
        has_implementation,
        languages: graph.languages,
    }
}

impl<'i> LinkGraph<'_, '_, 'i> {
    fn allocate(&mut self, item: Item<'i>, kind: Kind) -> (usize, bool) {
        let id = item.identity();
        if let Some(index) = self.index.get(&id) {
            return (*index, false);
        }
        let index = self.entries.len();
        self.index.insert(id, index);
        self.entries.push(Entry { item, kind });
        self.edges.push(Vec::new());
        self.inferred.push(None);
        (index, true)
    }

    fn add_edge(&mut self, parent: Option<usize>, child: usize) {
        if let Some(parent) = parent {
            self.edges[parent].push(child);
        } else {
            self.direct.push(child);
        }
    }

    fn add_list(&mut self, parent: Option<usize>, items: Vec<Item<'i>>) {
        let mut inferred: BTreeMap<usize, BTreeSet<usize>> = BTreeMap::new();
        let mut group: Option<(usize, bool, Vec<usize>)> = None;
        let mut objects = Vec::new();
        for mut item in items {
            if item.text == self.head.name {
                continue;
            }
            if let Some(feature) = item.group() {
                item.feature = feature.to_owned();
                let (index, new) = self.allocate(item, Kind::Group);
                self.add_edge(parent, index);
                group = Some((index, new, Vec::new()));
                continue;
            }
            if item.text.starts_with("</LINK_GROUP:") && item.text.ends_with('>') {
                if let Some((index, true, members)) = group.take() {
                    self.groups.insert(index, members);
                }
                continue;
            }
            if linked_object(self.query, &item) {
                objects.push(item);
                continue;
            }
            let mut kind = Kind::Library;
            if item.target.is_none()
                && item.text.starts_with('-')
                && !item.text.starts_with("-l")
                && !item.text.starts_with("-framework")
            {
                kind = Kind::Flag;
            }
            if let Some(feature) = self.overrides.get(&item.text) {
                item.feature = feature.clone();
            }
            // cmComputeLinkDepends::AddLinkEntry: a feature that cannot apply
            // to the item's target is dropped with an AUTHOR_WARNING carrying
            // the head target's backtrace.
            if let Some(target) = item.target {
                if let Some((group_index, _, _)) = &group
                    && !self.index.contains_key(&item.identity())
                    && matches!(
                        target.kind,
                        TargetKind::ObjectLibrary | TargetKind::InterfaceLibrary
                    )
                {
                    let group_feature = self.entries[*group_index].item.feature.clone();
                    self.query.warn(
                        self.head.backtrace.clone(),
                        format!("The feature '{group_feature}', specified as part of a generator-expression '$<LINK_GROUP:{group_feature}>', will not be applied to the {} library '{}'.", if target.kind == TargetKind::ObjectLibrary { "OBJECT" } else { "INTERFACE" }, item.text),
                    );
                }
                if item.feature != DEFAULT && !self.attributes(&item.feature).applies(target.kind) {
                    self.query.warn(
                        self.head.backtrace.clone(),
                        format!("The feature '{0}', specified as part of a generator-expression '$<LINK_LIBRARY:{0}>', will not be applied to the {1} '{2}'.", item.feature, target.kind.type_name(), item.text),
                    );
                }
            }
            if kind == Kind::Flag
                || item
                    .target
                    .is_some_and(|target| !self.attributes(&item.feature).applies(target.kind))
            {
                item.feature = DEFAULT.to_owned();
            }
            let feature = item.feature.clone();
            let (index, new) = self.allocate(item, kind);
            if new {
                let entry = &self.entries[index];
                if entry.item.target.is_some() || self.legacy_depends(&entry.item.text).is_some() {
                    self.queue
                        .push_back((index, group.as_ref().map(|group| group.0)));
                }
            } else if self.entries[index].item.feature != feature {
                self.resolve_mismatch(index, &feature);
            }

            if let Some((group_index, _, members)) = &mut group {
                let group_feature = &self.entries[*group_index].item.feature;
                for (&other, existing) in &self.groups {
                    let other_feature = &self.entries[other].item.feature;
                    if group_feature != other_feature && existing.contains(&index) {
                        self.query.error_at(self.head.backtrace.clone(), format!("Impossible to link target '{}' because the link item '{}', specified with the group feature '{}', has already occurred with the feature '{}', which is not allowed.", self.head.name, self.entries[index].item.text, group_feature, other_feature));
                    }
                }
                members.push(index);
                continue;
            }
            let entry = &self.entries[index];
            let mut dependees: Vec<_> = self
                .groups
                .iter()
                .filter(|(_, members)| {
                    members
                        .iter()
                        .any(|&member| self.entries[member].item.text == entry.item.text)
                })
                .map(|(&group, _)| group)
                .collect();
            if dependees.is_empty() {
                dependees.push(index);
            }
            for dependee in dependees {
                self.add_edge(parent, dependee);
                let entry = &self.entries[dependee];
                if entry.kind == Kind::Library && entry.item.target.is_none() {
                    for (&candidate, set) in &mut inferred {
                        if candidate != index {
                            set.insert(dependee);
                        }
                    }
                    if self.legacy_depends(&entry.item.text).is_none() {
                        inferred.entry(dependee).or_default();
                    }
                }
            }
        }
        for (index, set) in inferred {
            match &mut self.inferred[index] {
                Some(common) => common.retain(|candidate| set.contains(candidate)),
                empty @ None => *empty = Some(set),
            }
        }
        for item in objects {
            let (index, new) = self.allocate(item, Kind::Object);
            if new {
                self.objects.push(index);
            }
        }
    }

    fn legacy_depends(&self, text: &str) -> Option<String> {
        self.query
            .ctx
            .var(self.head.makefile, &format!("{text}_LIB_DEPENDS"))
    }

    fn legacy_items(&self, raw: &str) -> Vec<Item<'i>> {
        let debug_configs = self
            .query
            .ctx
            .interp
            .global_props
            .get("DEBUG_CONFIGURATIONS")
            .unwrap_or("DEBUG");
        let debug = !self.query.config.is_empty()
            && expand_list(debug_configs, false)
                .iter()
                .any(|config| config.eq_ignore_ascii_case(self.query.config));
        let mut kind = None;
        let mut items = Vec::new();
        for value in expand_list(raw, false) {
            if matches!(value.as_str(), "debug" | "optimized" | "general") {
                kind = Some(value);
                continue;
            }
            let kind = kind
                .take()
                .unwrap_or_else(|| self.query.var(self.head, &format!("{value}_LINK_TYPE")));
            if (kind == "debug" && !debug) || (kind == "optimized" && debug) {
                continue;
            }
            items.extend(plain_items(self.query, self.head, &value));
        }
        items
    }

    fn follow(&mut self) {
        while let Some((index, group)) = self.queue.pop_front() {
            let parent = group.unwrap_or(index);
            let item = self.entries[index].item.clone();
            if let Some(target) = item.target {
                let iface = interface(self.query, target, self.head);
                if matches!(
                    target.kind,
                    TargetKind::StaticLibrary | TargetKind::ObjectLibrary
                ) {
                    self.languages
                        .extend(dependency_languages(target, self.query));
                }
                self.add_list(Some(parent), iface.libraries.clone());
                if target.kind != TargetKind::InterfaceLibrary {
                    self.queue_shared(parent, target, &iface, false);
                }
            } else if let Some(deps) = self.legacy_depends(&item.text) {
                self.add_list(Some(parent), self.legacy_items(&deps));
            }
        }
        while let Some((parent, item)) = self.shared_queue.pop_front() {
            let target = item.target;
            let (index, _) = self.allocate(item, Kind::SharedDep);
            self.edges[parent].push(index);
            if let Some(target) = target {
                let iface = interface(self.query, target, self.head);
                self.queue_shared(index, target, &iface, true);
            }
        }
    }

    fn queue_shared(
        &mut self,
        index: usize,
        target: &Target,
        iface: &Interface<'i>,
        follow_interface: bool,
    ) {
        if !self.shared_followed.insert(index) {
            return;
        }
        if follow_interface {
            self.shared_queue
                .extend(iface.libraries.iter().cloned().map(|item| (index, item)));
        }
        let shared = if target.imported {
            plain_items(
                self.query,
                target,
                import_property(
                    target,
                    self.query.config,
                    "IMPORTED_LINK_DEPENDENT_LIBRARIES",
                )
                .unwrap_or_default(),
            )
        } else if matches!(
            target.kind,
            TargetKind::StaticLibrary | TargetKind::SharedLibrary
        ) {
            let mut emitted: HashSet<_> = iface.libraries.iter().map(Item::identity).collect();
            implementation(self.query, target)
                .into_iter()
                .filter(|item| {
                    emitted.insert(item.identity())
                        && item
                            .target
                            .is_some_and(|target| target.kind == TargetKind::SharedLibrary)
                })
                .collect()
        } else {
            Vec::new()
        };
        self.shared_queue
            .extend(shared.into_iter().map(|item| (index, item)));
    }

    fn finish(&mut self) -> Vec<Entry<'i>> {
        let mut member_groups = HashMap::new();
        for (&group, members) in &self.groups {
            for &member in members {
                member_groups.insert(member, group);
            }
        }
        for (edges, inferred) in self.edges.iter_mut().zip(&self.inferred) {
            edges.extend(inferred.iter().flatten().copied());
            for edge in &mut *edges {
                if !matches!(
                    self.entries[*edge].kind,
                    Kind::Flag | Kind::Group | Kind::Object
                ) && let Some(&group) = member_groups.get(edge)
                {
                    *edge = group;
                }
            }
            edges.sort_unstable();
            edges.dedup();
        }

        let components = Components::new(&self.edges);
        for members in &components.members {
            if members.len() > 1
                && members
                    .iter()
                    .any(|&index| self.groups.contains_key(&index))
            {
                use std::fmt::Write as _;
                let describe = |index: usize| {
                    let entry = &self.entries[index];
                    if entry.kind == Kind::Group
                        && let Some((feature, items)) = entry
                            .item
                            .text
                            .strip_prefix("<LINK_GROUP:")
                            .and_then(|tag| tag.strip_suffix('>'))
                            .and_then(|tag| tag.split_once(':'))
                    {
                        return format!("group \"{feature}:{{{}}}\"", items.replace('|', ","));
                    }
                    format!("\"{}\"", entry.item.text)
                };
                let mut message = format!(
                    "The inter-target dependency graph, for the target \"{}\", contains the following strongly connected component (cycle):\n",
                    self.head.name
                );
                for &index in members {
                    let _ = writeln!(message, "  {}", describe(index));
                    for &edge in &self.edges[index] {
                        if components.index[edge] == components.index[index] {
                            let _ = writeln!(message, "    depends on {}", describe(edge));
                        }
                    }
                }
                self.query.error_at(self.head.backtrace.clone(), message);
                return Vec::new();
            }
        }
        let counts = components
            .members
            .iter()
            .map(|members| {
                if members.len() == 1 {
                    return 1;
                }
                members
                    .iter()
                    .filter_map(|&index| self.entries[index].item.target)
                    .map(|target| multiplicity(self.query, target))
                    .fold(2, usize::max)
            })
            .collect();
        let mut schedule = Schedule::new(&components, counts);
        match self
            .head
            .props
            .get("LINK_LIBRARIES_STRATEGY")
            .unwrap_or("REORDER_MINIMALLY")
        {
            "REORDER_FREELY" => {
                for index in &self.direct {
                    schedule.require(components.index[*index]);
                }
            }
            "REORDER_MINIMALLY" => {
                for &index in &self.direct {
                    schedule.emit(index);
                }
            }
            strategy => {
                self.query.error_at(
                    self.head.backtrace.clone(),
                    format!("LINK_LIBRARIES_STRATEGY value '{strategy}' is not recognized."),
                );
                return Vec::new();
            }
        }
        while let Some((_, pending)) = schedule.pending.first_key_value() {
            schedule.emit(
                *pending
                    .remaining
                    .first()
                    .expect("pending component has members"),
            );
        }
        let mut result = self
            .objects
            .iter()
            .map(|&index| self.entries[index].clone())
            .collect::<Vec<_>>();
        let order = self.dedup(schedule.output, member_groups.into_keys().collect());
        for &index in &order {
            let entry = &self.entries[index];
            result.push(entry.clone());
            if let Some(members) = self.groups.get(&index) {
                result.extend(members.iter().map(|&member| self.entries[member].clone()));
                result.push(Entry {
                    item: entry.item.clone(),
                    kind: Kind::GroupEnd,
                });
            }
        }
        // cmComputeLinkDepends::Compute: LINK_DEPENDS_DEBUG_MODE dumps the
        // constraint graph, the strongly connected components, the computed
        // ordering and the final link line to stderr.
        if is_on(&self.query.var(self.head, "CMAKE_LINK_DEPENDS_DEBUG_MODE"))
            || on(self.head, "LINK_DEPENDS_DEBUG_MODE")
        {
            self.display_debug(&components, &order, &result);
        }
        result
    }

    /// `cmComputeLinkDepends::Display{ConstraintGraph,Components,
    /// OrderedEntries,FinalEntries}` — the `LINK_DEPENDS_DEBUG_MODE` dump.
    fn display_debug(&self, components: &Components, order: &[usize], result: &[Entry<'i>]) {
        eprintln!(
            "-------------------------------------------------------------------------------"
        );
        eprintln!(
            "Link dependency analysis for target {}, config {}",
            self.head.name,
            if self.query.config.is_empty() {
                "noconfig"
            } else {
                self.query.config
            }
        );
        {
            use std::fmt::Write as _;
            let mut graph = String::new();
            for (index, edges) in self.edges.iter().enumerate() {
                let _ = writeln!(graph, "item {index} is [{}]", self.entries[index].item.text);
                if !edges.is_empty() {
                    let _ = write!(graph, "  item");
                    for edge in edges {
                        let _ = write!(graph, " {edge}");
                    }
                    let _ = writeln!(graph, " must follow it\n");
                }
            }
            eprint!("{graph}");
        }
        eprintln!("The strongly connected components are:");
        for (component, members) in components.members.iter().enumerate() {
            eprintln!("Component ({component}):");
            for &index in members {
                eprintln!("  item {index} [{}]", self.entries[index].item.text);
            }
            for &edge in &components.edges[component] {
                eprintln!("  followed by Component ({edge})");
            }
            eprintln!(
                "  topo order index {}",
                components.members.len() - 1 - component
            );
        }
        eprintln!();
        eprintln!("target [{}] link dependency ordering:", self.head.name);
        let mut count = 2;
        for &index in order {
            Self::display_entry(&mut count, &self.entries[index]);
        }
        eprintln!();
        eprintln!("target [{}] link line:", self.head.name);
        let mut count = 2;
        for entry in result {
            Self::display_entry(&mut count, entry);
        }
        eprintln!();
    }

    /// `DisplayLinkEntry`: `  target [A]`/`  item [-Flag1]` lines with the
    /// `, feature [x]` suffix; group entries carry the group's indentation.
    fn display_entry(count: &mut usize, entry: &Entry<'_>) {
        match entry.kind {
            Kind::Group => {
                eprint!("  start group");
                *count = 4;
            }
            Kind::GroupEnd => {
                eprint!("  end group");
                *count = 2;
            }
            _ if entry.item.target.is_some() => {
                eprint!(
                    "{:>width$}target [{}]",
                    "",
                    entry.item.target.unwrap().name,
                    width = *count
                );
            }
            _ => {
                eprint!("{:>width$}item [{}]", "", entry.item.text, width = *count);
            }
        }
        if entry.item.feature != DEFAULT {
            eprint!(", feature [{}]", entry.item.feature);
        }
        eprintln!();
    }

    fn dedup(&self, mut entries: Vec<usize>, mut emitted: HashSet<usize>) -> Vec<usize> {
        let mut forward = false;
        let mut mode = "SHARED".to_owned();
        let first_static = self.query.policy(self.head, "CMP0179") == PolicyStatus::New;
        if self.query.policy(self.head, "CMP0156") == PolicyStatus::New {
            let variable = format!("CMAKE_{}_LINK_LIBRARIES_PROCESSING", self.query.language);
            let mut invalid = String::new();
            for option in expand_list(&self.query.var(self.head, &variable), false) {
                match option.split_once('=') {
                    Some(("ORDER", "FORWARD")) => forward = true,
                    Some(("ORDER", "REVERSE")) => forward = false,
                    Some(("DEDUPLICATION" | "UNICITY", value @ ("ALL" | "NONE" | "SHARED"))) => {
                        mode = value.to_owned()
                    }
                    _ => invalid.push_str(&format!("  {option}\n")),
                }
            }
            if !invalid.is_empty() {
                self.query.error_at(
                    self.head.backtrace.clone(),
                    format!("Erroneous option(s) for '{variable}':\n{invalid}"),
                );
            }
            if !first_static
                && mode == "ALL"
                && self.query.var(self.head, "CMAKE_EXECUTABLE_FORMAT") == "ELF"
                && self.query.var(
                    self.head,
                    &format!("CMAKE_{}_PLATFORM_LINKER_ID", self.query.language),
                ) == "LLD"
            {
                mode = "SHARED".to_owned();
            }
        }
        let keep_repeats = |index: usize| {
            let entry = &self.entries[index];
            let attrs = self.attributes(&entry.item.feature);
            let target = entry.item.target;
            if entry.item.feature != DEFAULT
                && target.is_none_or(|target| attrs.applies(target.kind))
                && let Some(dedup) = attrs.dedup
            {
                return !dedup;
            }
            match mode.as_str() {
                "SHARED" => target.is_none_or(|target| target.kind != TargetKind::SharedLibrary),
                "ALL" => entry.kind != Kind::Library,
                _ => true,
            }
        };
        if !forward {
            if mode == "ALL" && first_static {
                let mut seen = emitted.clone();
                entries.retain(|&index| {
                    self.entries[index]
                        .item
                        .target
                        .is_none_or(|target| target.kind != TargetKind::StaticLibrary)
                        || keep_repeats(index)
                        || seen.insert(index)
                });
            }
            entries.reverse();
        }
        entries.retain(|&index| keep_repeats(index) || emitted.insert(index));
        if !forward {
            entries.reverse();
        }
        entries
    }

    fn resolve_mismatch(&mut self, index: usize, feature: &str) {
        let entry = &self.entries[index].item;
        let old = self
            .attributes(&entry.feature)
            .overrides
            .contains(&feature.to_owned());
        let new = self.attributes(feature).overrides.contains(&entry.feature);
        if new && !old {
            self.entries[index].item.feature = feature.to_owned();
        } else if new && old {
            self.query.error_at(self.head.backtrace.clone(), format!("Impossible to link target '{}' because the link item '{}' is specified with the features '{}' and '{}', and both have an 'OVERRIDE' attribute that overrides the other. Such cycles are not allowed.", self.head.name, entry.text, feature, entry.feature));
        } else if !old {
            fn description(feature: &str) -> String {
                if feature == DEFAULT {
                    "without any feature or 'DEFAULT' feature".into()
                } else {
                    format!("with the feature '{feature}'")
                }
            }
            self.query.error_at(self.head.backtrace.clone(), format!("Impossible to link target '{}' because the link item '{}', specified {}, has already occurred {}, which is not allowed.", self.head.name, entry.text, description(feature), description(&entry.feature)));
        }
    }

    fn attributes(&self, feature: &str) -> Attributes {
        if feature == DEFAULT {
            return Attributes::default();
        }
        let mut key = format!(
            "CMAKE_{}_LINK_LIBRARY_{feature}_ATTRIBUTES",
            self.query.language
        );
        let mut value = self.query.var(self.head, &key);
        if value.is_empty() {
            key = format!("CMAKE_LINK_LIBRARY_{feature}_ATTRIBUTES");
            value = self.query.var(self.head, &key);
        }
        let mut attrs = Attributes::default();
        let mut invalid = String::new();
        for option in expand_list(&value, false) {
            match option.split_once('=') {
                Some(("DEDUPLICATION", "YES")) => attrs.dedup = Some(true),
                Some(("DEDUPLICATION", "NO")) => attrs.dedup = Some(false),
                Some(("DEDUPLICATION", "DEFAULT")) => attrs.dedup = None,
                Some(("OVERRIDE", list))
                    if list.split(',').all(|name| {
                        !name.is_empty()
                            && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
                    }) =>
                {
                    attrs.overrides = list.split(',').map(str::to_owned).collect();
                }
                Some(("LIBRARY_TYPE", kinds)) => {
                    let types: Option<Vec<_>> = kinds
                        .split(',')
                        .map(|kind| match kind {
                            "STATIC" => Some(TargetKind::StaticLibrary),
                            "SHARED" => Some(TargetKind::SharedLibrary),
                            "MODULE" => Some(TargetKind::ModuleLibrary),
                            "EXECUTABLE" => Some(TargetKind::Executable),
                            _ => None,
                        })
                        .collect();
                    if let Some(types) = types {
                        attrs.types = Some(types)
                    } else {
                        invalid.push_str(&format!("  {option}\n"))
                    }
                }
                _ => invalid.push_str(&format!("  {option}\n")),
            }
        }
        if !invalid.is_empty() {
            // cmComputeLinkDepends::GetFeatureAttributes: this IssueMessage
            // site passes no backtrace, so the diagnostic has no location.
            self.query
                .error(format!("Erroneous option(s) for '{key}':\n{invalid}"));
        }
        attrs
    }
}

#[derive(Default)]
struct Attributes {
    types: Option<Vec<TargetKind>>,
    dedup: Option<bool>,
    overrides: Vec<String>,
}

impl Attributes {
    fn applies(&self, kind: TargetKind) -> bool {
        kind == TargetKind::UnknownLibrary
            || self.types.as_ref().map_or_else(
                || {
                    matches!(
                        kind,
                        TargetKind::StaticLibrary
                            | TargetKind::SharedLibrary
                            | TargetKind::ModuleLibrary
                            | TargetKind::Executable
                    )
                },
                |types| types.contains(&kind),
            )
    }
}

fn multiplicity(query: &Query<'_, '_>, target: &Target) -> usize {
    let value = if target.imported {
        import_property(target, query.config, "IMPORTED_LINK_INTERFACE_MULTIPLICITY")
    } else if target.kind == TargetKind::StaticLibrary {
        target
            .props
            .get(&format!(
                "LINK_INTERFACE_MULTIPLICITY{}",
                config_suffix(query.config)
            ))
            .or_else(|| target.props.get("LINK_INTERFACE_MULTIPLICITY"))
    } else {
        None
    };
    value
        .and_then(|value| {
            value
                .trim_start()
                .trim_start_matches('+')
                .split(|ch: char| !ch.is_ascii_digit())
                .next()?
                .parse()
                .ok()
        })
        .unwrap_or(0)
}

fn linked_object(query: &Query<'_, '_>, item: &Item<'_>) -> bool {
    item.target.is_none() && linked_object_owner(query, &item.text).is_some()
}

pub(super) fn linked_object_owner<'i>(query: &Query<'_, 'i>, path: &str) -> Option<&'i Target> {
    if !Path::new(path).is_absolute() {
        return None;
    }
    // INTEGRATION: Prefer the source record's EXTERNAL_OBJECT and owning
    // object-library metadata once available to the generator.
    query
        .ctx
        .targets_in_order()
        .into_iter()
        .filter(|target| target.kind == TargetKind::ObjectLibrary)
        .find(|target| {
            if target.imported {
                expand_list(
                    import_property(target, query.config, "IMPORTED_OBJECTS").unwrap_or_default(),
                    false,
                )
                .iter()
                .any(|object| object == path)
            } else {
                (path.ends_with(".o") || path.ends_with(".obj"))
                    && Path::new(path).starts_with(query.generator.object_dir(
                        query.ctx,
                        target,
                        query.config,
                    ))
            }
        })
}

/// SCCs indexed by DFS completion order, as in cmComputeComponentGraph.
pub(super) struct Components {
    pub index: Vec<usize>,
    pub members: Vec<Vec<usize>>,
    pub edges: Vec<Vec<usize>>,
}

impl Components {
    pub fn new(graph: &[Vec<usize>]) -> Self {
        let mut transpose = vec![Vec::new(); graph.len()];
        for (node, edges) in graph.iter().enumerate() {
            for &edge in edges {
                transpose[edge].push(node);
            }
        }
        let order = postorder(graph, 0..graph.len());
        let mut visited = vec![false; graph.len()];
        let mut members = Vec::new();
        for &node in order.iter().rev() {
            if visited[node] {
                continue;
            }
            let mut component = Vec::new();
            let mut work = vec![node];
            while let Some(node) = work.pop() {
                if !std::mem::replace(&mut visited[node], true) {
                    component.push(node);
                    work.extend(&transpose[node]);
                }
            }
            component.sort_unstable();
            members.push(component);
        }
        let mut finish = vec![0; graph.len()];
        for (position, node) in order.into_iter().enumerate() {
            finish[node] = position;
        }
        members.sort_by_key(|component| component.iter().map(|&node| finish[node]).max());
        let mut index = vec![0; graph.len()];
        for (component, nodes) in members.iter().enumerate() {
            for &node in nodes {
                index[node] = component;
            }
        }
        let mut edges = vec![Vec::new(); members.len()];
        for (node, outgoing) in graph.iter().enumerate() {
            let component = index[node];
            edges[component].extend(
                outgoing
                    .iter()
                    .map(|&edge| index[edge])
                    .filter(|&edge| edge != component),
            );
        }
        Self {
            index,
            members,
            edges,
        }
    }
}

pub(super) fn postorder(
    graph: &[Vec<usize>],
    roots: impl IntoIterator<Item = usize>,
) -> Vec<usize> {
    let mut visited = vec![false; graph.len()];
    let mut output = Vec::new();
    for root in roots {
        let mut work = vec![(root, false)];
        while let Some((node, finish)) = work.pop() {
            if finish {
                output.push(node);
            } else if !std::mem::replace(&mut visited[node], true) {
                work.push((node, true));
                work.extend(graph[node].iter().rev().map(|&edge| (edge, false)));
            }
        }
    }
    output
}

struct Pending {
    component: usize,
    count: usize,
    remaining: BTreeSet<usize>,
}

struct Schedule<'a> {
    components: &'a Components,
    priority: Vec<usize>,
    counts: Vec<usize>,
    pending: BTreeMap<usize, Pending>,
    output: Vec<usize>,
}

impl<'a> Schedule<'a> {
    fn new(components: &'a Components, counts: Vec<usize>) -> Self {
        let edges: Vec<_> = components
            .edges
            .iter()
            .map(|edges| edges.iter().rev().copied().collect())
            .collect();
        let mut priority = vec![0; edges.len()];
        for (priority_id, component) in postorder(&edges, (0..edges.len()).rev())
            .into_iter()
            .rev()
            .enumerate()
        {
            priority[component] = priority_id;
        }
        Self {
            components,
            priority,
            counts,
            pending: BTreeMap::new(),
            output: Vec::new(),
        }
    }

    fn require(&mut self, component: usize) {
        let pending = self
            .pending
            .entry(self.priority[component])
            .or_insert_with(|| Pending {
                component,
                count: 0,
                remaining: BTreeSet::new(),
            });
        pending.count = self.counts[component];
        pending
            .remaining
            .extend(&self.components.members[component]);
    }

    fn emit(&mut self, index: usize) {
        self.output.push(index);
        let component = self.components.index[index];
        let priority = self.priority[component];
        if self.counts[component] > 1 && !self.pending.contains_key(&priority) {
            self.require(component);
        }
        if let Some(pending) = self.pending.get_mut(&priority) {
            pending.remaining.remove(&index);
            if !pending.remaining.is_empty() {
                return;
            }
            pending.count -= 1;
            if pending.count > 0 {
                pending
                    .remaining
                    .extend(&self.components.members[pending.component]);
                return;
            }
            self.pending.remove(&priority);
        }
        for &dependency in &self.components.edges[component] {
            self.require(dependency);
        }
    }
}
