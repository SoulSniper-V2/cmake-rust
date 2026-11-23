//! Generation context: read-only view of the configured interpreter state
//! plus memoization, standing in for cmGlobalGenerator/cmLocalGenerator
//! queries. Diagnostics are collected and reported by `gen::generate`.

use std::cell::RefCell;
use std::collections::{BTreeSet, HashMap};

use crate::interp::Interp;
use crate::messages::{Frame, MessageType};
use crate::state::{Target, TargetKind};

/// One diagnostic produced during generation, in the order it was raised.
#[derive(Clone, Debug)]
pub enum Diagnostic {
    /// `cmake::IssueMessage`: rendered with location and call stack.
    Message(MessageType, Vec<Frame>, String),
    /// `cmSystemTools::Error`: a bare `CMake Error: <text>` line.
    Bare(String),
}

/// What an expression evaluation collected besides its value.
#[derive(Default)]
pub struct GenexOutcome {
    pub value: String,
    /// `cmGeneratorExpression::GetSourceSensitiveTargets` of the evaluated
    /// expression (target names as `ext.targets` keys).
    pub source_sensitive_targets: BTreeSet<String>,
}

impl Diagnostic {
    pub fn text(&self) -> &str {
        match self {
            Diagnostic::Message(_, _, text) | Diagnostic::Bare(text) => text,
        }
    }

    /// Whether the diagnostic sets the error flag at this generation run
    /// (dev warnings become errors under --Werror=dev).
    pub fn is_error(&self, dev_warnings_as_errors: bool) -> bool {
        match self {
            Diagnostic::Bare(_) => true,
            Diagnostic::Message(kind, _, _) => {
                crate::messages::is_error(*kind)
                    || (dev_warnings_as_errors
                        && matches!(*kind, MessageType::AuthorWarning | MessageType::AuthorError))
            }
        }
    }
}

pub struct GenCtx<'a> {
    pub interp: &'a Interp,
    /// The build configuration (CMAKE_BUILD_TYPE) — single-config Ninja.
    pub config: String,
    pub errors: RefCell<Vec<Diagnostic>>,
    /// Free-form memo for expensive per-target computations.
    pub memo: RefCell<HashMap<String, String>>,
    /// Backtrace table referenced by generator-expression provenance ids.
    pub backtraces: RefCell<Vec<Vec<Frame>>>,
    /// `cmMakefile::AddTargetObject`: object files named by
    /// `$<TARGET_OBJECTS:...>`, as (referencing directory, path) -> library.
    pub target_objects: RefCell<HashMap<(usize, String), String>>,
    /// `cmGeneratorTarget::KindedSourcesMap`: computed sources per target
    /// and configuration.
    pub sources: RefCell<HashMap<String, std::rc::Rc<Vec<super::target::SourceInfo>>>>,
    /// Usage requirements per (target, property, config, language), as
    /// cmGeneratorTarget caches its evaluated property entries.
    pub usage: RefCell<HashMap<String, std::rc::Rc<Vec<crate::genex::UsageItem>>>>,
    /// `cmComputeTargetDepends` final edges with backtraces (File API
    /// codemodel target dependencies), computed once per configuration.
    /// Keyed by depender target pointer; dependees are stored by name.
    /// Pointer-free so `GenCtx` stays covariant over `'a`.
    pub dep_graph: RefCell<Option<std::rc::Rc<DepGraphEdges>>>,
}

/// Depender target pointer -> (dependee name, edge backtrace).
pub type DepGraphEdges = HashMap<usize, Vec<(String, std::rc::Rc<Vec<Frame>>)>>;

