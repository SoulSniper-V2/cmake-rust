//! `cmTargetTraceDependencies` (cmTargetTraceDependencies.cxx) and
//! `cmLocalGenerator::TraceDependencies`: before generation, follow each
//! target's sources and custom command dependencies to the sources that
//! carry the custom commands producing them, adding those to the target
//! (`AddTracedSources`) and recording target-level utilities.

use std::collections::{BTreeMap, BTreeSet, HashSet, VecDeque};

use super::ctx::{Caches, GenCtx};
use super::custom::make_generators;
use super::target as gt;
use crate::interp::Interp;
use crate::state::{CustomCommand, CustomCommandKind, TargetKind};
use crate::util::{collapse_full_path, expand_list, filename_name, filename_path, is_full_path};

/// What tracing found for one target.
#[derive(Default)]
struct Traced {
    sources: Vec<String>,
    utilities: Vec<(String, bool)>,
    codegen: BTreeSet<String>,
}

/// `cmSourcesWithOutput`.
#[derive(Clone, Default)]
struct SourcesWithOutput {
    target: Option<String>,
    /// Full path of the source carrying the producing custom command.
    source: Option<String>,
    source_is_byproduct: bool,
}

/// `cmLocalGenerator::TraceDependencies` for every local generator, in
/// order. Target sources and utilities are updated in place.
pub fn trace_dependencies(interp: &mut Interp) {
    let mut caches = Caches::default();
    for dir in 0..interp.makefiles.len() {
        let keys = interp.makefiles[dir].targets.clone();
        for key in keys {
            let traced = {
                let Some(target) = interp.ext.targets.get(&key) else {
                    continue;
                };
                // cmLocalGenerator::TraceDependencies: only targets in the
                // build system; CMake-generated targets have nothing to trace.
                if target.kind == TargetKind::GlobalTarget
                    || !super::ninja::is_in_build_system(target)
                {
                    continue;
                }
                let ctx = GenCtx::with_caches(interp, std::mem::take(&mut caches));
                let traced = Tracer::new(&ctx, &key).run();
                // Compute-phase diagnostics (cmLocalGenerator::IssueMessage,
                // genex eval errors) are reported as they happen — here, at
                // the end of this target's trace.
                let diagnostics = ctx.errors.take();
                if !traced.sources.is_empty() {
                    gt::clear_sources_cache(&ctx, target);
                }
                caches = ctx.into_caches();
                (traced, diagnostics)
            };
            let (traced, diagnostics) = traced;
            for diagnostic in diagnostics {
                match diagnostic {
                    super::ctx::Diagnostic::Message(kind, backtrace, message) => {
                        interp.issue_at(kind, &message, &backtrace);
                    }
                    super::ctx::Diagnostic::Bare(message) => {
                        interp.error = true;
                        interp.stderr_line(&format!("CMake Error: {message}"));
                    }
                }
            }
            apply(interp, &key, traced);
        }
    }
}

/// `cmGlobalGenerator::GetFilenameTargetDepends`: the targets whose
/// file(GENERATE) OUTPUT expressions depended on the source `path`.
fn interp_filename_depends_contains(ctx: &GenCtx, path: &str, key: &str) -> bool {
    ctx.interp
        .ext
        .filename_target_depends
        .get(path)
        .is_some_and(|targets| targets.contains(key))
}

fn apply(interp: &mut Interp, key: &str, traced: Traced) {
    let Some(target) = interp.ext.targets.get_mut(key) else {
        return;
    };
    if !traced.sources.is_empty() {
        // cmTarget::AddTracedSources: one appended SOURCES entry, recorded
        // under the makefile's persistent file-scope frame (the generate-time
        // DirectoryBacktrace).
        let makefile = &interp.makefiles[target.makefile];
        let file = makefile
            .list_files
            .first()
            .cloned()
            .unwrap_or_else(|| format!("{}/CMakeLists.txt", makefile.source_dir));
        let backtrace = [crate::messages::Frame {
            file: std::rc::Rc::from(file.as_str()),
            line: 0,
            name: None,
            deferred: false,
        }];
        let joined = traced.sources.join(";");
        target.insert_entry("SOURCES", &joined, &backtrace, false);
    }
    for utility in traced.utilities {
        if !target.traced_utilities.contains(&utility) {
            target.traced_utilities.push(utility);
        }
    }
    target.codegen_dependencies.extend(traced.codegen);
}

struct Tracer<'c, 'a> {
    ctx: &'c GenCtx<'a>,
    dir: usize,
    queue: VecDeque<String>,
    queued: HashSet<String>,
    name_map: BTreeMap<String, SourcesWithOutput>,
    traced: Traced,
}

