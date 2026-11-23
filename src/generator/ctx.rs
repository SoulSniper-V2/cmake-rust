//! Generation context: read-only view of the configured interpreter state
//! plus memoization, standing in for cmGlobalGenerator/cmLocalGenerator
//! queries. Diagnostics are collected and reported by `gen::generate`.

use std::cell::RefCell;
use std::collections::{BTreeSet, HashMap};
use std::rc::Rc;

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

#[derive(Clone)]
pub struct GenCtx<'a> {
    pub interp: &'a Interp,
    /// The build configuration being evaluated (CMAKE_BUILD_TYPE for
    /// single-config Ninja; the iterated configuration for multi-config).
    pub config: String,
    /// `cmGlobalGenerator::IsMultiConfig` (Ninja Multi-Config).
    pub multi_config: bool,
    /// `CMAKE_CONFIGURATION_TYPES` (all generator configurations).
    pub configs: Vec<String>,
    /// `cmGlobalNinjaMultiGenerator::DefaultFileConfig`: the configuration
    /// of the impl file included by `build.ninja`.
    pub default_file_config: String,
    /// `CMAKE_CROSS_CONFIGS`.
    pub cross_configs: BTreeSet<String>,
    /// `CMAKE_DEFAULT_CONFIGS`.
    pub default_configs: BTreeSet<String>,
    pub errors: Rc<RefCell<Vec<Diagnostic>>>,
    /// Free-form memo for expensive per-target computations.
    pub memo: Rc<RefCell<HashMap<String, String>>>,
    /// Backtrace table referenced by generator-expression provenance ids.
    pub backtraces: Rc<RefCell<Vec<Vec<Frame>>>>,
    /// `cmMakefile::AddTargetObject`: object files named by
    /// `$<TARGET_OBJECTS:...>`, as (referencing directory, path) -> library.
    pub target_objects: Rc<RefCell<HashMap<(usize, String), String>>>,
    /// `cmGeneratorTarget::KindedSourcesMap`: computed sources per target
    /// and configuration.
    pub sources: Rc<RefCell<HashMap<String, Rc<Vec<super::target::SourceInfo>>>>>,
    /// Usage requirements per (target, property, config, language), as
    /// cmGeneratorTarget caches its evaluated property entries.
    pub usage: Rc<RefCell<HashMap<String, Rc<Vec<crate::genex::UsageItem>>>>>,
    /// `cmGlobalGenerator::TargetDependencies`, computed on first use.
    /// Keyed by depender target pointer; dependees are stored by key.
    /// Pointer-free so `GenCtx` stays covariant over `'a`.
    pub dep_graph: Rc<RefCell<Option<Rc<DepGraph>>>>,
}

/// Depender target pointer -> its direct dependees.
pub type DepGraph = HashMap<usize, Rc<Vec<super::link::TargetDepend>>>;

impl<'a> GenCtx<'a> {
    pub fn new(interp: &'a Interp) -> GenCtx<'a> {
        let multi_config = interp.ext.multi_config;
        let config = if multi_config {
            interp.ext.gen_default_file_config.clone()
        } else {
            interp
                .get_def("CMAKE_BUILD_TYPE")
                .unwrap_or_default()
                .to_owned()
        };
        let configs = if multi_config {
            interp.ext.gen_configs.clone()
        } else {
            vec![config.clone()]
        };
        GenCtx {
            interp,
            config,
            multi_config,
            configs,
            default_file_config: interp.ext.gen_default_file_config.clone(),
            cross_configs: interp.ext.gen_cross_configs.clone(),
            default_configs: interp.ext.gen_default_configs.clone(),
            errors: Rc::new(RefCell::new(Vec::new())),
            memo: Rc::new(RefCell::new(HashMap::new())),
            backtraces: Rc::new(RefCell::new(Vec::new())),
            target_objects: Rc::new(RefCell::new(HashMap::new())),
            sources: Rc::new(RefCell::new(HashMap::new())),
            usage: Rc::new(RefCell::new(HashMap::new())),
            dep_graph: Rc::default(),
        }
    }

    /// A view of this context that evaluates generator expressions for
    /// `config` (the multi-config generator's per-configuration passes).
    /// All shared state stays shared.
    pub fn for_config(&self, config: &str) -> GenCtx<'a> {
        let mut ctx = self.clone();
        ctx.config = config.to_owned();
        ctx
    }

