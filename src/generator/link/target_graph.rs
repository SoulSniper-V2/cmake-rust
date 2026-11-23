//! Target-level dependencies and static-library cycle linearization.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::rc::Rc;

use crate::messages::Frame;

use super::dependencies::Components;
use super::*;

#[derive(Clone)]
struct Edge {
    to: usize,
    strong: bool,
    /// `cmGraphEdge::IsCross`: a cross-configuration dependency (a custom
    /// command naming the target, evaluated in the command config).
    cross: bool,
    /// `cmGraphEdge::Backtrace`: the call site that created this dependency.
    backtrace: Rc<Vec<Frame>>,
}

/// A `cmComputeTargetDepends::FinalGraph` edge.
#[derive(Clone)]
pub(crate) struct DepEdge {
    pub to: usize,
    pub strong: bool,
    pub cross: bool,
    pub backtrace: Rc<Vec<Frame>>,
}

/// `cmTargetDepend`: the combination of all the final edges to one dependee.
pub struct TargetDepend {
    /// The dependee's `ext.targets` key.
    pub key: String,
    /// `IsLink`: some edge is a (weak) link dependency.
    pub link: bool,
    /// `IsCross`, as the last edge set it.
    pub cross: bool,
    /// As the last edge set it.
    pub backtrace: Rc<Vec<Frame>>,
}

/// The whole `cmComputeTargetDepends` result: the in-build-system targets in
/// `cmGlobalGenerator`'s target order plus each one's `FinalGraph` edges.
pub(crate) struct DepGraph<'i> {
    pub targets: Vec<&'i Target>,
    pub edges: Vec<Vec<DepEdge>>,
}

/// `cmGlobalGenerator::GetTargetDirectDepends`: `head`'s dependees in the
/// global generator's target order. The graph unites all configurations.
pub(crate) fn direct_depends(ctx: &GenCtx, head: &Target) -> Rc<Vec<TargetDepend>> {
    let cached = ctx.dep_graph.borrow().clone();
    let graph = cached.unwrap_or_else(|| {
        let graph = Rc::new(compute_all(ctx));
        *ctx.dep_graph.borrow_mut() = Some(graph.clone());
        graph
    });
    graph
        .get(&(std::ptr::from_ref(head) as usize))
        .cloned()
        .unwrap_or_default()
}

fn compute_all(ctx: &GenCtx) -> HashMap<usize, Rc<Vec<TargetDepend>>> {
    let contexts: Vec<GenCtx> = if ctx.multi_config {
        ctx.configs
            .iter()
            .map(|config| ctx.for_config(config))
            .collect()
    } else {
        vec![ctx.clone()]
    };
    let queries: Vec<Query> = contexts
        .iter()
        .map(|ctx| Query::new(ctx, &ctx.config, ""))
        .collect();
    let graph = compute(&queries);
    // Target pointers to their `ext.targets` keys (which can be the
    // scoped `[dir]name` form for imported/duplicate targets).
    let keys: HashMap<usize, &str> = ctx
        .interp
        .ext
        .targets
        .iter()
        .map(|(key, target)| (std::ptr::from_ref(target) as usize, key.as_str()))
        .collect();
    let mut result = HashMap::new();
    for (target, edges) in graph.targets.iter().zip(&graph.edges) {
        let mut depends: BTreeMap<usize, TargetDepend> = BTreeMap::new();
        for edge in edges {
            let dependee = std::ptr::from_ref(graph.targets[edge.to]) as usize;
            let Some(key) = keys.get(&dependee) else {
                continue;
            };
            let depend = depends.entry(edge.to).or_insert_with(|| TargetDepend {
                key: (*key).to_owned(),
                link: false,
                cross: false,
                backtrace: Rc::default(),
            });
            depend.link |= !edge.strong;
            depend.cross = edge.cross;
            depend.backtrace = edge.backtrace.clone();
        }
        result.insert(
            std::ptr::from_ref(*target) as usize,
            Rc::new(depends.into_values().collect()),
        );
    }
    result
}

