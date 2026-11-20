//! cmake_install.cmake generation (cmInstallGenerator family).
//! OWNER: install worker.

use super::ctx::GenCtx;

/// Writes `<dir>/cmake_install.cmake` for every directory.
pub fn write_install_scripts(ctx: &GenCtx) -> std::io::Result<()> {
    let _ = ctx;
    todo!("generator::install::write_install_scripts")
}
