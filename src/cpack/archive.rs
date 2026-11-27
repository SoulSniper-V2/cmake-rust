//! Archive-family CPack generators (`cmCPackArchiveGenerator`).

use std::collections::{BTreeMap, BTreeSet};
use std::fs;

use super::{
    Component, Generator, Kind, Logger, is_on, option, option_nonempty, relative_path,
    sanitize_name, set_if_not_set,
};
use crate::carchive::{ArchiveWrite, Compress};

fn compression(kind: Kind) -> (Compress, &'static str) {
    match kind {
        Kind::Archive7z => (Compress::None, "7zip"),
        Kind::Tbz2 => (Compress::BZip2, "paxr"),
        Kind::Tgz | Kind::Stgz => (Compress::GZip, "paxr"),
        Kind::Txz => (Compress::Xz, "paxr"),
        Kind::Tz => (Compress::Compress, "paxr"),
        Kind::Tzst => (Compress::Zstd, "paxr"),
        Kind::Tar => (Compress::None, "gnutar"),
        Kind::Zip => (Compress::None, "zip"),
        _ => unreachable!(),
    }
}

pub(super) fn initialize_stgz(interp: &mut crate::interp::Interp, log: &mut Logger) -> bool {
    set_if_not_set(interp, "CPACK_INCLUDE_TOPLEVEL_DIRECTORY", "0");
    let mut input = String::new();
    for dir in super::list_option(interp, "CMAKE_MODULE_PATH") {
        let path = format!("{dir}/CPack.STGZ_Header.sh.in");
        if std::path::Path::new(&path).is_file() {
            input = path;
            break;
        }
    }
    if input.is_empty() {
        input = format!(
            "{}/Modules/Internal/CPack/CPack.STGZ_Header.sh.in",
            interp.cmake_root
        );
    }
    if !std::path::Path::new(&input).is_file() {
        log.error(&format!("Cannot find template file: {input}"));
        return false;
    }
    set_if_not_set(interp, "CPACK_STGZ_HEADER_FILE", &input);
    set_if_not_set(interp, "CPACK_AT_SIGN", "@");
    true
}

struct Deduplicator {
    symlinks: BTreeMap<String, String>,
    folders: BTreeSet<String>,
    files: BTreeMap<String, String>,
}

impl Deduplicator {
    fn new() -> Self {
        Self {
            symlinks: BTreeMap::new(),
            folders: BTreeSet::new(),
            files: BTreeMap::new(),
        }
    }

    /// 1 add, 0 skip, -1 error.
    fn check(&mut self, path: &str, local_top: &str) -> i32 {
        let disk = format!("{local_top}/{path}");
        let meta = fs::symlink_metadata(&disk).ok();
        if meta.as_ref().is_some_and(|m| m.file_type().is_dir()) {
            if self.folders.contains(path) {
                0
            } else {
                self.folders.insert(path.into());
                1
            }
        } else if meta.as_ref().is_some_and(|m| m.file_type().is_symlink()) {
            let target = fs::read_link(&disk)
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_default();
            if let Some(old) = self.symlinks.get(path) {
                if old == &target { 0 } else { -1 }
            } else {
                self.symlinks.insert(path.into(), target);
                1
            }
        } else if let Some(old) = self.files.get(path) {
            let same = fs::read(&disk)
                .ok()
                .zip(fs::read(old).ok())
                .is_some_and(|(a, b)| a == b);
            if same { 0 } else { -1 }
        } else {
            self.files.insert(path.into(), disk);
            1
        }
    }
}

fn open_archive(
    generator: &Generator,
    interp: &crate::interp::Interp,
    log: &mut Logger,
    filename: &str,
) -> Option<ArchiveWrite> {
    if let Some(parent) = std::path::Path::new(filename).parent() {
        let _ = fs::create_dir_all(parent);
    }
    let mut file = match fs::File::create(filename) {
        Ok(file) => file,
        Err(_) => {
            log.error(&format!("Problem to open archive <{filename}>"));
            return None;
        }
    };
    if generator.kind == Kind::Stgz && !write_stgz_header(interp, log, &mut file) {
        log.error(&format!(
            "Problem to generate Header for archive <{filename}>."
        ));
        return None;
    }
    let (compress, format) = compression(generator.kind);
    let threads = option_nonempty(interp, "CPACK_ARCHIVE_THREADS")
        .or_else(|| option_nonempty(interp, "CPACK_THREADS"))
        .and_then(|v| v.parse::<i32>().ok())
        .unwrap_or(1);
    let archive = ArchiveWrite::new(file, compress, format, 0, threads);
    if !archive.error().is_empty() {
        log.error(&format!(
            "Problem to create archive <{filename}>, ERROR = {}",
            archive.error()
        ));
        return None;
    }
    Some(archive)
}

