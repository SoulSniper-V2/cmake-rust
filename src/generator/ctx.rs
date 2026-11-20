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
}

impl<'a> GenCtx<'a> {
    pub fn new(interp: &'a Interp) -> GenCtx<'a> {
        let config = interp.get_def("CMAKE_BUILD_TYPE").unwrap_or_default().to_owned();
        GenCtx {
            interp,
            config,
            errors: RefCell::new(Vec::new()),
            memo: RefCell::new(HashMap::new()),
        }
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

    /// Evaluates generator expressions in `text` for `target`/`config`/
    /// `language`. Until the genex engine is integrated, text without `$<`
    /// passes through and anything else is reported as unsupported.
    pub fn genex(&self, text: &str, target: Option<&Target>, language: Option<&str>) -> String {
        let _ = (target, language);
        if !text.contains("$<") {
            return text.to_owned();
        }
        self.error(Vec::new(), format!("Error evaluating generator expression:\n  {text}\ngenerator expressions are not yet supported by this build of cmake-rust"));
        String::new()
    }

    /// Runs the whole generate step.
    pub fn check_and_generate(&self) -> Result<(), Vec<Diagnostic>> {
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
