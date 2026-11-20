//! cmLocalGenerator flag assembly and rule-variable expansion. OWNER: flags
//! worker. Spec: cmLocalGenerator.cxx (AddLanguageFlags, AddCompileOptions,
//! AddFeatureFlags, AddArchitectureFlags, AddVisibilityPresetFlags,
//! AddConfigVariableFlags, AddCMP0018Flags/PIC, GetIncludeFlags,
//! GetTargetDefines/JoinDefines/AppendDefines, GetTargetCompileFlags,
//! GetStaticLibraryFlags, GetTargetFlags (link flags), AppendFlags/
//! AppendFlagEscape), cmStandardLevelResolver.cxx, cmRulePlaceholderExpander.cxx,
//! cmNinjaTargetGenerator::ComputeFlagsForObject/ComputeDefines/ComputeIncludes.

use super::ctx::GenCtx;
use super::target::SourceInfo;
use crate::state::Target;

/// Per-object compile variables as written into build.ninja
/// (`FLAGS`, `DEFINES`, `INCLUDES`).
#[derive(Clone, Debug, Default)]
pub struct CompileVars {
    pub flags: String,
    pub defines: String,
    pub includes: String,
}

/// `cmNinjaTargetGenerator::ComputeFlagsForObject` +
/// `ComputeDefines` + `ComputeIncludes` for one source.
pub fn compile_vars(ctx: &GenCtx, target: &Target, config: &str, source: &SourceInfo) -> CompileVars {
    let _ = (ctx, target, config, source);
    todo!("generator::flags::compile_vars")
}

/// Expands a rule template (e.g. `CMAKE_C_COMPILE_OBJECT`) with
/// `cmRulePlaceholderExpander`; `vars` maps placeholder names
/// (`<FLAGS>`, `<SOURCE>`...) to replacement text (Ninja `$VAR` refs).
pub fn expand_rule(ctx: &GenCtx, target: &Target, template: &str, vars: &[(&str, String)]) -> String {
    let _ = (ctx, target, template, vars);
    todo!("generator::flags::expand_rule")
}

/// Link-step flag variables for the link statement: `FLAGS`/
/// `LANGUAGE_COMPILE_FLAGS`, `LINK_FLAGS`, `ARCH_FLAGS`.
#[derive(Clone, Debug, Default)]
pub struct LinkFlagVars {
    pub flags: String,
    pub language_compile_flags: String,
    pub link_flags: String,
    pub arch_flags: String,
}

pub fn link_flag_vars(ctx: &GenCtx, target: &Target, config: &str, language: &str) -> LinkFlagVars {
    let _ = (ctx, target, config, language);
    todo!("generator::flags::link_flag_vars")
}