fn open_in_place(archive: &mut ArchiveWrite, log: &mut Logger, filename: &str) -> bool {
    if !archive.open() {
        log.error(&format!(
            "Problem to open archive <{filename}>, ERROR = {}",
            archive.error()
        ));
        return false;
    }
    true
}

fn write_stgz_header(
    interp: &crate::interp::Interp,
    _log: &mut Logger,
    out: &mut fs::File,
) -> bool {
    use std::io::Write;
    let license_file = option(interp, "CPACK_RESOURCE_FILE_LICENSE").unwrap_or_default();
    let license = fs::read_to_string(&license_file).unwrap_or_default();
    let template = option(interp, "CPACK_STGZ_HEADER_FILE").unwrap_or_default();
    let Ok(text) = fs::read_to_string(template) else {
        return false;
    };
    let mut configured = String::new();
    let mut rest = text.as_str();
    while let Some(pos) = rest.find('@') {
        configured.push_str(&rest[..pos]);
        rest = &rest[pos + 1..];
        if let Some(end) = rest.find('@') {
            let name = &rest[..end];
            if !name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
                if name == "CPACK_RESOURCE_FILE_LICENSE_CONTENT" && interp.get_def(name).is_none() {
                    configured.push_str(&license);
                } else {
                    configured.push_str(interp.get_def(name).unwrap_or(""));
                }
                rest = &rest[end + 1..];
            } else {
                configured.push('@');
            }
        } else {
            configured.push('@');
            configured.push_str(rest);
            rest = "";
        }
    }
    configured.push_str(rest);
    let counter = configured.bytes().filter(|b| *b == b'\n').count() + 1;
    configured = configured.replace("###CPACK_HEADER_LENGTH###", &counter.to_string());
    out.write_all(configured.as_bytes()).is_ok()
}

fn add_component(
    _generator: &Generator,
    interp: &crate::interp::Interp,
    log: &mut Logger,
    archive: &mut ArchiveWrite,
    component: &Component,
    mut dedup: Option<&mut Deduplicator>,
) -> bool {
    log.verbose(&format!("   - packaging component: {}", component.name));
    let temp = option(interp, "CPACK_TEMPORARY_DIRECTORY").unwrap_or_default();
    let local_top = format!("{temp}/{}", sanitize_name(&component.name, false));
    let mut prefix = String::new();
    if is_on(interp, "CPACK_COMPONENT_INCLUDE_TOPLEVEL_DIRECTORY") {
        prefix.push_str(&option(interp, "CPACK_PACKAGE_FILE_NAME").unwrap_or_default());
        prefix.push('/');
    }
    if let Some(install_prefix) = option_nonempty(interp, "CPACK_PACKAGING_INSTALL_PREFIX")
        && install_prefix.len() > 1
        && install_prefix.starts_with('/')
    {
        prefix.push_str(install_prefix.trim_start_matches('/'));
        prefix.push('/');
    }
    for file in &component.files {
        let rp = format!("{prefix}{file}");
        let disk = format!("{local_top}/{file}");
        let mut add = true;
        if let Some(d) = dedup.as_deref_mut() {
            match d.check(&rp, &local_top) {
                0 => add = false,
                -1 => {
                    log.error("ERROR The data in files with the same filename is different.");
                    return false;
                }
                _ => {}
            }
        }
        if add {
            log.debug(&format!("Adding file: {rp}"));
            let skip = local_top.len() + 1;
            if !archive.add_options(&disk, skip, Some(&prefix), false) {
                log.error(&format!("ERROR while packaging files: {}", archive.error()));
                return false;
            }
        }
    }
    true
}

fn archive_file_name(generator: &Generator, interp: &crate::interp::Interp) -> String {
    let base = option_nonempty(interp, "CPACK_ARCHIVE_FILE_NAME")
        .unwrap_or_else(|| option(interp, "CPACK_PACKAGE_FILE_NAME").unwrap_or_default());
    format!(
        "{}/{}{}",
        generator.toplevel,
        base,
        generator.extension(interp)
    )
}

