//! The install name queries of `cmGeneratorTarget` on Apple platforms:
//! the `INSTALL_NAME_DIR` and rpath-derived install_name directories,
//! the CMP0068 warn-target bookkeeping of `cmGlobalGenerator`, and the
//! GNU-to-Microsoft import library name conversion.

use super::ctx::GenCtx;
use super::target::{ArtifactType, directory, has_import_library, import_info, target_policy};
use crate::policy::{PolicyStatus, index_of};
use crate::state::{Target, TargetKind};
use crate::util::{is_on, is_on_opt};

/// `cmMakefile::IsSet`.
fn is_set(ctx: &GenCtx, target: &Target, name: &str) -> bool {
    ctx.var(target.makefile, name).is_some_and(|value| {
        !value.is_empty() && value != "NOTFOUND" && !value.ends_with("-NOTFOUND")
    })
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
        // `MacOSXUseInstallNameDir` runs (and may warn) even without
        // INSTALL_NAME_DIR.
        let use_install_name = use_install_name_dir_warn(ctx, target);
        if let Some(name) = target.props.get("INSTALL_NAME_DIR")
            && use_install_name
        {
            if name != "@rpath" {
                return false;
            }
            install_name_is_rpath = true;
        } else {
            macosx_rpath = rpath_install_name_dir_default(ctx, target);
        }
    } else if let Some(info) = import_info(target, config) {
        // Lookup the imported soname.
        if !info.no_soname && !info.soname.is_empty() {
            if info.soname.starts_with("@rpath/") {
                install_name_is_rpath = true;
            }
        } else if guess_install_name(info.location)
            .is_some_and(|install_name| install_name.contains("@rpath"))
        {
            install_name_is_rpath = true;
        }
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
pub(super) fn use_install_name_dir_warn(ctx: &GenCtx, target: &Target) -> bool {
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
pub(super) fn can_generate_install_name_dir(
    ctx: &GenCtx,
    target: &Target,
    for_install: bool,
) -> bool {
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
    if !is_on(&ctx.var_safe(target.makefile, "CMAKE_PLATFORM_HAS_INSTALLNAME")) {
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
    if !is_on(&ctx.var_safe(target.makefile, "CMAKE_PLATFORM_HAS_INSTALLNAME")) {
        return String::new();
    }
    let mut dir = String::new();
    let install_name_dir = target.props.get("INSTALL_NAME_DIR");
    if can_generate_install_name_dir(ctx, target, true)
        && let Some(value) = install_name_dir.filter(|value| !value.is_empty())
    {
        let value = crate::genex::replace_install_prefix(value, install_prefix);
        let value = ctx.for_config(config).genex(&value, Some(target), None);
        if !value.is_empty() {
            dir = format!("{value}/");
        }
    }
    if install_name_dir.is_none() && rpath_install_name_dir_default(ctx, target) {
        dir = "@rpath/".to_owned();
    }
    dir
}

/// `cmGeneratorTarget::HasImplibGNUtoMS`.
pub fn has_implib_gnu_to_ms(ctx: &GenCtx, target: &Target, config: &str) -> bool {
    has_import_library(ctx, target, config) && is_on_opt(target.props.get("GNUtoMS"))
}

/// `cmGeneratorTarget::GetImplibGNUtoMS`: the Microsoft-style import
/// library name for a GNU import library name, when the target's
/// `GNUtoMS` conversion applies.
pub fn implib_gnu_to_ms(
    ctx: &GenCtx,
    target: &Target,
    config: &str,
    gnu_name: &str,
    new_ext: Option<&str>,
) -> Option<String> {
    if has_implib_gnu_to_ms(ctx, target, config)
        && gnu_name.len() > ".dll.a".len()
        && let Some(base) = gnu_name.strip_suffix(".dll.a")
    {
        return Some(format!("{base}{}", new_ext.unwrap_or(".lib")));
    }
    None
}

/// `cmSystemTools::GuessLibraryInstallName`: the install_name recorded
/// by a Mach-O binary's `LC_ID_DYLIB` load command.
fn guess_install_name(path: &str) -> Option<String> {
    use std::io::Read as _;
    let mut data = Vec::new();
    std::fs::File::open(path)
        .ok()?
        .take(1 << 20)
        .read_to_end(&mut data)
        .ok()?;
    mach_o_install_name(&data, 0)
}

/// The `LC_ID_DYLIB` name of the Mach-O binary at `offset` in `data`.
fn mach_o_install_name(data: &[u8], offset: usize) -> Option<String> {
    let data = data.get(offset..)?;
    let magic = u32::from_be_bytes(data.get(..4)?.try_into().ok()?);
    let (little, is64) = match magic {
        0xFEEDFACE | 0xCEFAEDFE => (magic == 0xCEFAEDFE, false),
        0xFEEDFACF | 0xCFFAEDFE => (magic == 0xCFFAEDFE, true),
        // A fat binary: inspect its first architecture.
        0xCAFEBABE | 0xCAFEBABF => {
            let u32be = |at: usize| -> Option<u32> {
                Some(u32::from_be_bytes(data.get(at..at + 4)?.try_into().ok()?))
            };
            if u32be(4)? == 0 {
                return None;
            }
            let offset = if magic == 0xCAFEBABE {
                u32be(16)? as usize
            } else {
                let high = u32be(16)? as usize;
                (high << 32) | u32be(20)? as usize
            };
            return mach_o_install_name(data, offset);
        }
        _ => return None,
    };
    let u32x = |at: usize| -> Option<u32> {
        let bytes: [u8; 4] = data.get(at..at + 4)?.try_into().ok()?;
        Some(if little {
            u32::from_le_bytes(bytes)
        } else {
            u32::from_be_bytes(bytes)
        })
    };
    let ncmds = u32x(16)? as usize;
    let mut at = if is64 { 32 } else { 28 };
    const LC_ID_DYLIB: u32 = 0x0D;
    for _ in 0..ncmds {
        let cmd = u32x(at)?;
        let cmdsize = u32x(at + 4)? as usize;
        if cmdsize < 8 {
            return None;
        }
        if cmd == LC_ID_DYLIB {
            // struct dylib_command: lc_str name at cmd+8; the install
            // name is the NUL-terminated string at cmd + name.offset.
            let name_offset = u32x(at + 8)? as usize;
            let name_at = at.checked_add(name_offset)?;
            let end = data.get(name_at..)?.iter().position(|b| *b == 0)? + name_at;
            return String::from_utf8(data.get(name_at..end)?.to_vec()).ok();
        }
        at = at.checked_add(cmdsize)?;
    }
    None
}