impl<'c, 'a> Tracer<'c, 'a> {
    fn new(ctx: &'c GenCtx<'a>, key: &str) -> Self {
        let target = &ctx.interp.ext.targets[key];
        let mut tracer = Tracer {
            ctx,
            dir: target.makefile,
            queue: VecDeque::new(),
            queued: HashSet::new(),
            name_map: BTreeMap::new(),
            traced: Traced::default(),
        };
        // Queue all the source files already specified for the target.
        let mut emitted = HashSet::new();
        // cmGeneratorTarget::GetAllConfigSources.
        let mut sources = Vec::new();
        if ctx.multi_config {
            for config in &ctx.configs {
                sources.extend(
                    gt::sources(&ctx.for_config(config), target, config)
                        .iter()
                        .map(|source| source.path.clone()),
                );
            }
        } else {
            sources.extend(
                gt::sources(ctx, target, &ctx.config)
                    .iter()
                    .map(|source| source.path.clone()),
            );
        }
        for path in sources {
            // cmTargetTraceDependencies: a file(GENERATE) output whose
            // OUTPUT expression depends on this target's sources must not
            // also be a source of the target.
            if interp_filename_depends_contains(ctx, &path, key) {
                ctx.error(
                    ctx.dir_backtrace(target.makefile),
                    format!(
                        "Evaluation output file\n  \"{}\"\ndepends on the sources of a target it is used in.  This is a dependency loop and is not allowed.",
                        path
                    ),
                );
                return tracer;
            }
            if emitted.insert(path.clone()) && tracer.queued.insert(path.clone()) {
                tracer.queue.push_back(path.clone());
            }
        }
        // Queue pre-build, pre-link, and post-build rule dependencies.
        for stage in [
            CustomCommandKind::PreBuild,
            CustomCommandKind::PreLink,
            CustomCommandKind::PostBuild,
        ] {
            for command in target
                .custom_commands
                .iter()
                .filter(|command| command.kind == stage)
            {
                tracer.check_custom_command(command);
            }
        }
        tracer
    }

    fn run(mut self) -> Traced {
        let interp = self.ctx.interp;
        while let Some(path) = self.queue.pop_front() {
            let source = crate::commands::target::find_known_source(interp, self.dir, &path)
                .map(|index| &interp.ext.sources[&self.dir][index]);
            // Queue dependencies added explicitly by the user.
            if let Some(depends) = source.and_then(|source| source.props.get("OBJECT_DEPENDS")) {
                let names: Vec<String> = expand_list(depends, false)
                    .into_iter()
                    .map(|dep| {
                        if is_full_path(&dep) {
                            collapse_full_path(&dep, "")
                        } else {
                            dep
                        }
                    })
                    .collect();
                for name in names {
                    self.follow_name(&name);
                }
            }
            // Queue the source needed to generate this file, if any.
            self.follow_name(&path);
            // Queue custom command dependencies.
            if let Some(command) = source.and_then(|source| source.custom_command.as_ref()) {
                self.check_custom_command(command);
            }
        }
        self.traced
    }

    fn queue_source(&mut self, path: String) {
        if self.queued.insert(path.clone()) {
            self.queue.push_back(path.clone());
            // Make sure this file is in the target at the end.
            self.traced.sources.push(path);
        }
    }

    fn follow_name(&mut self, name: &str) {
        let sources = match self.name_map.get(name) {
            Some(found) => found.clone(),
            None => {
                let mut sources = sources_with_output(self.ctx.interp, self.dir, name);
                if sources.target.is_none() && sources.source.is_none() && !is_full_path(name) {
                    let binary = &self.ctx.interp.makefiles[self.dir].binary_dir;
                    let full = collapse_full_path(
                        &format!("{binary}/{name}"),
                        &self.ctx.interp.home_binary,
                    );
                    sources = sources_with_output(self.ctx.interp, self.dir, &full);
                }
                self.name_map.insert(name.to_owned(), sources.clone());
                sources
            }
        };
        if let Some(target) = sources.target {
            // A byproduct of a utility target or a build-event command.
            self.add_utility(target.clone(), false);
            self.traced.codegen.insert(target);
        }
        if let Some(source) = sources.source
            && !sources.source_is_byproduct
        {
            self.queue_source(source);
        }
    }

    fn add_utility(&mut self, name: String, cross: bool) {
        let utility = (name, cross);
        if !self.traced.utilities.contains(&utility) {
            self.traced.utilities.push(utility);
        }
    }