/// `cmComputeTargetDepends::Compute`, with one query per configuration.
pub(crate) fn compute<'i>(queries: &[Query<'_, 'i>]) -> DepGraph<'i> {
    let query = &queries[0];
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
        let language_sensitive = matches!(
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
        });
        // CollectTargetDepends: the union over all configurations.
        let mut edges = Vec::new();
        let mut emitted = HashSet::from([std::ptr::from_ref(*target) as usize]);
        let mut utility_followed = HashSet::new();
        for (config, query) in queries.iter().enumerate() {
            let language = if language_sensitive {
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
                emitted,
                utility_followed,
                edges,
            };
            let links = implementation(&query, target);
            collector.follow_interface(&links, None);
            if config == 0 {
                collector.follow_utilities(target, true);
            }
            for source in target::sources(query.ctx, target, query.config).iter() {
                if source.kind == target::SourceKind::ExternalObject {
                    collector.add_object_source(&source.path);
                }
            }
            Collector {
                emitted,
                utility_followed,
                edges,
                ..
            } = collector;
        }
        initial.push(edges);
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
        return DepGraph {
            targets,
            edges: Vec::new(),
        };
    }

    // cmComputeTargetDepends::CollectSideEffects: each target's side-effect
    // sets contain itself (when it produces side effects) plus the union of
    // all transitively reachable dependees' sets.
    let count = targets.len();
    let mut cc_side: Vec<BTreeSet<usize>> = vec![BTreeSet::new(); count];
    let mut lang_side: Vec<HashMap<String, BTreeSet<usize>>> = vec![HashMap::new(); count];
    {
        let mut visited = vec![false; count];
        for index in 0..count {
            collect_side_effects(
                queries,
                &targets,
                &initial,
                index,
                &mut visited,
                &mut cc_side,
                &mut lang_side,
            );
        }
    }

    let mut intermediate = initial.clone();
    for (index, target) in targets.iter().enumerate() {
        if matches!(
            target.kind,
            TargetKind::StaticLibrary | TargetKind::ObjectLibrary
        ) && on(target, "OPTIMIZE_DEPENDENCIES")
        {
            let mut optimized = Vec::new();
            let mut emitted = HashSet::new();
            for edge in &initial[index] {
                if edge.strong {
                    optimized.push(edge.clone());
                    continue;
                }
                // cmComputeTargetDepends::OptimizeLinkDependencies: keep
                // edges to transitive dependees with custom-command side
                // effects or side effects in the depender's languages.
                for &dep in &cc_side[edge.to] {
                    if emitted.insert(dep) {
                        optimized.push(Edge {
                            to: dep,
                            strong: false,
                            cross: edge.cross,
                            backtrace: edge.backtrace.clone(),
                        });
                    }
                }
                for lang in compile_languages(queries, target) {
                    if let Some(set) = lang_side[edge.to].get(&lang) {
                        for &dep in set {
                            if emitted.insert(dep) {
                                optimized.push(Edge {
                                    to: dep,
                                    strong: false,
                                    cross: edge.cross,
                                    backtrace: edge.backtrace.clone(),
                                });
                            }
                        }
                    }
                }
            }
            intermediate[index] = optimized;
        }
    }

    let components = self::components(&intermediate);
    let mut heads = vec![0; components.members.len()];
    let mut tails = vec![0; components.members.len()];
    let mut result: Vec<Vec<DepEdge>> = vec![Vec::new(); targets.len()];
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
                return DepGraph {
                    targets,
                    edges: Vec::new(),
                };
            }
        }
        if let Some(&tail) = linear.order.first() {
            tails[component] = tail;
        }
        if let Some(&head) = linear.order.last() {
            heads[component] = head;
        }
        for pair in linear.order.windows(2) {
            // The linking edges that linearize a component carry no
            // backtrace (`cmListFileBacktrace()` in IntraComponent).
            result[pair[1]].push(DepEdge {
                to: pair[0],
                strong: false,
                cross: false,
                backtrace: Rc::default(),
            });
        }
    }
    for (node, edges) in intermediate.iter().enumerate() {
        let source = components.index[node];
        for edge in edges {
            let dest = components.index[edge.to];
            if source != dest {
                result[tails[source]].push(DepEdge {
                    to: heads[dest],
                    strong: edge.strong,
                    cross: edge.cross,
                    backtrace: edge.backtrace.clone(),
                });
            }
        }
    }
    DepGraph {
        targets,
        edges: result,
    }
}

