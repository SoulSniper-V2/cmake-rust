//! The install name queries of `cmGeneratorTarget` on Apple platforms.

use super::ctx::GenCtx;
use super::target::{ArtifactType, directory, import_info, target_policy};
use crate::policy::{PolicyStatus, index_of};
use crate::state::{Target, TargetKind};
use crate::util::{is_on, is_on_opt};

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

/// `cmGlobalGenerator::AddCMP0068WarnTarget`: the names are collected and
/// the warning is issued once generation finishes.
fn warn_cmp0068(ctx: &GenCtx, target: &Target) {
    ctx.cmp0068_warn_targets
        .borrow_mut()
        .insert(target.name.clone());
}

/// `cmGeneratorTarget::MacOSXUseInstallNameDir`, with the CMP0068 warning.
fn use_install_name_dir_warn(ctx: &GenCtx, target: &Target) -> bool {
    if let Some(value) = target.props.get("BUILD_WITH_INSTALL_NAME_DIR") {
        return is_on(value);
    }
    let policy = target_policy(ctx, target, index_of("CMP0068").unwrap());
    if policy == PolicyStatus::New {
        return false;
    }
    let use_install_name = is_on_opt(target.props.get("BUILD_WITH_INSTALL_RPATH"));
    if use_install_name && policy == PolicyStatus::Warn {
        warn_cmp0068(ctx, target);
    }
    use_install_name
}

/// `cmGeneratorTarget::CanGenerateInstallNameDir`.
fn can_generate_install_name_dir(ctx: &GenCtx, target: &Target, for_install: bool) -> bool {
    let policy = target_policy(ctx, target, index_of("CMP0068").unwrap());
    if policy == PolicyStatus::New {
        return true;
    }
    let mut skip = is_on_opt(ctx.var(target.makefile, "CMAKE_SKIP_RPATH").as_deref());
    if for_install {
        skip |= is_on_opt(
            ctx.var(target.makefile, "CMAKE_SKIP_INSTALL_RPATH")
                .as_deref(),
        );
    } else {
        skip |= is_on_opt(target.props.get("SKIP_BUILD_RPATH"));
    }
    if skip && policy == PolicyStatus::Warn {
        warn_cmp0068(ctx, target);
    }
    !skip
}

/// `cmGeneratorTarget::GetInstallNameDirForBuildTree`.
pub(super) fn install_name_dir_for_build_tree(
    ctx: &GenCtx,
    target: &Target,
    config: &str,
) -> String {
    if !is_set(ctx, target, "CMAKE_PLATFORM_HAS_INSTALLNAME") {
        return String::new();
    }
    // If building directly for installation then the build tree
    // install_name is the same as the install tree.
    if use_install_name_dir_warn(ctx, target) {
        let prefix = ctx.var_safe(target.makefile, "CMAKE_INSTALL_PREFIX");
        return install_name_dir_for_install_tree(ctx, target, config, &prefix);
    }
    // Use the build tree directory for the target.
    if can_generate_install_name_dir(ctx, target, false) {
        let mut dir = if rpath_install_name_dir_default(ctx, target) {
            "@rpath".to_owned()
        } else {
            directory(ctx, target, config, ArtifactType::Runtime)
        };
        dir.push('/');
        return dir;
    }
    String::new()
}

/// `cmGeneratorTarget::GetInstallNameDirForInstallTree`.
pub(super) fn install_name_dir_for_install_tree(
    ctx: &GenCtx,
    target: &Target,
    config: &str,
    install_prefix: &str,
) -> String {
    if !is_set(ctx, target, "CMAKE_PLATFORM_HAS_INSTALLNAME") {
        return String::new();
    }
    let mut dir = String::new();
    let install_name_dir = target.props.get("INSTALL_NAME_DIR");
    if can_generate_install_name_dir(ctx, target, true)
        && let Some(value) = install_name_dir.filter(|value| !value.is_empty())
    {
        // cmGeneratorExpression::ReplaceInstallPrefix.
        let value = value.replace("$<INSTALL_PREFIX>", install_prefix);
        let value = ctx.genex(&value, None, Some(config));
        if !value.is_empty() {
            dir = format!("{value}/");
        }
    }
    if install_name_dir.is_none() && rpath_install_name_dir_default(ctx, target) {
        dir = "@rpath/".to_owned();
    }
    dir
}