    /// `cmGlobalNinjaGenerator::BuildAlias`: `name:config` for a nonempty
    /// config, otherwise `name`.
    pub fn build_alias(&self, alias: &str, config: &str) -> String {
        if !self.multi_config || config.is_empty() {
            alias.to_owned()
        } else {
            format!("{alias}:{config}")
        }
    }

    /// `cmGlobalNinjaGenerator::ConfigDirectory`: `/<config>` for a
    /// nonempty config, otherwise empty.
    pub fn config_directory(&self, config: &str) -> String {
        if !self.multi_config || config.is_empty() {
            String::new()
        } else {
            format!("/{config}")
        }
    }

    /// `cmGlobalNinjaGenerator::OrderDependsTargetForTarget`: the
    /// config-suffixed order-only phony name under Ninja Multi-Config.
    pub fn order_depends_name(&self, name: &str, config: &str) -> String {
        if self.multi_config && !config.is_empty() {
            format!(
                "cmake_object_order_depends_target_{name}_{}",
                config.to_uppercase()
            )
        } else {
            format!("cmake_object_order_depends_target_{name}")
        }
    }

    /// `cmTarget::IsPerConfig`: every target created by the project is
    /// per-config (`cmMakefile::AddNewTarget` passes `PerConfig::Yes`);
    /// only the generator's own global targets may not be, and those are
    /// modelled by `ninja::GlobalTarget::per_config`.
    pub fn is_per_config(&self, target: &Target) -> bool {
        target.kind != TargetKind::GlobalTarget
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
        super::target::directory_backtrace(self, dir)
    }

