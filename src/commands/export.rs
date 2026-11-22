//! Configure-time handling of export().
use std::path::Path;

use md5::{Digest, Md5};

use super::install::{package_dependencies_enabled, parse};
use super::project::export_package_info_enabled;
use crate::install_model::{BuildExport, ExportOptions};
use crate::interp::{CmdResult, Interp, Mode, err};
use crate::messages::MessageType;
use crate::package_info::PackageInfoArguments;
use crate::policy::{PolicyStatus, index_of, policy_warning};
use crate::state::TargetKind;
use crate::util::{filename_name, is_full_path, is_off, is_on};

pub fn register(interp: &mut Interp) {
    interp.register("export", export);
}

fn export(interp: &mut Interp, args: &[String]) -> CmdResult {
    if interp.mode == Mode::Script { return err("command is not scriptable"); }
    if args.len() < 2 { return err("called with too few arguments"); }
    let mode = args[0].as_str();
    if mode == "PACKAGE" { return package(interp, &args[1..]); }
    let mut singles = vec!["NAMESPACE", "FILE", "CXX_MODULES_DIRECTORY"];
    let mut lists = Vec::new();
    let mut flags = Vec::new();
    let experimental = package_dependencies_enabled(interp);
    match mode {
        "EXPORT" => {
            singles.push(mode);
            if experimental { flags.push("EXPORT_PACKAGE_DEPENDENCIES"); }
            if export_package_info_enabled(interp) {
                singles.extend(crate::package_info::SINGLE_KEYS);
                lists.extend(crate::package_info::LIST_KEYS);
                flags.extend(crate::package_info::FLAG_KEYS);
            }
        }
        "SETUP" => return setup(interp, args, experimental),
        _ => {
            singles.push("ANDROID_MK");
            lists.push("TARGETS");
            flags.extend(["APPEND", "EXPORT_LINK_INTERFACE_LIBRARIES"]);
        }
    }
    let parsed = parse(args, &singles, &lists, &flags);
    if let Some(arg) = parsed.unknown.first() { return err(format!("Unknown argument: \"{arg}\".")); }
    if parsed.has("ANDROID_MK") { return err("ANDROID_MK is not supported by cmake-rust"); }
    let mut package = PackageInfoArguments::from_args(&parsed);
    let mut cps = false;
    if package.package_name.is_empty() {
        package.check(false)?;
    } else {
        if !parsed.get("FILE").is_empty() {
            return err("PACKAGE_INFO and FILE are mutually exclusive.");
        }
        if !parsed.get("NAMESPACE").is_empty() {
            return err("PACKAGE_INFO and NAMESPACE are mutually exclusive.");
        }
        package.check(true)?;
        package.set_metadata_from_project(interp)?;
    }
    let set = parsed.get("EXPORT");
    let file = parsed.get("FILE");
    let fname = if file.is_empty() {
        if mode != "EXPORT" { return err("FILE <filename> option missing."); }
        if package.package_name.is_empty() {
            format!("{set}.cmake")
        } else {
            cps = true;
            package.package_file_name()
        }
    } else {
        if !file.ends_with(".cmake") {
            return err(format!("FILE option given filename \"{file}\" which does not have an extension of \".cmake\".\n"));
        }
        file.to_owned()
    };
    let path = if is_full_path(&fname) {
        if interp.is_on("CMAKE_DISABLE_SOURCE_CHANGES")
            && Path::new(&fname).starts_with(&interp.home_source)
            && !Path::new(&fname).starts_with(&interp.home_binary) {
            return err(format!("FILE option given filename \"{fname}\" which is in the source tree.\n"));
        }
        fname
    } else { format!("{}/{fname}", interp.current_binary_dir()) };
    let targets = parsed.list("TARGETS");
    if mode == "EXPORT" {
        if !interp.ext.install.export_sets.contains_key(set) {
            return err(format!("Export set \"{set}\" not found."));
        }
    } else if parsed.has("TARGETS") {
        for name in targets {
            if interp.ext.aliases.contains_key(name) {
                return err(format!("given ALIAS target \"{name}\" which may not be exported."));
            }
            let Some(target) = interp.ext.targets.get(name).filter(|t| !t.imported) else {
                return err(format!("given target \"{name}\" which is not built by this project."));
            };
            if target.kind == TargetKind::Utility {
                return err(format!("given custom target \"{name}\" which may not be exported."));
            }
        }
        if parsed.has("APPEND")
            && let Some(existing) = interp.ext.install.build_exports.get_mut(&path)
        {
            existing.targets.extend_from_slice(targets);
            return Ok(());
        }
    } else { return err("EXPORT or TARGETS specifier missing."); }
    let append = parsed.has("APPEND");
    if interp.ext.install.build_exports.contains_key(&path) {
        if cps {
            return err(format!(
                "command already specified for the file {}.",
                filename_name(&path)
            ));
        }
        let message = format!("command already specified for the file\n  {file}\nDid you miss 'APPEND' keyword?");
        match interp.policy_status("CMP0103") {
            PolicyStatus::New => return err(message),
            PolicyStatus::Warn => {
                let warning = policy_warning(index_of("CMP0103").unwrap());
                interp.issue(MessageType::AuthorWarning, &format!("{warning}\nexport() {message}"));
            }
            PolicyStatus::Old => {}
        }
    }
    let options = ExportOptions {
        namespace: parsed.get("NAMESPACE").into(),
        modules_directory: parsed.get("CXX_MODULES_DIRECTORY").into(),
        old_link_interface: parsed.has("EXPORT_LINK_INTERFACE_LIBRARIES"),
        package_dependencies: parsed.has("EXPORT_PACKAGE_DEPENDENCIES"),
    };
    interp.ext.install.build_exports.insert(path, BuildExport {
        makefile: interp.mf_index(),
        set: (mode == "EXPORT").then(|| set.to_owned()),
        targets: targets.to_vec(),
        options,
        append,
        package_info: cps.then_some(package),
        backtrace: interp.backtrace.clone(),
    });
    Ok(())
}

