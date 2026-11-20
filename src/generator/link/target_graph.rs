//! Target-level dependencies and static-library cycle linearization.

use std::collections::{HashMap, HashSet};

use super::dependencies::Components;
use super::*;

#[derive(Clone, Copy)]
struct Edge {
    to: usize,
    strong: bool,
}

pub(super) fn dependencies(ctx: &GenCtx, head: &Target, config: &str) -> Vec<String> {
    let key = format!("link:target-dependencies:{config}:{}", head.name);
    if let Some(value) = ctx.memo.borrow().get(&key) {
        return expand_list(value, false);
    }
    let query = Query::new(ctx, config, "");
    for (target, deps) in compute(&query) {
        ctx.memo.borrow_mut().insert(
            format!("link:target-dependencies:{config}:{}", target.name),
            deps.join(";"),
        );
    }
    ctx.memo
        .borrow()
        .get(&key)
        .map(|value| expand_list(value, false))
        .unwrap_or_default()
}

pub(super) fn compute<'i>(query: &Query<'_, 'i>) -> Vec<(&'i Target, Vec<String>)> {
    let mut targets = query.ctx.targets_in_order();
    targets.retain(|target| in_build_system(target));
    targets.sort_by_key(|target| target.makefile);
    let indices: HashMap<_, _> = targets
        .iter()
        .enumerate()
        .map(|(index, &target)| (std::ptr::from_ref(target) as usize, index))
        .collect();

    let mut initial = Vec::new();
    for (index, target) in targets.iter().enumerate() {
        let language = if matches!(
            target.kind,
            TargetKind::Executable
                | TargetKind::StaticLibrary
                | TargetKind::ObjectLibrary
                | TargetKind::SharedLibrary
                | TargetKind::ModuleLibrary
        ) && ["LINK_LIBRARIES", "SOURCES"].into_iter().any(|property| {
            target
                .props
                .get(property)
                .is_some_and(|value| value.contains("$<"))
        }) {
            query.generator.language(query.ctx, target, query.config)
        } else {
            query.language.to_owned()
        };
        let query = Query {
            language: &language,
            ..*query
        };
        let mut collector = Collector {
            query: &query,
            head: target,
            head_index: index,
            indices: &indices,
            emitted: HashSet::from([std::ptr::from_ref(*target) as usize]),
            utility_followed: HashSet::new(),
            edges: Vec::new(),
        };
        let links = implementation(&query, target);
        collector.follow_interface(&links);
        collector.follow_utilities(target, true);
        if let Some(sources) = target.props.get("SOURCES") {
            for source in expand_list(sources, false) {
                collector.add_object_source(&source);
            }
        }
        initial.push(collector.edges);
    }

    let components = self::components(&initial);
    let no_cycles = is_on_opt(
        query
            .ctx
            .interp
            .global_props
            .get("GLOBAL_DEPENDS_NO_CYCLES"),
    );
    if let Some(component) = invalid_component(&components, &targets, no_cycles) {
        cycle_error(
            query,
            &targets,
            &initial,
            &components,
            component,
            no_cycles,
            false,
        );
        return targets
            .into_iter()
            .map(|target| (target, Vec::new()))
            .collect();
    }

    let mut intermediate = initial.clone();
    for (index, target) in targets.iter().enumerate() {
        if matches!(
            target.kind,
            TargetKind::StaticLibrary | TargetKind::ObjectLibrary
        ) && on(target, "OPTIMIZE_DEPENDENCIES")
        {
            let mut optimized = Vec::new();
            let mut visited = HashSet::new();
            for edge in &initial[index] {
                if edge.strong {
                    optimized.push(*edge);
                    continue;
                }
                let mut work = vec![edge.to];
                while let Some(to) = work.pop() {
                    if !visited.insert(to) {
                        continue;
                    }
                    let target = targets[to];
                    // INTEGRATION: Generated sources/custom commands and
                    // non-C language side effects need the corresponding
                    // configure model records.
                    if target.kind == TargetKind::Utility
                        || target.props.contains("CUSTOM_COMMAND_SIDE_EFFECTS")
                    {
                        optimized.push(Edge { to, strong: false });
                    }
                    work.extend(initial[to].iter().map(|edge| edge.to));
                }
            }
            intermediate[index] = optimized;
        }
    }

    let components = self::components(&intermediate);
    let mut heads = vec![0; components.members.len()];
    let mut tails = vec![0; components.members.len()];
    let mut result = vec![Vec::new(); targets.len()];
    for (component, nodes) in components.members.iter().enumerate() {
        let mut linear = LinearComponent {
            component,
            components: &components,
            edges: &intermediate,
            emitted: HashSet::new(),
            active: HashSet::new(),
            order: Vec::with_capacity(nodes.len()),
            result: &mut result,
        };
        for &node in nodes.iter().rev() {
            if !linear.visit(node) {
                cycle_error(
                    query,
                    &targets,
                    &initial,
                    &components,
                    component,
                    no_cycles,
                    true,
                );
                return targets
                    .into_iter()
                    .map(|target| (target, Vec::new()))
                    .collect();
            }
        }
        if let Some(&tail) = linear.order.first() {
            tails[component] = tail;
        }
        if let Some(&head) = linear.order.last() {
            heads[component] = head;
        }
        for pair in linear.order.windows(2) {
            result[pair[1]].push(pair[0]);
        }
    }
    for (node, edges) in intermediate.iter().enumerate() {
        let source = components.index[node];
        for edge in edges {
            let dest = components.index[edge.to];
            if source != dest {
                result[tails[source]].push(heads[dest]);
            }
        }
    }
    targets
        .iter()
        .zip(result)
        .map(|(&target, dependencies)| {
            (
                target,
                unique(
                    dependencies
                        .into_iter()
                        .map(|index| targets[index].name.clone()),
                ),
            )
        })
        .collect()
}

