//! The install name queries of `cmGeneratorTarget` on Apple platforms.

use super::ctx::GenCtx;
use super::target::{import_info, target_policy};
use crate::policy::{PolicyStatus, index_of};
use crate::state::{Target, TargetKind};
use crate::util::is_on;

/// `cmMakefile::IsSet`.
fn is_set(ctx: &GenCtx, target: &Target, name: &str) -> bool {
    ctx.var(target.makefile, name).is_some_and(|value| {
        !value.is_empty() && value != "NOTFOUND" && !value.ends_with("-NOTFOUND")
    })
}

/// `cmGeneratorTarget::MacOSXUseInstallNameDir`.
pub(super) fn use_install_name_dir(ctx: &GenCtx, target: &Target) -> bool {
    if let Some(value) = target.props.get("BUILD_WITH_INSTALL_NAME_DIR") {
        return is_on(value);
    }
    let policy = target_policy(ctx, target, index_of("CMP0068").unwrap());
    if policy == PolicyStatus::New {
        return false;
    }
    target
        .props
        .get("BUILD_WITH_INSTALL_RPATH")
        .is_some_and(is_on)
}

/// `cmGeneratorTarget::MacOSXRpathInstallNameDirDefault`.
pub(super) fn rpath_install_name_dir_default(ctx: &GenCtx, target: &Target) -> bool {
    is_set(ctx, target, "CMAKE_SHARED_LIBRARY_RUNTIME_C_FLAG")
        && target.props.get("MACOSX_RPATH").is_none_or(is_on)
}

/// `cmGeneratorTarget::HasMacOSXRpathInstallNameDir`.
pub(super) fn has_rpath_install_name_dir(ctx: &GenCtx, target: &Target, config: &str) -> bool {
    let key = format!("rpath-install-name:{}:{config}", ctx.target_key(target));
    if let Some(found) = ctx.memo.borrow().get(&key) {
        return found == "1";
    }
    let result = determine_rpath_install_name_dir(ctx, target, config);
    ctx.memo
        .borrow_mut()
        .insert(key, if result { "1" } else { "0" }.to_owned());
    result
}

fn determine_rpath_install_name_dir(ctx: &GenCtx, target: &Target, config: &str) -> bool {
    let mut install_name_is_rpath = false;
    let mut macosx_rpath = false;
    if !target.imported {
        if target.kind != TargetKind::SharedLibrary {
            return false;
        }
        if let Some(name) = target.props.get("INSTALL_NAME_DIR")
            && use_install_name_dir(ctx, target)
        {
            if name != "@rpath" {
                return false;
            }
            install_name_is_rpath = true;
        } else {
            macosx_rpath = rpath_install_name_dir_default(ctx, target);
        }
    } else if let Some(info) = import_info(target, config) {
        // Reading the install name of a library without a known soname needs
        // a Mach-O parser, which CMake has only on macOS hosts.
        install_name_is_rpath = !info.no_soname && info.soname.starts_with("@rpath/");
    }
    if !install_name_is_rpath && !macosx_rpath {
        return false;
    }
    if !is_set(ctx, target, "CMAKE_SHARED_LIBRARY_RUNTIME_C_FLAG") {
        let what = if macosx_rpath {
            "MACOSX_RPATH"
        } else {
            "@rpath"
        };
        ctx.error(
            target.backtrace.clone(),
            format!(
                "Attempting to use {what} without CMAKE_SHARED_LIBRARY_RUNTIME_C_FLAG being set.  This could be because you are using a Mac OS X version less than 10.5 or because CMake's platform configuration is corrupt."
            ),
        );
    }
    true
}
