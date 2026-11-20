//! Build-system generation (port of cmGlobalGenerator::Generate and the
//! cmGeneratorTarget / cmLocalGenerator / Ninja generator layer).
//!
//! Structure mirrors the C++ sources so work can proceed in parallel:
//! - `ctx.rs`      GenCtx: read-only access to the configured state + caches
//!                 (cmGlobalGenerator/cmLocalGenerator queries, genex hook).
//! - `target.rs`   cmGeneratorTarget core: names, directories, artifacts,
//!                 sources, linker language.            (lead)
//! - `options.rs`  compile definitions/options/includes/features/link
//!                 options/directories with usage requirements (uses genex).
//! - `flags.rs`    cmLocalGenerator flag assembly: AddLanguageFlags,
//!                 standards, PIC, visibility, include/define formatting,
//!                 cmRulePlaceholderExpander.
//! - `link.rs`     cmComputeLinkDepends + cmComputeLinkInformation (link
//!                 line items, search paths, rpaths).
//! - `custom.rs`   cmCustomCommandGenerator.
//! - `ninja/`      cmGlobalNinjaGenerator, cmLocalNinjaGenerator,
//!                 cmNinjaTargetGenerator + Normal/Utility.  (lead)
//! - `install.rs`  cmInstallGenerator family -> cmake_install.cmake.
//! - `tests.rs`    cmTestGenerator -> CTestTestfile.cmake.
//! - `export.rs`   export()/install(EXPORT) files.

pub mod ctx;
pub mod custom;
pub mod flags;
pub mod install;
pub mod link;
pub mod target;
pub mod tests;

use crate::configure::ConfigureOptions;
use crate::interp::Interp;

/// `cmake::Generate`: compute and write the build system. Returns 0 on
/// success.
pub fn generate(interp: &mut Interp, options: &ConfigureOptions) -> i32 {
    let start = std::time::Instant::now();
    let result = {
        let ctx = ctx::GenCtx::new(interp);
        ctx.check_and_generate()
    };
    if let Err(errors) = result {
        for (backtrace, message) in errors {
            interp.issue_at(crate::messages::MessageType::FatalError, &message, &backtrace);
        }
        return -1;
    }
    if interp.error {
        return -1;
    }
    if !options.in_try_compile {
        let elapsed = start.elapsed().as_secs_f64();
        crate::commands::core::status_line(interp, &format!("Generating done ({elapsed:.1}s)"));
    }
    0
}
