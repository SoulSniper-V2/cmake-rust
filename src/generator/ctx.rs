//! Generation context: read-only view of the configured interpreter state
//! plus memoization, standing in for cmGlobalGenerator/cmLocalGenerator
//! queries. Diagnostics are collected and reported by `gen::generate`.

use std::cell::RefCell;
use std::collections::HashMap;

use crate::interp::Interp;
use crate::messages::Frame;
use crate::state::Target;

pub type Diagnostic = (Vec<Frame>, String);

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
}

impl<'a> GenCtx<'a> {
    pub fn new(interp: &'a Interp) -> GenCtx<'a> {
        let config = interp.get_def("CMAKE_BUILD_TYPE").unwrap_or_default().to_owned();
        GenCtx {
            interp,
            config,
            errors: RefCell::new(Vec::new()),
            memo: RefCell::new(HashMap::new()),
            backtraces: RefCell::new(Vec::new()),
            target_objects: RefCell::new(HashMap::new()),
            sources: RefCell::new(HashMap::new()),
            usage: RefCell::new(HashMap::new()),
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
        eprintln!("{}", crate::messages::render(
            crate::messages::MessageType::AuthorWarning,
            &message,
            &backtrace,
            Some(&self.interp.home_source),
        ));
    }

    pub fn error(&self, backtrace: Vec<Frame>, message: impl Into<String>) {
        self.errors.borrow_mut().push((backtrace, message.into()));
    }

    /// Variable lookup in the context of directory `makefile`'s final
    /// state. NOTE: variables are currently global to the interpreter's
    /// root scope after configure; per-directory final scopes are recorded
    /// by the configure step in `dir_vars` when available.
    pub fn var(&self, makefile: usize, name: &str) -> Option<String> {
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
        let real = self.interp.ext.aliases.get(name).map(String::as_str).unwrap_or(name);
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
        id.and_then(|id| self.backtraces.borrow().get(id).cloned()).unwrap_or_default()
    }

    /// Evaluates generator expressions in `text` for `target` (head and
    /// current), the build configuration and `language`
    /// (`cmGeneratorExpressionInterpreter`). Diagnostics are recorded.
    pub fn genex(&self, text: &str, target: Option<&Target>, language: Option<&str>) -> String {
        let dir = target.map_or(0, |target| target.makefile);
        let key = target.map(|target| self.target_key(target));
        self.genex_full(text, dir, key.as_deref(), language, true)
    }

    /// The `ext.targets` key of `target`.
    pub fn target_key(&self, target: &Target) -> String {
        if self.interp.ext.targets.get(&target.name).is_some_and(|found| std::ptr::eq(found, target)) {
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

    pub fn genex_full(
        &self,
        text: &str,
        dir: usize,
        target: Option<&str>,
        language: Option<&str>,
        for_buildsystem: bool,
    ) -> String {
        if !text.contains("$<") {
            return text.to_owned();
        }
        let compiled = crate::genex::parse(text);
        let mut ectx = crate::genex::EvalContext {
            config: &self.config,
            head_target: target,
            current_target: target,
            language,
            evaluate_for_buildsystem: for_buildsystem,
            ..crate::genex::EvalContext::default()
        };
        let mut host = super::genex_host::CtxHost::new(self, dir);
        match crate::genex::evaluate(&compiled, &mut ectx, &mut host) {
            Ok(value) => value,
            Err(error) => {
                if !error.quiet {
                    let backtrace = self.backtrace(error.backtrace);
                    self.error(backtrace, error.to_string());
                }
                String::new()
            }
        }
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
        let root = crate::genex::DagChecker {
            computing_link_libraries: link,
            transitive,
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
            ..crate::genex::EvalContext::default()
        };
        let mut host = super::genex_host::CtxHost::new(self, owner.makefile);
        match crate::genex::evaluate(&compiled, &mut ectx, &mut host) {
            Ok(value) => value,
            Err(error) => {
                if !error.quiet {
                    let backtrace = self.backtrace(error.backtrace);
                    self.error(backtrace, error.to_string());
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
        if let Some(utilities) = utilities {
            for dependency in dependencies {
                let name = self
                    .target_at(dir, &dependency.target)
                    .map_or(dependency.target.clone(), |target| target.name.clone());
                if !utilities.iter().any(|(existing, cross)| *existing == name && *cross == dependency.cross) {
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
                    self.error(frames, error.to_string());
                }
                String::new()
            }
        }
    }

    /// Runs the whole generate step.
    pub fn check_and_generate(&self) -> Result<(), Vec<Diagnostic>> {
        // cmGlobalGenerator::Compute: source resolution diagnostics surface
        // during the pre-generation target computations, and
        // CheckTargetsForMissingSources stops generation before any build
        // files are written.
        if super::target::check_targets_for_missing_sources(self) {
            return Err(std::mem::take(&mut *self.errors.borrow_mut()));
        }
        super::evaluation_files::generate(self);
        let mut ninja = super::ninja::NinjaGenerator::new(self);
        if let Err(error) = ninja.generate() {
            self.error(Vec::new(), format!("Unable to write build files: {error}"));
        }
        for result in [
            super::install::write_install_scripts(self),
            super::tests::write_test_files(self),
            super::export::write_build_exports(self),
        ] {
            if let Err(error) = result {
                self.error(Vec::new(), format!("Unable to write build files: {error}"));
            }
        }
        let errors = self.errors.borrow().clone();
        if errors.is_empty() { Ok(()) } else { Err(errors) }
    }
}