pub(crate) fn in_build_system(target: &Target) -> bool {
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
    if !query
        .ctx
        .errors
        .borrow()
        .iter()
        .any(|error| error.text() == message)
    {
        query.ctx.system_error(message);
    }
}

struct LinearComponent<'a> {
    component: usize,
    components: &'a Components,
    edges: &'a [Vec<Edge>],
    emitted: HashSet<usize>,
    active: HashSet<usize>,
    order: Vec<usize>,
    result: &'a mut [Vec<DepEdge>],
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
        for edge in &self.edges[node] {
            if !edge.strong || self.components.index[edge.to] != self.component {
                continue;
            }
            self.result[node].push(DepEdge {
                to: edge.to,
                strong: true,
                cross: edge.cross,
                backtrace: edge.backtrace.clone(),
            });
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
    /// `cmComputeTargetDepends::AddInterfaceDepends`. `inherited` is the
    /// backtrace of the top-level link dependency whose interface is being
    /// followed, which CMake injects into every transitive entry.
    fn follow_interface(&mut self, items: &[Item<'i>], inherited: Option<Rc<Vec<Frame>>>) {
        for item in items {
            let Some(target) = item.target else {
                continue;
            };
            if self.emitted.insert(std::ptr::from_ref(target) as usize) {
                let backtrace = inherited
                    .clone()
                    .unwrap_or_else(|| Rc::new(item.backtrace.clone()));
                // A linked executable without exports is treated as an
                // external file, not a target dependency.
                let no_exports = target.kind == TargetKind::Executable
                    && !is_on_opt(target.props.get("ENABLE_EXPORTS"))
                    && !is_on_opt(target.props.get("WINDOWS_EXPORT_ALL_SYMBOLS"))
                    && !is_on_opt(target.props.get("CYGWIN_EXPORT_ALL_SYMBOLS"));
                if !no_exports {
                    self.add(target, false, backtrace.clone());
                    self.follow_interface(
                        &interface(self.query, target, self.head).libraries,
                        Some(backtrace),
                    );
                }
            }
        }
        for item in items.iter().filter(|item| item.object) {
            self.add_object_source(&item.text);
        }
    }

    fn add(&mut self, target: &Target, strong: bool, backtrace: Rc<Vec<Frame>>) {
        self.add_edge(target, strong, false, backtrace);
    }

    fn add_edge(&mut self, target: &Target, strong: bool, cross: bool, backtrace: Rc<Vec<Frame>>) {
        if let Some(&to) = self.indices.get(&(std::ptr::from_ref(target) as usize)) {
            if to != self.head_index {
                self.edges.push(Edge {
                    to,
                    strong,
                    cross,
                    backtrace,
                });
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
        // cmGeneratorTarget::GetUtilityItems: add_dependencies plus the
        // utilities found by dependency tracing.
        {
            // std::set<BT<std::pair<std::string, bool>>>: one item per
            // (name, cross) pair, ordered by name then cross.
            let mut deps: Vec<(String, bool, Rc<Vec<Frame>>)> = expand_list(
                target
                    .props
                    .get("MANUALLY_ADDED_DEPENDENCIES")
                    .unwrap_or_default(),
                false,
            )
            .into_iter()
            .map(|name| {
                let backtrace = target
                    .dependencies
                    .get(&name)
                    .map(|frames| Rc::new(frames.clone()))
                    .unwrap_or_default();
                (name, false, backtrace)
            })
            .collect();
            deps.extend(
                target
                    .traced_utilities
                    .iter()
                    .map(|(name, cross)| (name.clone(), *cross, Rc::new(Vec::new()))),
            );
            deps.sort_by(|(a, ac, _), (b, bc, _)| (a, ac).cmp(&(b, bc)));
            deps.dedup_by(|(a, ac, _), (b, bc, _)| a == b && ac == bc);
            for (name, cross, backtrace) in deps {
                if let Some(dependee) = self.query.resolve(target.makefile, &name) {
                    self.add_edge(dependee, true, cross, backtrace);
                } else if report_missing && target.kind != TargetKind::GlobalTarget {
                    // cmComputeTargetDepends::CheckTarget uses the
                    // dependee name's recorded add_dependencies backtrace.
                    self.query.error_at(
                        (*backtrace).clone(),
                        format!(
                            "The dependency target \"{name}\" of target \"{}\" does not exist.",
                            target.name
                        ),
                    );
                }
            }
        }
    }

    /// `cmComputeTargetDepends::AddObjectDepends`.
    fn add_object_source(&mut self, path: &str) {
        let directory = self.head.makefile;
        if let Some(library) = target::target_object_library(self.query.ctx, directory, path)
            .and_then(|name| self.query.resolve(directory, &name))
            && self.emitted.insert(std::ptr::from_ref(library) as usize)
        {
            // AddObjectDepends records a utility dependency captured after
            // configure; its backtrace is empty.
            self.add(library, true, Rc::new(Vec::new()));
        }
    }
}

/// `cmGeneratorTarget::AppendCustomCommandSideEffects`: the target produces
/// build side effects if it has target-level custom commands or a source
/// with an attached custom command.
fn has_custom_command_side_effect(queries: &[Query<'_, '_>], target: &Target) -> bool {
    if !target.custom_commands.is_empty() {
        return true;
    }
    queries.iter().any(|query| {
        target::sources(query.ctx, target, query.config)
            .iter()
            .any(|source| {
                crate::commands::target::find_known_source(
                    query.ctx.interp,
                    target.makefile,
                    &source.path,
                )
                .is_some_and(|index| {
                    query.ctx.interp.ext.sources[&target.makefile][index]
                        .custom_command
                        .is_some()
                })
            })
    })
}

/// `cmGeneratorTarget::GetAllConfigCompileLanguages` restricted to the
/// languages that do not have side effects of their own.
fn compile_languages(queries: &[Query<'_, '_>], target: &Target) -> BTreeSet<String> {
    queries
        .iter()
        .flat_map(|query| {
            target::sources(query.ctx, target, query.config)
                .iter()
                .map(|source| source.language.clone())
                .collect::<Vec<_>>()
        })
        .filter(|language| !language.is_empty())
        .collect()
}

/// `cmComputeTargetDepends::CollectSideEffectsForTarget`.
#[allow(clippy::too_many_arguments, reason = "mirrors the C++ accumulation")]
fn collect_side_effects(
    queries: &[Query<'_, '_>],
    targets: &[&Target],
    initial: &[Vec<Edge>],
    index: usize,
    visited: &mut [bool],
    cc_side: &mut [BTreeSet<usize>],
    lang_side: &mut [HashMap<String, BTreeSet<usize>>],
) {
    const NO_SIDE_EFFECT: [&str; 6] = ["C", "CXX", "OBJC", "OBJCXX", "ASM", "CUDA"];
    if visited[index] {
        return;
    }
    visited[index] = true;
    if has_custom_command_side_effect(queries, targets[index]) {
        cc_side[index].insert(index);
    }
    for language in compile_languages(queries, targets[index]) {
        if !NO_SIDE_EFFECT.contains(&language.as_str()) && language != "HIP" {
            lang_side[index].entry(language).or_default().insert(index);
        }
    }
    for edge in &initial[index] {
        collect_side_effects(
            queries, targets, initial, edge.to, visited, cc_side, lang_side,
        );
        let transitive_cc: Vec<usize> = cc_side[edge.to].iter().copied().collect();
        cc_side[index].extend(transitive_cc);
        let transitive_lang: Vec<(String, Vec<usize>)> = lang_side[edge.to]
            .iter()
            .map(|(language, set)| (language.clone(), set.iter().copied().collect()))
            .collect();
        for (language, set) in transitive_lang {
            lang_side[index].entry(language).or_default().extend(set);
        }
    }
}