impl<'a> GenCtx<'a> {
    pub fn new(interp: &'a Interp) -> GenCtx<'a> {
        let config = interp
            .get_def("CMAKE_BUILD_TYPE")
            .unwrap_or_default()
            .to_owned();
        GenCtx {
            interp,
            config,
            errors: RefCell::new(Vec::new()),
            memo: RefCell::new(HashMap::new()),
            backtraces: RefCell::new(Vec::new()),
            target_objects: RefCell::new(HashMap::new()),
            sources: RefCell::new(HashMap::new()),
            usage: RefCell::new(HashMap::new()),
            dep_graph: RefCell::new(None),
        }
    }

    /// An AUTHOR_WARNING issued during generation (cmake::IssueMessage).
    pub fn warning(&self, backtrace: Vec<Frame>, message: impl Into<String>) {
        let message = message.into();
        if self.interp.dev_warnings_as_errors {
            self.error(backtrace, message);
            return;
        }
        if self.interp.suppress_dev_warnings {
            return;
        }
        eprintln!(
            "{}",
            crate::messages::render(
                crate::messages::MessageType::AuthorWarning,
                &message,
                &backtrace,
                Some(&self.interp.home_source),
            )
        );
    }

    /// Reports a generator-expression error at `backtrace`, followed by
    /// the "Loop step N" messages of a dependency loop
    /// (cmGeneratorExpressionDAGChecker::ReportError), each at its
    /// checker's backtrace.
    pub fn genex_error(&self, backtrace: Vec<Frame>, error: &crate::genex::GenexError) {
        self.error(backtrace, error.to_string());
        for (text, step_backtrace) in &error.loop_steps {
            self.error(self.backtrace(*step_backtrace), text.clone());
        }
    }

    pub fn error(&self, backtrace: Vec<Frame>, message: impl Into<String>) {
        self.errors.borrow_mut().push(Diagnostic::Message(
            MessageType::FatalError,
            backtrace,
            message.into(),
        ));
    }

    /// A WARNING or AUTHOR_WARNING issued during generation
    /// (`cmake::IssueMessage`), e.g. a generator-expression eval warning.
    pub fn message(&self, kind: MessageType, backtrace: Vec<Frame>, message: impl Into<String>) {
        self.errors
            .borrow_mut()
            .push(Diagnostic::Message(kind, backtrace, message.into()));
    }

    /// `cmSystemTools::Error`: a bare `CMake Error:` diagnostic.
    pub fn system_error(&self, message: impl Into<String>) {
        self.errors
            .borrow_mut()
            .push(Diagnostic::Bare(message.into()));
    }

    /// The cmNinjaNormalTargetGenerator::Generate linker-language check,
    /// which reports once per target (it is also made ahead of the build
    /// export files, which cmGlobalGenerator::Generate writes after every
    /// local generator ran).
    pub fn report_unknown_linker_language(&self, target: &Target) -> bool {
        if super::target::linker_language(self, target, &self.config).is_empty() {
            let message = format!(
                "CMake can not determine linker language for target: {}",
                target.name
            );
            let known = self
                .errors
                .borrow()
                .iter()
                .any(|existing| matches!(existing, Diagnostic::Bare(text) if *text == message));
            if !known {
                self.system_error(message);
            }
            return true;
        }
        false
    }

    /// `cmLocalGenerator::IssueMessage`'s backtrace: the directory scope of
    /// `dir`'s list file, rendered as `in <dir>/CMakeLists.txt`.
    pub fn dir_backtrace(&self, dir: usize) -> Vec<Frame> {
        let source = &self.interp.makefiles[dir].source_dir;
        vec![Frame {
            file: format!("{source}/CMakeLists.txt").into(),
            line: 0,
            name: None,
            deferred: false,
        }]
    }

    /// True when any collected diagnostic sets the error flag.
    pub fn has_error(&self) -> bool {
        let dev = self.interp.dev_warnings_as_errors;
        self.errors.borrow().iter().any(|d| d.is_error(dev))
    }

    /// Variable lookup in the context of directory `makefile`'s final
    /// state. NOTE: variables are currently global to the interpreter's
    /// root scope after configure; per-directory final scopes are recorded
    /// by the configure step in `dir_vars` when available.
    pub fn var(&self, makefile: usize, name: &str) -> Option<String> {
        self.interp.mark_cli_used(name);
        if let Some(vars) = self.interp.dir_final_vars.get(makefile)
            && let Some(Some(value)) = vars.get(name)
        {
            return Some(value.clone());
        }
        self.interp.cache.value(name).map(str::to_owned)
    }

    pub fn var_safe(&self, makefile: usize, name: &str) -> String {
        self.var(makefile, name).unwrap_or_default()
    }

    pub fn targets_in_order(&self) -> Vec<&'a Target> {
        self.interp
            .ext
            .target_order
            .iter()
            .filter_map(|name| self.interp.ext.targets.get(name))
            .collect()
    }