    /// `cmExperimental::HasSupportEnabled(*mf, feature)` during generation,
    /// issuing the once-per-value author warning in directory `makefile`.
    pub fn experimental_enabled(&self, makefile: usize, feature: &str) -> bool {
        let Some(option) = crate::commands::language::experimental_variable(feature) else {
            return false;
        };
        let value = self.var(makefile, option);
        let Some((enabled, warning)) =
            crate::commands::language::experimental_check(&self.interp.ext, feature, value)
        else {
            return false;
        };
        if let Some(warning) = warning {
            self.message(
                MessageType::AuthorWarning,
                self.dir_backtrace(makefile),
                warning,
            );
        }
        enabled
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

    /// `targets_in_order`, split where the global targets of each directory
    /// belong (`cmLocalGenerator::GetGeneratorTargets`).
    pub fn targets_around_globals(&self) -> (Vec<&'a Target>, Vec<&'a Target>) {
        let ext = &self.interp.ext;
        let order = &ext.target_order;
        let split = ext
            .generated_targets_start
            .unwrap_or(order.len())
            .min(order.len());
        let find = |names: &[String]| {
            names
                .iter()
                .filter_map(|name| ext.targets.get(name))
                .collect()
        };
        (find(&order[..split]), find(&order[split..]))
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
        self.genex_with_dag_bt(
            text,
            owner,
            head,
            property,
            property,
            (language, compile_usage),
            backtrace,
        )
    }

    /// `genex_with_dag_at` with an explicit DAG `root_property` on `head`:
    /// `EvaluateInterfaceProperty`/`AddInterfaceEntries` parent the owner's
    /// property checker under the consuming property's checker, so the DAG
    /// Top stays `(head, consuming_property)` and checks like
    /// `EvaluatingLinkExpression`/`EvaluatingLinkLibraries` see it.
    #[allow(clippy::too_many_arguments)] // mirrors the upstream signature
    pub fn genex_with_dag_bt(
        &self,
        text: &str,
        owner: &Target,
        head: &Target,
        property: &str,
        root_property: &str,
        (language, compile_usage): (Option<&str>, bool),
        backtrace: &[Frame],
    ) -> String {
        if !text.contains("$<") {
            return text.to_owned();
        }
        let owner_key = self.target_key(owner);
        let head_key = self.target_key(head);
        let link = matches!(
            root_property,
            "LINK_LIBRARIES"
                | "INTERFACE_LINK_LIBRARIES"
                | "INTERFACE_LINK_LIBRARIES_DIRECT"
                | "INTERFACE_LINK_LIBRARIES_DIRECT_EXCLUDE"
        ) || root_property.starts_with("IMPORTED_LINK_INTERFACE_LIBRARIES");
        // cmGeneratorTarget::ComputeLinkClosure second pass: link-library
        // expressions see the first pass's linker language.
        let second_pass_language = if language.is_none() && link {
            self.memo.borrow().get(&second_pass_key(&head_key)).cloned()
        } else {
            None
        };
        let language = language.or(second_pass_language.as_deref());
        let host_policy = super::genex_host::CtxHost::new(self, owner.makefile);
        let transitive = link
            && !root_property.starts_with("INTERFACE_LINK_LIBRARIES_DIRECT")
            && crate::genex::GenexHost::local_policy_status(&host_policy, "CMP0189")
                == crate::policy::PolicyStatus::New;
        let interface = root_property != "LINK_LIBRARIES";
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
            ..crate::genex::DagChecker::new(&head_key, root_property)
        };
        // EvaluateInterfaceProperty: the evaluated entry's checker sits under
        // the consuming checker, so DAG-Top checks see `root_property` on
        // `head`. When owner and property coincide with the root a single
        // checker suffices.
        let frame = crate::genex::DagChecker {
            parent: Some(&root),
            ..crate::genex::DagChecker::new(&owner_key, property)
        };
        let dag = if owner_key == head_key && property == root_property {
            &root
        } else {
            &frame
        };
        let compiled = crate::genex::parse(text);
        let mut ectx = crate::genex::EvalContext {
            config: &self.config,
            head_target: Some(&head_key),
            current_target: Some(&owner_key),
            language,
            evaluate_for_buildsystem: true,
            dag: Some(dag),
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
                    let frames = self.backtrace(error.backtrace);
                    let frames = if frames.is_empty() {
                        backtrace.to_vec()
                    } else {
                        frames
                    };
                    self.genex_error(frames, &error);
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
    /// `cmCustomCommandGenerator`'s split-config evaluation: `output_config`
    /// is where the evaluated paths will be consumed, `command_config` is
    /// the configuration of the ninja file holding the command.
    #[allow(clippy::too_many_arguments)]
    pub fn genex_command(
        &self,
        text: &str,
        dir: usize,
        target: Option<&str>,
        use_output_config: bool,
        output_config: &str,
        command_config: &str,
        backtrace: &[Frame],
        utilities: Option<&mut Vec<(String, bool)>>,
    ) -> String {
        if !text.contains("$<") {
            return text.to_owned();
        }
        let mut ectx = crate::genex::EvalContext {
            config: if use_output_config {
                output_config
            } else {
                command_config
            },
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
            output_config,
            command_config,
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

    /// Runs the rest of the generate step; returns whether
    /// `cmGlobalGenerator::Compute` succeeded and the diagnostics raised.
    /// The caller reports them and decides failure from their severity.
    pub fn check_and_generate(
        &self,
        autogen: &[super::autogen::Initializer],
    ) -> (bool, Vec<Diagnostic>) {
        if !super::flags::validate_compile_features(self) {
            return (false, self.errors.take());
        }
        // cmGlobalGenerator::Compute: source resolution diagnostics surface
        // during the pre-generation target computations, and
        // CheckTargetsForMissingSources stops generation before any build
        // files are written.
        if super::target::check_targets_for_missing_sources(self) {
            return (false, self.errors.take());
        }
        (true, self.generate(autogen))
    }

    /// `cmGlobalGenerator::Generate`.
    fn generate(&self, autogen: &[super::autogen::Initializer]) -> Vec<Diagnostic> {
        // cmGlobalGenerator::Generate: evaluation files first.
        super::evaluation_files::generate(self);
        // cmGlobalGenerator::Generate: QtAutoGen->SetupCustomTargets.
        if !super::autogen::write_info_files(self, autogen) {
            return self.errors.take();
        }
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
            // cmGlobalGenerator::Generate finishes by invoking the extra
            // (IDE project file) generator, if one was selected.
            super::extra::generate(self, ninja.global_targets());
        }
        // cmGlobalGenerator::Generate ends with CheckTargetLinkLibraries:
        // validation of the link structures memoized during generation.
        super::link::check_link_libraries(self);
        std::mem::take(&mut *self.errors.borrow_mut())
    }
}

/// The memo key under which `ComputeLinkClosure`'s second pass publishes
/// the first-pass linker language of the head target.
pub(super) fn second_pass_key(head_key: &str) -> String {
    format!("linker-language-second-pass\0{head_key}")
}