fn component_archive_name(
    generator: &Generator,
    interp: &crate::interp::Interp,
    component: &str,
    group: bool,
) -> String {
    let base = if let Some(v) = option_nonempty(
        interp,
        &format!("CPACK_ARCHIVE_{}_FILE_NAME", component.to_ascii_uppercase()),
    ) {
        v
    } else if let Some(v) = option_nonempty(interp, "CPACK_ARCHIVE_FILE_NAME") {
        generator.component_package_file_name(interp, &v, component, group)
    } else {
        let v = option(interp, "CPACK_PACKAGE_FILE_NAME").unwrap_or_default();
        generator.component_package_file_name(interp, &v, component, group)
    };
    format!("{}{}", base, generator.extension(interp))
}

fn package_components(
    generator: &mut Generator,
    interp: &mut crate::interp::Interp,
    log: &mut Logger,
    ignore_group: bool,
) -> bool {
    generator.package_file_names.clear();
    let mut jobs: Vec<(String, bool, Vec<String>)> = Vec::new();
    if !ignore_group {
        for (name, group) in &generator.groups {
            jobs.push((name.clone(), true, group.components.clone()));
        }
        for (name, comp) in &generator.components {
            if comp.group.is_none() {
                jobs.push((name.clone(), false, vec![name.clone()]));
            }
        }
    } else {
        for name in generator.components.keys() {
            jobs.push((name.clone(), false, vec![name.clone()]));
        }
    }
    for (package_name, is_group, components) in jobs {
        if is_group {
            log.verbose(&format!("Packaging component group: {package_name}"));
        }
        let filename = format!(
            "{}/{}",
            generator.toplevel,
            component_archive_name(generator, interp, &package_name, is_group)
        );
        let Some(mut archive) = open_archive(generator, interp, log, &filename) else {
            return false;
        };
        if !open_in_place(&mut archive, log, &filename) {
            return false;
        }
        let mut dedup = Deduplicator::new();
        for comp_name in components {
            let Some(comp) = generator.components.get(&comp_name) else {
                continue;
            };
            if !add_component(generator, interp, log, &mut archive, comp, Some(&mut dedup)) {
                return false;
            }
        }
        drop(archive);
        generator.package_file_names.push(filename);
    }
    true
}

fn package_all_in_one(
    generator: &mut Generator,
    interp: &mut crate::interp::Interp,
    log: &mut Logger,
) -> bool {
    generator.package_file_names.clear();
    let filename = archive_file_name(generator, interp);
    log.verbose(
        "Packaging all groups in one package...(CPACK_COMPONENTS_ALL_GROUPS_IN_ONE_PACKAGE is set)",
    );
    let Some(mut archive) = open_archive(generator, interp, log, &filename) else {
        return false;
    };
    if !open_in_place(&mut archive, log, &filename) {
        return false;
    }
    let mut dedup = Deduplicator::new();
    let components: Vec<Component> = generator.components.values().cloned().collect();
    for comp in &components {
        if !add_component(generator, interp, log, &mut archive, comp, Some(&mut dedup)) {
            return false;
        }
    }
    drop(archive);
    generator.package_file_names.push(filename);
    true
}

pub(super) fn package_files(
    generator: &mut Generator,
    interp: &mut crate::interp::Interp,
    log: &mut Logger,
) -> bool {
    log.debug(&format!("Toplevel: {}", generator.toplevel));
    if generator.wants_components(interp) {
        let ok = if generator.method == super::Method::OnePackage {
            package_all_in_one(generator, interp, log)
        } else {
            package_components(
                generator,
                interp,
                log,
                generator.method == super::Method::OnePackagePerComponent,
            )
        };
        if !ok {
            return false;
        }
    } else {
        generator.package_file_names.clear();
        let filename = archive_file_name(generator, interp);
        let Some(mut archive) = open_archive(generator, interp, log, &filename) else {
            return false;
        };
        if !open_in_place(&mut archive, log, &filename) {
            return false;
        }
        let skip = generator.toplevel.len() + 1;
        for file in generator.files.clone() {
            let rp = relative_path(&generator.toplevel, &file);
            if !archive.add_options(&file, skip, None, false) {
                log.error(&format!(
                    "Problem while adding file <{file}> to archive <{filename}>, ERROR = {}",
                    archive.error()
                ));
                return false;
            }
            let _ = rp;
        }
        drop(archive);
        generator.package_file_names.push(filename);
    }
    if generator.kind == Kind::Stgz {
        #[cfg(unix)]
        for name in generator.package_file_names.clone() {
            use std::os::unix::fs::PermissionsExt;
            if fs::set_permissions(&name, fs::Permissions::from_mode(0o777)).is_err() {
                return false;
            }
        }
    }
    true
}