    pub fn target(&self, name: &str) -> Option<&'a Target> {
        let real = self
            .interp
            .ext
            .aliases
            .get(name)
            .map(String::as_str)
            .unwrap_or(name);
        self.interp
            .ext
            .targets
            .get(real)
            .or_else(|| self.target_at(0, name))
    }

    /// Target lookup as seen from directory `dir` (imported targets are
    /// directory-scoped; `cmLocalGenerator::FindGeneratorTargetToUse`).
    pub fn target_at(&self, dir: usize, name: &str) -> Option<&'a Target> {
        let mut current = Some(dir);
        while let Some(index) = current {
            if let Some(key) = crate::state::find_target_key_at(self.interp, index, name) {
                return self.interp.ext.targets.get(&key);
            }
            current = self.interp.makefiles.get(index).and_then(|mf| mf.parent);
        }
        None
    }

    pub fn intern_backtrace(&self, backtrace: Vec<Frame>) -> usize {
        let mut table = self.backtraces.borrow_mut();
        table.push(backtrace);
        table.len() - 1
    }

    pub fn backtrace(&self, id: Option<usize>) -> Vec<Frame> {
        id.and_then(|id| self.backtraces.borrow().get(id).cloned())
            .unwrap_or_default()
    }

    /// Evaluates generator expressions in `text` for `target` (head and
    /// current), the build configuration and `language`
    /// (`cmGeneratorExpressionInterpreter`). Diagnostics are recorded.
    pub fn genex(&self, text: &str, target: Option<&Target>, language: Option<&str>) -> String {
        let dir = target.map_or(0, |target| target.makefile);
        let key = target.map(|target| self.target_key(target));
        self.genex_full(text, dir, key.as_deref(), language, true, &[])
    }

    /// The `ext.targets` key of `target`.
    pub fn target_key(&self, target: &Target) -> String {
        if self
            .interp
            .ext
            .targets
            .get(&target.name)
            .is_some_and(|found| std::ptr::eq(found, target))
        {
            return target.name.clone();
        }
        self.interp
            .ext
            .targets
            .iter()
            .find(|(_, found)| std::ptr::eq(*found, target))
            .map(|(key, _)| key.clone())
            .unwrap_or_else(|| target.name.clone())
    }

    /// `cmCompiledGeneratorExpression::Evaluate`: evaluates `text` in `dir`
    /// for `target`, `config` and `language`. `backtrace` is the list-file
    /// backtrace of the command the expression came from — the equivalent
    /// of the backtrace a compiled generator expression carries — and is
    /// used by `context->Backtrace` for every diagnostic it raises.
    pub fn genex_eval(
        &self,
        text: &str,
        dir: usize,
        target: Option<&str>,
        language: Option<&str>,
        for_buildsystem: bool,
        backtrace: &[Frame],
    ) -> GenexOutcome {
        if !text.contains("$<") {
            return GenexOutcome {
                value: text.to_owned(),
                ..GenexOutcome::default()
            };
        }
        let compiled = crate::genex::parse(text);
        let mut ectx = crate::genex::EvalContext {
            config: &self.config,
            head_target: target,
            current_target: target,
            language,
            evaluate_for_buildsystem: for_buildsystem,
            backtrace: (!backtrace.is_empty()).then(|| self.intern_backtrace(backtrace.to_vec())),
            ..crate::genex::EvalContext::default()
        };
        let mut host = super::genex_host::CtxHost::new(self, dir);
        let result = crate::genex::evaluate(&compiled, &mut ectx, &mut host);
        let outcome = GenexOutcome {
            value: match result {
                Ok(value) => value,
                Err(error) => {
                    if !error.quiet {
                        let frames = self.backtrace(error.backtrace);
                        self.genex_error(
                            if frames.is_empty() {
                                backtrace.to_vec()
                            } else {
                                frames
                            },
                            &error,
                        );
                    }
                    String::new()
                }
            },
            source_sensitive_targets: ectx.source_sensitive_targets.clone(),
        };
        self.report_warnings(&ectx);
        outcome
    }

    /// `IssueMessage` for diagnostics raised during an evaluation — like
    /// `cmGeneratorExpressionContext`'s WARNING/AUTHOR_WARNING paths — with
    /// `context->Backtrace` as captured when each was raised.
    fn report_warnings(&self, ectx: &crate::genex::EvalContext) {
        for warning in &ectx.warnings {
            let kind = if warning.dev {
                MessageType::AuthorWarning
            } else {
                MessageType::Warning
            };
            self.message(
                kind,
                self.backtrace(warning.backtrace),
                warning.text.clone(),
            );
        }
    }

    pub fn genex_full(
        &self,
        text: &str,
        dir: usize,
        target: Option<&str>,
        language: Option<&str>,
        for_buildsystem: bool,
        backtrace: &[Frame],
    ) -> String {
        self.genex_eval(text, dir, target, language, for_buildsystem, backtrace)
            .value
    }

    /// Evaluates `text` from `owner`'s `property` for consumer `head` under
    /// a cmGeneratorExpressionDAGChecker rooted at (owner, property), as
    /// cmGeneratorTarget does for link implementation/interface items and
    /// other per-target properties.
    pub fn genex_with_dag(
        &self,
        text: &str,
        owner: &Target,
        head: &Target,
        property: &str,
        language: Option<&str>,
    ) -> String {
        self.genex_with_dag_usage((text, &[]), owner, head, property, language, false)
    }

    /// `genex_with_dag` for `UseTo::Compile` link evaluation: the DAG
    /// checker evaluates transitive properties only, so `$<LINK_ONLY:...>`
    /// is dropped from link interfaces (cmGeneratorTarget::ExpandLinkItems).
    pub fn genex_with_dag_usage(
        &self,
        (text, backtrace): (&str, &[Frame]),
        owner: &Target,
        head: &Target,
        property: &str,
        language: Option<&str>,
        compile_usage: bool,
    ) -> String {
        self.genex_with_dag_at(
            text,
            owner,
            head,
            property,
            (language, compile_usage),
            backtrace,
        )
    }

    pub fn genex_with_dag_at(
        &self,
        text: &str,
        owner: &Target,
        head: &Target,
        property: &str,
        (language, compile_usage): (Option<&str>, bool),
        backtrace: &[Frame],
    ) -> String {
        if !text.contains("$<") {
            return text.to_owned();
        }
        let owner_key = self.target_key(owner);
        let head_key = self.target_key(head);
        let link = matches!(
            property,
            "LINK_LIBRARIES"
                | "INTERFACE_LINK_LIBRARIES"
                | "INTERFACE_LINK_LIBRARIES_DIRECT"
                | "INTERFACE_LINK_LIBRARIES_DIRECT_EXCLUDE"
        ) || property.starts_with("IMPORTED_LINK_INTERFACE_LIBRARIES");
        let host_policy = super::genex_host::CtxHost::new(self, owner.makefile);
        let transitive = link
            && !property.starts_with("INTERFACE_LINK_LIBRARIES_DIRECT")
            && crate::genex::GenexHost::local_policy_status(&host_policy, "CMP0189")
                == crate::policy::PolicyStatus::New;
        let interface = property != "LINK_LIBRARIES";
        let root = crate::genex::DagChecker {
            computing_link_libraries: link,
            transitive,
            transitive_properties_only: compile_usage,
            transitive_properties_only_cmp0131: compile_usage
                && (interface
                    || crate::generator::target::target_policy(
                        self,
                        owner,
                        crate::policy::index_of("CMP0131").unwrap_or_default(),
                    ) == crate::policy::PolicyStatus::New),
            ..crate::genex::DagChecker::new(&owner_key, property)
        };
        let compiled = crate::genex::parse(text);
        let mut ectx = crate::genex::EvalContext {
            config: &self.config,
            head_target: Some(&head_key),
            current_target: Some(&owner_key),
            language,
            evaluate_for_buildsystem: true,
            dag: Some(&root),
            backtrace: (!backtrace.is_empty()).then(|| self.intern_backtrace(backtrace.to_vec())),
            ..crate::genex::EvalContext::default()
        };
        let mut host = super::genex_host::CtxHost::new(self, owner.makefile);
        let result = crate::genex::evaluate(&compiled, &mut ectx, &mut host);
        if property == "LINK_LIBRARIES" && std::ptr::eq(owner, head) {
            self.memo.borrow_mut().extend(
                ectx.seen_target_properties
                    .iter()
                    .filter(|prop| !head.props.contains(prop))
                    .map(|prop| {
                        (
                            super::compatible::implied_key(self, head, prop),
                            String::new(),
                        )
                    }),
            );
        }
        self.report_warnings(&ectx);
        match result {
            Ok(value) => value,
            Err(error) => {
                if !error.quiet {
                    let backtrace = self.backtrace(error.backtrace);
                    self.genex_error(backtrace, &error);
                }
                String::new()
            }
        }
    }

    /// `cmCompiledGeneratorExpression::Evaluate` as used by
    /// `cmLocalGenerator::ExpandCustomCommandOutputPaths`: a plain
    /// evaluation (OUTPUT_CONFIG/COMMAND_CONFIG have no special meaning
    /// here) whose diagnostics fall back to the command's `backtrace`.
    pub fn genex_output(&self, text: &str, dir: usize, backtrace: &[Frame]) -> String {
        if !text.contains("$<") {
            return text.to_owned();
        }
        let compiled = crate::genex::parse(text);
        let mut ectx = crate::genex::EvalContext {
            config: &self.config,
            ..crate::genex::EvalContext::default()
        };
        let mut host = super::genex_host::CtxHost::new(self, dir);
        match crate::genex::evaluate(&compiled, &mut ectx, &mut host) {
            Ok(value) => value,
            Err(error) => {
                if !error.quiet {
                    let frames = error
                        .backtrace
                        .map(|id| self.backtrace(Some(id)))
                        .filter(|frames| !frames.is_empty())
                        .unwrap_or_else(|| backtrace.to_vec());
                    self.genex_error(frames, &error);
                }
                String::new()
            }
        }
    }

    /// `EvaluateSplitConfigGenex` of cmCustomCommandGenerator.cxx (single
    /// config): evaluates `text` as `cge->Evaluate(lg, config, target)` per
    /// top-level expression and records the targets the expressions depend
    /// on (`cge->GetTargets()`) with their cross flag into `utilities`.
    pub fn genex_command(
        &self,
        text: &str,
        dir: usize,
        target: Option<&str>,
        use_output_config: bool,
        backtrace: &[Frame],
        utilities: Option<&mut Vec<(String, bool)>>,
    ) -> String {
        if !text.contains("$<") {
            return text.to_owned();
        }
        let mut ectx = crate::genex::EvalContext {
            config: &self.config,
            head_target: target,
            current_target: target,
            // cmCompiledGeneratorExpression: the backtrace of the command
            // that created the expression travels with it, so diagnostics
            // at generate time cite the originating add_custom_command,
            // add_custom_target, or test.
            backtrace: (!backtrace.is_empty()).then(|| self.intern_backtrace(backtrace.to_vec())),
            ..crate::genex::EvalContext::default()
        };
        let mut host = super::genex_host::CtxHost::new(self, dir);
        let mut dependencies = std::collections::BTreeSet::new();
        let result = crate::genex::evaluate_custom_command(
            text,
            &mut ectx,
            &mut host,
            &self.config,
            &self.config,
            use_output_config,
            &mut dependencies,
        );
        self.report_warnings(&ectx);
        if let Some(utilities) = utilities {
            for dependency in dependencies {
                let name = self
                    .target_at(dir, &dependency.target)
                    .map_or(dependency.target.clone(), |target| target.name.clone());
                if !utilities
                    .iter()
                    .any(|(existing, cross)| *existing == name && *cross == dependency.cross)
                {
                    utilities.push((name, dependency.cross));
                }
            }
        }
        match result {
            Ok(value) => value,
            Err(error) => {
                if !error.quiet {
                    let frames = error
                        .backtrace
                        .map(|id| self.backtrace(Some(id)))
                        .filter(|frames| !frames.is_empty())
                        .unwrap_or_else(|| backtrace.to_vec());
                    self.genex_error(frames, &error);
                }
                String::new()
            }
        }
    }

    /// Runs the whole generate step; returns the diagnostics it raised.
    /// The caller reports them and decides failure from their severity.
    pub fn check_and_generate(&self) -> Vec<Diagnostic> {
        // cmGlobalGenerator::Compute: cmQtAutoGenGlobalInitializer precedes
        // ComputeTargetCompileFeatures.
        super::autogen::initialize(self);
        if self.has_error() {
            return self.errors.take();
        }
        if !super::flags::validate_compile_features(self) {
            return self.errors.take();
        }
        // cmGlobalGenerator::Compute: source resolution diagnostics surface
        // during the pre-generation target computations, and
        // CheckTargetsForMissingSources stops generation before any build
        // files are written.
        if super::target::check_targets_for_missing_sources(self) {
            return std::mem::take(&mut *self.errors.borrow_mut());
        }
        // cmGlobalGenerator::Generate: evaluation files first.
        super::evaluation_files::generate(self);
        let failed_before_targets = self.has_error();
        // The local generators' target generation (in directory order)
        // reports targets without a linker language before the build
        // export sets are written.
        for dir in 0..self.interp.makefiles.len() {
            for target in self.targets_in_order() {
                if target.makefile == dir
                    && !target.imported
                    && target.alias_of.is_none()
                    && matches!(
                        target.kind,
                        TargetKind::Executable
                            | TargetKind::StaticLibrary
                            | TargetKind::SharedLibrary
                            | TargetKind::ModuleLibrary
                            | TargetKind::ObjectLibrary
                    )
                {
                    self.report_unknown_linker_language(target);
                }
            }
        }
        // cmGlobalGenerator::Generate order: per-directory install rules and
        // test files, then build export sets, and (back in
        // cmGlobalNinjaGenerator::Generate) the ninja files last.
        for result in [
            super::install::write_install_scripts(self),
            super::tests::write_test_files(self),
            super::export::write_build_exports(self),
        ] {
            if let Err(error) = result {
                self.error(Vec::new(), format!("Unable to write build files: {error}"));
            }
        }
        if !failed_before_targets {
            // cmGlobalNinjaGenerator::Generate: bail before writing build
            // files when earlier generation raised a fatal error (target
            // generation itself precedes the install, test and export files).
            let mut ninja = super::ninja::NinjaGenerator::new(self);
            if let Err(error) = ninja.generate() {
                self.error(Vec::new(), format!("Unable to write build files: {error}"));
            }
        }
        std::mem::take(&mut *self.errors.borrow_mut())
    }
}
