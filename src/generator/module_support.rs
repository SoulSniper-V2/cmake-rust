//! C++ module support queries of `cmGeneratorTarget`: HaveCxxModuleSupport,
//! NeedCxxDyndep and NeedDyndepForSource.

use std::cell::OnceCell;

use super::ctx::GenCtx;
use super::flags::standard;
use super::target::SourceInfo;
use crate::policy::PolicyStatus;
use crate::state::Target;
use crate::util;

/// `cmGeneratorTarget::Cxx20SupportLevel`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum CxxSupportLevel {
    MissingCxx,
    NoCxx20,
    MissingRule,
    Supported,
}

/// `cmGeneratorTarget::CxxModuleSupport`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CxxModuleSupport {
    Unavailable,
    Enabled,
    Disabled,
}

/// The index of a C++ standard among the resolver's levels.
pub(super) fn cxx_level(standard: &str) -> usize {
    standard::level(standard::levels("CXX"), standard).expect("C++ standard")
}

/// `cmGeneratorTarget::HaveCxxModuleSupport`.
pub(super) fn support_level(ctx: &GenCtx, target: &Target, config: &str) -> CxxSupportLevel {
    if !ctx.interp.langs.enabled.iter().any(|lang| lang == "CXX") {
        return CxxSupportLevel::MissingCxx;
    }
    if ctx
        .var(target.makefile, "CMAKE_CXX_STANDARD_DEFAULT")
        .is_none_or(|default| default.is_empty())
    {
        // We do not know any meaningful C++ standard levels.
        return CxxSupportLevel::NoCxx20;
    }
    if standard::explicit_level(ctx, target, config, "CXX")
        .is_none_or(|level| level < cxx_level("20"))
    {
        return CxxSupportLevel::NoCxx20;
    }
    if ctx
        .var(target.makefile, "CMAKE_CXX_SCANDEP_SOURCE")
        .is_none()
    {
        return CxxSupportLevel::MissingRule;
    }
    CxxSupportLevel::Supported
}

/// The files of the target's `CXX_MODULES` file sets.
pub(super) fn file_set_files(ctx: &GenCtx, target: &Target) -> Vec<String> {
    let source_dir = &ctx.interp.makefiles[target.makefile].source_dir;
    let mut files = Vec::new();
    for set in target.file_sets.values() {
        if set.kind != "CXX_MODULES" {
            continue;
        }
        for (entry, _backtrace) in &set.files {
            for file in util::expand_list(&ctx.genex(entry, Some(target), None), false) {
                files.push(util::collapse_full_path(&file, source_dir));
            }
        }
    }
    files
}

/// The module queries of one target in one configuration, which `ctx`
/// evaluates.
pub(super) struct ModuleSupport<'c, 'i> {
    ctx: &'c GenCtx<'i>,
    target: &'c Target,
    config: &'c str,
    files: OnceCell<Vec<String>>,
    level: OnceCell<CxxSupportLevel>,
    dyndep: OnceCell<CxxModuleSupport>,
}

impl<'c, 'i> ModuleSupport<'c, 'i> {
    pub(super) fn new(ctx: &'c GenCtx<'i>, target: &'c Target, config: &'c str) -> Self {
        ModuleSupport {
            ctx,
            target,
            config,
            files: OnceCell::new(),
            level: OnceCell::new(),
            dyndep: OnceCell::new(),
        }
    }

    pub(super) fn files(&self) -> &[String] {
        self.files
            .get_or_init(|| file_set_files(self.ctx, self.target))
    }

    pub(super) fn level(&self) -> CxxSupportLevel {
        *self
            .level
            .get_or_init(|| support_level(self.ctx, self.target, self.config))
    }

    /// `cmGeneratorTarget::NeedCxxDyndep`.
    fn need_cxx_dyndep(&self) -> CxxModuleSupport {
        *self.dyndep.get_or_init(|| {
            let have_rule = match self.level() {
                CxxSupportLevel::MissingCxx | CxxSupportLevel::NoCxx20 => {
                    return CxxModuleSupport::Unavailable;
                }
                CxxSupportLevel::MissingRule => false,
                CxxSupportLevel::Supported => true,
            };
            let enabled = match self.target.props.get("CXX_SCAN_FOR_MODULES") {
                Some(prop) => util::is_on(prop),
                None => {
                    have_rule
                        && self
                            .ctx
                            .interp
                            .policy_status_at(self.target.makefile, "CMP0155")
                            == PolicyStatus::New
                }
            };
            if enabled {
                CxxModuleSupport::Enabled
            } else {
                CxxModuleSupport::Disabled
            }
        })
    }

    /// `cmGeneratorTarget::NeedDyndepForSource`.
    pub(super) fn need_dyndep_for_source(&self, source: &SourceInfo) -> bool {
        match source.language.as_str() {
            "Fortran" => return true,
            "CXX" => {}
            _ => return false,
        }
        if self.files().contains(&source.path) {
            return true;
        }
        let dyndep = self.need_cxx_dyndep();
        if dyndep == CxxModuleSupport::Unavailable {
            return false;
        }
        if let Some(index) = crate::commands::target::find_known_source(
            self.ctx.interp,
            self.target.makefile,
            &source.path,
        ) && let Some(prop) = self.ctx.interp.ext.sources[&self.target.makefile][index]
            .props
            .get("CXX_SCAN_FOR_MODULES")
        {
            return util::is_on(prop);
        }
        dyndep == CxxModuleSupport::Enabled
    }
}