fn setup(interp: &mut Interp, args: &[String], experimental: bool) -> CmdResult {
    let mut base_end = args.len();
    let split_keywords = if experimental { &["TARGET", "PACKAGE_DEPENDENCY"][..] } else { &["TARGET"][..] };
    if let Some(index) = args.iter().position(|arg| split_keywords.contains(&arg.as_str())) { base_end = index; }
    let base = parse(&args[..base_end], &["SETUP", "NAMESPACE", "FILE", "CXX_MODULES_DIRECTORY"], &[], &[]);
    if let Some(arg) = base.unknown.first() { return err(format!("Unknown argument: \"{arg}\".")); }
    let set = interp.ext.install.export_sets.entry(base.get("SETUP").to_owned()).or_default();
    let mut start = base_end;
    while start < args.len() {
        let end = args[start + 1..].iter().position(|arg| split_keywords.contains(&arg.as_str()))
            .map_or(args.len(), |offset| start + 1 + offset);
        if let Some((name, values)) = args[start + 1..end].split_first() {
            let target = args[start] == "TARGET";
            let parsed = if target { parse(values, &["XCFRAMEWORK_LOCATION"], &[], &[]) }
                else { parse(values, &["ENABLED"], &["EXTRA_ARGS"], &[]) };
            if let Some(arg) = parsed.unknown.first() { return err(format!("Unknown argument: \"{arg}\".")); }
            if target {
                set.xcframework_locations.insert(name.clone(), parsed.get("XCFRAMEWORK_LOCATION").into());
            } else {
                let enabled = parsed.get("ENABLED");
                if !enabled.is_empty() && enabled != "AUTO" && !is_on(enabled) && !is_off(enabled) {
                    return err(format!("Invalid enable setting for package dependency: \"{enabled}\""));
                }
                if let Some((_, previous, extra)) = set.package_dependencies.iter_mut().find(|(package, _, _)| package == name) {
                    if !enabled.is_empty() { *previous = enabled.to_owned(); }
                    extra.extend_from_slice(parsed.list("EXTRA_ARGS"));
                } else {
                    set.package_dependencies.push((name.clone(), enabled.to_owned(), parsed.list("EXTRA_ARGS").to_vec()));
                }
            }
        }
        start = end;
    }
    Ok(())
}

fn package(interp: &mut Interp, args: &[String]) -> CmdResult {
    if let Some(arg) = args.get(1) { return err(format!("PACKAGE given unknown argument: {arg}")); }
    let name = &args[0];
    if name.is_empty() { return err("PACKAGE must be given a package name."); }
    if !name.bytes().all(|c| c.is_ascii_alphanumeric() || matches!(c, b'_' | b'.' | b'-')) {
        return err(format!("PACKAGE given invalid package name \"{name}\".  Package names must match \"^[A-Za-z0-9_.-]+$\"."));
    }
    let enabled = if interp.policy_status("CMP0090") == PolicyStatus::New {
        interp.is_on("CMAKE_EXPORT_PACKAGE_REGISTRY")
    } else { !interp.is_on("CMAKE_EXPORT_NO_PACKAGE_REGISTRY") };
    if enabled && let Some(home) = std::env::var_os("HOME") {
        let binary = interp.current_binary_dir();
        let dir = Path::new(&home).join(".cmake/packages").join(name);
        let path = dir.join(format!("{:x}", Md5::digest(binary.as_bytes())));
        match std::fs::create_dir_all(&dir).and_then(|()| {
            if path.exists() { return Ok(()); }
            std::fs::write(&path, format!("{binary}\n"))
        }) {
            Ok(()) => {}
            Err(error) => interp.issue(MessageType::Warning, &format!(
                "Cannot create package registry file:\n  {}\n{}\n", path.display(), error
            )),
        }
    }
    Ok(())
}