fn in_build_system(target: &Target) -> bool {
    !target.imported
        && target.alias_of.is_none()
        && match target.kind {
            TargetKind::UnknownLibrary => false,
            TargetKind::InterfaceLibrary => ["SOURCES", "HEADER_SETS", "CXX_MODULE_SETS"]
                .into_iter()
                .any(|key| target.props.get(key).is_some_and(|value| !value.is_empty())),
            _ => true,
        }
}

fn components(graph: &[Vec<Edge>]) -> Components {
    Components::new(
        &graph
            .iter()
            .map(|edges| edges.iter().map(|edge| edge.to).collect())
            .collect::<Vec<_>>(),
    )
}

fn invalid_component(
    components: &Components,
    targets: &[&Target],
    no_cycles: bool,
) -> Option<usize> {
    components.members.iter().position(|nodes| {
        nodes.len() > 1
            && (no_cycles
                || nodes
                    .iter()
                    .any(|&index| targets[index].kind != TargetKind::StaticLibrary))
    })
}

fn cycle_error(
    query: &Query<'_, '_>,
    targets: &[&Target],
    graph: &[Vec<Edge>],
    components: &Components,
    component: usize,
    no_cycles: bool,
    strong: bool,
) {
    use std::fmt::Write as _;
    let mut message = String::from(
        "The inter-target dependency graph contains the following strongly connected component (cycle):\n",
    );
    for &index in &components.members[component] {
        let target = targets[index];
        let _ = writeln!(
            message,
            "  \"{}\" of type {}",
            target.name,
            target.kind.type_name()
        );
        for edge in graph[index]
            .iter()
            .filter(|edge| components.index[edge.to] == component)
        {
            let _ = writeln!(
                message,
                "    depends on \"{}\" ({})",
                targets[edge.to].name,
                if edge.strong { "strong" } else { "weak" }
            );
        }
    }
    message.push_str(if strong {
        "The component contains at least one cycle consisting of strong dependencies (created by add_dependencies) that cannot be broken."
    } else if no_cycles {
        "The GLOBAL_DEPENDS_NO_CYCLES global property is enabled, so cyclic dependencies are not allowed even among static libraries."
    } else {
        "At least one of these targets is not a STATIC_LIBRARY.  Cyclic dependencies are allowed only among static libraries."
    });
    query.error(message);
}

struct LinearComponent<'a> {
    component: usize,
    components: &'a Components,
    edges: &'a [Vec<Edge>],
    emitted: HashSet<usize>,
    active: HashSet<usize>,
    order: Vec<usize>,
    result: &'a mut [Vec<usize>],
}

impl LinearComponent<'_> {
    fn visit(&mut self, node: usize) -> bool {
        if self.active.contains(&node) {
            return false;
        }
        if !self.emitted.insert(node) {
            return true;
        }
        self.active.insert(node);
        for &edge in &self.edges[node] {
            if !edge.strong || self.components.index[edge.to] != self.component {
                continue;
            }
            self.result[node].push(edge.to);
            if !self.visit(edge.to) {
                return false;
            }
        }
        self.active.remove(&node);
        self.order.push(node);
        true
    }
}

struct Collector<'a, 'q, 'i> {
    query: &'a Query<'q, 'i>,
    head: &'a Target,
    head_index: usize,
    indices: &'a HashMap<usize, usize>,
    emitted: HashSet<usize>,
    utility_followed: HashSet<usize>,
    edges: Vec<Edge>,
}

impl<'i> Collector<'_, '_, 'i> {
    fn follow_interface(&mut self, items: &[Item<'i>]) {
        for item in items {
            let Some(target) = item.target else {
                self.add_object_source(&item.text);
                continue;
            };
            if self.emitted.insert(std::ptr::from_ref(target) as usize) {
                self.add(target, false);
                self.follow_interface(&interface(self.query, target, self.head).libraries);
            }
        }
    }

    fn add(&mut self, target: &Target, strong: bool) {
        if let Some(&to) = self.indices.get(&(std::ptr::from_ref(target) as usize)) {
            if to != self.head_index {
                self.edges.push(Edge { to, strong });
            }
        } else {
            self.follow_utilities(target, false);
        }
    }

    fn follow_utilities(&mut self, target: &Target, report_missing: bool) {
        if !self
            .utility_followed
            .insert(std::ptr::from_ref(target) as usize)
        {
            return;
        }
        // INTEGRATION: Feed target dependencies inferred by custom command
        // evaluation, including executable argv[0] and target genex uses.
        for property in [
            "MANUALLY_ADDED_DEPENDENCIES",
            "CUSTOM_COMMAND_TARGET_DEPENDS",
        ] {
            let mut deps = expand_list(target.props.get(property).unwrap_or_default(), false);
            deps.sort_unstable();
            deps.dedup();
            for name in deps {
                if let Some(dependee) = self.query.resolve(target.makefile, &name) {
                    self.add(dependee, true);
                } else if report_missing && target.kind != TargetKind::GlobalTarget {
                    self.query.error(format!(
                        "The dependency target \"{name}\" of target \"{}\" does not exist.",
                        target.name
                    ));
                }
            }
        }
    }

    fn add_object_source(&mut self, value: &str) {
        let owner = value
            .strip_prefix("$<TARGET_OBJECTS:")
            .and_then(|name| name.strip_suffix('>'))
            .and_then(|name| self.query.resolve(self.head.makefile, name))
            .or_else(|| super::dependencies::linked_object_owner(self.query, value));
        if let Some(target) = owner.filter(|target| target.kind == TargetKind::ObjectLibrary) {
            self.add(target, true);
        }
    }
}