    /// `cmTargetTraceDependencies::IsUtility`.
    fn is_utility(&mut self, dep: &str) -> bool {
        let mut util = filename_name(dep);
        if let Some(stem) = util.strip_suffix(".exe") {
            util = stem.to_owned();
        }
        let Some(found) = self.ctx.target_at(self.dir, &util) else {
            return false;
        };
        if is_full_path(dep) {
            if matches!(
                found.kind,
                TargetKind::Executable
                    | TargetKind::StaticLibrary
                    | TargetKind::SharedLibrary
                    | TargetKind::ModuleLibrary
            ) {
                // GetLocationForBuild: without a configuration.
                let location = if found.imported {
                    gt::imported_full_path(self.ctx, found, "", gt::ArtifactType::Runtime)
                } else {
                    gt::full_path(
                        &self.ctx.for_config(""),
                        found,
                        "",
                        gt::ArtifactType::Runtime,
                        false,
                    )
                };
                let target_dir = collapse_full_path(&filename_path(&location), "");
                let dep_dir = collapse_full_path(&filename_path(dep), "");
                if dep_dir == target_dir {
                    self.add_utility(util, false);
                    return true;
                }
            }
            false
        } else {
            self.add_utility(util, true);
            true
        }
    }

    /// `cmTargetTraceDependencies::CheckCustomCommand`.
    fn check_custom_command(&mut self, command: &CustomCommand) {
        // Collect file-level dependencies referenced in DEPENDS (std::set).
        let mut depends: BTreeSet<String> = BTreeSet::new();
        for config in &self.ctx.configs.clone() {
            for (_, evaluated) in make_generators(self.ctx, command, config) {
                for (name, cross) in &evaluated.utilities {
                    self.add_utility(name.clone(), *cross);
                    if command.codegen {
                        self.traced.codegen.insert(name.clone());
                    }
                }
                depends.extend(evaluated.raw_depends);
            }
        }
        for dep in depends {
            if !self.is_utility(&dep) {
                self.follow_name(&dep);
            } else {
                self.traced.codegen.insert(dep);
            }
        }
    }
}

/// `cmLocalGenerator::GetSourcesWithOutput`.
fn sources_with_output(interp: &Interp, dir: usize, name: &str) -> SourcesWithOutput {
    let source_path = |index: usize| -> String {
        let source = &interp.ext.sources[&dir][index];
        source.location.path()
    };
    if !is_full_path(name) {
        // LinearGetTargetWithOutput / LinearGetSourceFileWithOutput.
        let mut result = SourcesWithOutput {
            target: linear_target_with_output(interp, dir, name),
            ..SourcesWithOutput::default()
        };
        if let Some(sources) = interp.ext.sources.get(&dir) {
            let mut fallback = None;
            for (index, source) in sources.iter().enumerate() {
                let Some(command) = &source.custom_command else {
                    continue;
                };
                if any_output_matches(name, &command.outputs) {
                    result.source = Some(source_path(index));
                    result.source_is_byproduct = false;
                    fallback = None;
                    break;
                }
                if fallback.is_none() && any_output_matches(name, &command.byproducts) {
                    fallback = Some(index);
                }
            }
            if result.source.is_none()
                && let Some(index) = fallback
            {
                result.source = Some(source_path(index));
                result.source_is_byproduct = true;
            }
        }
        return result;
    }
    let Some(entry) = interp
        .ext
        .output_to_source
        .get(&dir)
        .and_then(|map| map.get(name))
    else {
        return SourcesWithOutput::default();
    };
    SourcesWithOutput {
        target: entry
            .target
            .as_ref()
            .and_then(|key| interp.ext.targets.get(key))
            .map(|target| target.name.clone()),
        source: entry.source.map(source_path),
        source_is_byproduct: entry.source_is_byproduct,
    }
}

/// `cmLocalGenerator::LinearGetTargetWithOutput`.
fn linear_target_with_output(interp: &Interp, dir: usize, name: &str) -> Option<String> {
    // Search for a target that has the name as a byproduct of a build
    // event or utility command.
    for key in &interp.makefiles[dir].targets {
        let Some(target) = interp.ext.targets.get(key) else {
            continue;
        };
        if target
            .custom_commands
            .iter()
            .any(|command| any_output_matches(name, &command.byproducts))
        {
            return Some(target.name.clone());
        }
    }
    None
}

/// `AnyOutputMatches`.
fn any_output_matches(name: &str, outputs: &[String]) -> bool {
    outputs.iter().any(|output| {
        output.ends_with(name) && {
            let pos = output.len() - name.len();
            pos == 0 || output.as_bytes()[pos - 1] == b'/'
        }
    })
}
