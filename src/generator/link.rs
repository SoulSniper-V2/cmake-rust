//! Link line computation. OWNER: link worker. Spec: cmComputeLinkDepends.cxx,
//! cmComputeLinkInformation.cxx, cmLinkLineComputer.cxx,
//! cmNinjaLinkLineComputer, cmGeneratorTarget_Link.cxx,
//! cmComputeTargetDepends.cxx (target-level dependency graph, cycles).

use super::ctx::GenCtx;
use crate::state::Target;

/// Values for a Ninja link statement.
#[derive(Clone, Debug, Default)]
pub struct LinkLine {
    /// `LINK_LIBRARIES` value (already formatted, incl. rpath flags).
    pub libraries: String,
    /// `LINK_PATH` value (`-L...`).
    pub link_path: String,
    /// Files the link step depends on (implicit deps `|`).
    pub depends: Vec<String>,
}

pub fn link_line(ctx: &GenCtx, target: &Target, config: &str, language: &str) -> LinkLine {
    let _ = (ctx, target, config, language);
    todo!("generator::link::link_line")
}

/// Direct target-level dependencies (`cmComputeTargetDepends`): names of
/// targets that must be built before `target` (link deps + add_dependencies
/// + custom command target deps), in CMake's order.
pub fn target_dependencies(ctx: &GenCtx, target: &Target, config: &str) -> Vec<String> {
    let _ = (ctx, target, config);
    todo!("generator::link::target_dependencies")
}
