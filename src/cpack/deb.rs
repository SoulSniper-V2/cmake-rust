//! Debian generator (`cmCPackDebGenerator`).

use std::fs::{self, File};
use std::path::Path;

use super::{Generator, Logger, is_on, option, option_nonempty, read_module, set_option};
use crate::carchive::{ArchiveWrite, Compress};
use crate::util;

fn recursive_files(root: &str) -> Vec<String> {
    let mut result = Vec::new();
    fn visit(path: &Path, out: &mut Vec<String>) {
        let mut entries = fs::read_dir(path)
            .map(|read| read.flatten().collect::<Vec<_>>())
            .unwrap_or_default();
        entries.sort_by_key(|entry| entry.file_name());
        for entry in entries {
            let child = entry.path();
            out.push(child.to_string_lossy().into_owned());
            if child.is_dir()
                && child
                    .symlink_metadata()
                    .is_ok_and(|m| !m.file_type().is_symlink())
            {
                visit(&child, out);
            }
        }
    }
    visit(Path::new(root), &mut result);
    result
}

fn md5_file(path: &str) -> String {
    use md5::Digest;
    let mut hasher = md5::Md5::new();
    if let Ok(bytes) = fs::read(path) {
        hasher.update(bytes);
    }
    hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn compression(spec: Option<&str>) -> (Compress, &'static str) {
    match spec.unwrap_or("gzip") {
        "lzma" => (Compress::Lzma, ".lzma"),
        "xz" => (Compress::Xz, ".xz"),
        "bzip2" => (Compress::BZip2, ".bz2"),
        "gzip" => (Compress::GZip, ".gz"),
        "zstd" => (Compress::Zstd, ".zst"),
        "none" => (Compress::None, ""),
        _ => (Compress::GZip, ".gz"),
    }
}

fn ordered_with_parents(files: &[String], workdir: &str) -> Vec<String> {
    let mut set = std::collections::BTreeSet::new();
    for file in files {
        let mut current = file.clone();
        while current != workdir {
            set.insert(current.clone());
            match Path::new(&current).parent() {
                Some(parent) => current = parent.to_string_lossy().into_owned(),
                None => break,
            }
        }
    }
    set.into_iter().collect()
}

fn open_writer(file: File, compress: Compress, format: &str, threads: i32) -> Option<ArchiveWrite> {
    let archive = ArchiveWrite::new(file, compress, format, 0, threads);
    if archive.error().is_empty() {
        Some(archive)
    } else {
        None
    }
}

#[allow(clippy::too_many_lines)]
pub(super) fn package_files(
    generator: &mut Generator,
    interp: &mut crate::interp::Interp,
    log: &mut Logger,
) -> bool {
    // The non-component and all-in-one cases share the same implementation;
    // per-component DEB packaging additionally changes WDIR/output naming
    // before running CPackDeb.cmake.
    generator.package_file_names.clear();
    let component_mode = generator.wants_components(interp);
    if component_mode && generator.method != super::Method::OnePackage {
        // Implement the standard one-package-per-group/component arrangement.
        let initial_top = option(interp, "CPACK_TEMPORARY_DIRECTORY").unwrap_or_default();
        let mut names: Vec<String> = if generator.method == super::Method::OnePackagePerComponent {
            generator.components.keys().cloned().collect()
        } else {
            let mut result = generator.groups.keys().cloned().collect::<Vec<_>>();
            result.extend(
                generator
                    .components
                    .values()
                    .filter(|c| c.group.is_none())
                    .map(|c| c.name.clone()),
            );
            result
        };
        names.sort();
        for package_name in names {
            if !package_one_pack(generator, interp, log, &initial_top, &package_name) {
                return false;
            }
        }
        return true;
    }
    if component_mode {
        set_option(
            interp,
            "CPACK_DEB_PACKAGE_COMPONENT_PART_PATH",
            "/ALL_COMPONENTS_IN_ONE",
        );
        set_option(
            interp,
            "CPACK_TEMPORARY_DIRECTORY",
            &format!(
                "{}/ALL_COMPONENTS_IN_ONE",
                option(interp, "CPACK_TEMPORARY_DIRECTORY").unwrap_or_default()
            ),
        );
    }
    package_all_in_one(generator, interp, log)
}

fn package_one_pack(
    generator: &mut Generator,
    interp: &mut crate::interp::Interp,
    log: &mut Logger,
    initial_top: &str,
    package_name: &str,
) -> bool {
    let sanitized = package_name.replace(' ', "-");
    let local_top = format!("{initial_top}/{sanitized}");
    let output = format!(
        "{}-{}{}",
        option(interp, "CPACK_PACKAGE_FILE_NAME").unwrap_or_default(),
        package_name,
        generator.output_extension(interp)
    );
    set_option(interp, "CPACK_TEMPORARY_DIRECTORY", &local_top);
    set_option(interp, "CPACK_OUTPUT_FILE_NAME", &output);
    set_option(
        interp,
        "CPACK_TEMPORARY_PACKAGE_FILE_NAME",
        &format!(
            "{}/{output}",
            Path::new(&generator.toplevel)
                .parent()
                .unwrap_or(Path::new("/"))
                .display()
        ),
    );
    set_option(interp, "CPACK_DEB_PACKAGE_COMPONENT", package_name);
    set_option(
        interp,
        "CPACK_DEB_PACKAGE_COMPONENT_PART_PATH",
        &format!("/{sanitized}"),
    );
    package_all_in_one(generator, interp, log)
}

#[allow(clippy::too_many_lines)]
fn package_all_in_one(
    generator: &mut Generator,
    interp: &mut crate::interp::Interp,
    log: &mut Logger,
) -> bool {
    if !read_module(interp, "Internal/CPack/CPackDeb.cmake") {
        log.error("Error while execution CPackDeb.cmake");
        return false;
    }
    let workdir = match option_nonempty(interp, "GEN_WDIR") {
        Some(value) => value,
        None => {
            log.error("Cannot find any files in the installed directory");
            return false;
        }
    };
    let files = recursive_files(&workdir);
    if files.is_empty() {
        log.error("Cannot find any files in the installed directory");
        return false;
    }

    // Control fields are emitted in std::map's lexicographic order upstream.
    let mut control: Vec<(String, String)> = Vec::new();
    for (var, key) in [
        ("GEN_CPACK_DEBIAN_PACKAGE_ARCHITECTURE", "Architecture"),
        ("GEN_CPACK_DEBIAN_PACKAGE_DESCRIPTION", "Description"),
        ("GEN_CPACK_DEBIAN_PACKAGE_MAINTAINER", "Maintainer"),
        ("GEN_CPACK_DEBIAN_PACKAGE_NAME", "Package"),
        ("GEN_CPACK_DEBIAN_PACKAGE_PRIORITY", "Priority"),
        ("GEN_CPACK_DEBIAN_PACKAGE_SECTION", "Section"),
        ("GEN_CPACK_DEBIAN_PACKAGE_VERSION", "Version"),
    ] {
        if let Some(value) = option(interp, var) {
            let value = if key == "Package" {
                value.to_lowercase()
            } else {
                value
            };
            control.push((key.to_owned(), value));
        }
    }
    for (var, key) in [
        ("GEN_CPACK_DEBIAN_PACKAGE_SOURCE", "Source"),
        ("GEN_CPACK_DEBIAN_PACKAGE_DEPENDS", "Depends"),
        ("GEN_CPACK_DEBIAN_PACKAGE_RECOMMENDS", "Recommends"),
        ("GEN_CPACK_DEBIAN_PACKAGE_SUGGESTS", "Suggests"),
        ("GEN_CPACK_DEBIAN_PACKAGE_HOMEPAGE", "Homepage"),
        ("GEN_CPACK_DEBIAN_PACKAGE_PREDEPENDS", "Pre-Depends"),
        ("GEN_CPACK_DEBIAN_PACKAGE_ENHANCES", "Enhances"),
        ("GEN_CPACK_DEBIAN_PACKAGE_BREAKS", "Breaks"),
        ("GEN_CPACK_DEBIAN_PACKAGE_CONFLICTS", "Conflicts"),
        ("GEN_CPACK_DEBIAN_PACKAGE_PROVIDES", "Provides"),
        ("GEN_CPACK_DEBIAN_PACKAGE_REPLACES", "Replaces"),
        ("GEN_CPACK_DEBIAN_PACKAGE_MULTIARCH", "Multi-Arch"),
    ] {
        if let Some(value) = option_nonempty(interp, var) {
            control.push((key.to_owned(), value));
        }
    }
    if let Some(multiarch) = option_nonempty(interp, "GEN_CPACK_DEBIAN_PACKAGE_MULTIARCH")
        && !matches!(multiarch.as_str(), "same" | "foreign" | "allowed")
    {
        log.error(&format!("Error: invalid value for Multi-Arch: {multiarch}. Valid values are: same, foreign, allowed"));
        return false;
    }
    control.sort();
    let _ = fs::create_dir_all(&workdir);
    let mut control_text = String::new();
    for (key, value) in &control {
        control_text.push_str(&format!("{key}: {value}\n"));
    }
    let total_size: u64 = files
        .iter()
        .map(|file| fs::metadata(file).map(|m| m.len()).unwrap_or(0))
        .sum();
    control_text.push_str(&format!("Installed-Size: {}\n", total_size.div_ceil(1024)));
    if fs::write(format!("{workdir}/control"), control_text).is_err() {
        return false;
    }
    if fs::write(format!("{workdir}/debian-binary"), "2.0\n").is_err() {
        return false;
    }

    let (compress, suffix) =
        compression(option(interp, "GEN_CPACK_DEBIAN_COMPRESSION_TYPE").as_deref());
    let archive_type =
        option(interp, "GEN_CPACK_DEBIAN_ARCHIVE_TYPE").unwrap_or_else(|| "gnutar".into());
    let threads = option(interp, "CPACK_THREADS")
        .and_then(|v| v.parse().ok())
        .unwrap_or(1);

    // data.tar.<compression>
    let data_name = format!("{workdir}/data.tar{suffix}");
    let Some(file) = File::create(&data_name).ok() else {
        return false;
    };
    let Some(mut data) = open_writer(file, compress, &archive_type, threads) else {
        return false;
    };
    if !data.open() {
        return false;
    }
    data.set_uid_gid(0, 0);
    data.set_uname_gname("root", "root");
    for file in ordered_with_parents(&files, &workdir) {
        if !data.add_options(&file, workdir.len(), Some("."), false) {
            log.error(&format!("Problem adding file to tar:\n#top level directory: {workdir}\n#file: {file}\n#error:{}", data.error()));
            return false;
        }
    }
    drop(data);

    // md5sums, whose paths are relative to the staging temporary directory.
    let temporary_dir =
        option(interp, "CPACK_TEMPORARY_DIRECTORY").unwrap_or_else(|| workdir.clone());
    let prefix = format!("{temporary_dir}/");
    let mut md5s = String::new();
    for file in &files {
        let meta = match fs::symlink_metadata(file) {
            Ok(meta) => meta,
            Err(_) => continue,
        };
        if meta.is_dir() || meta.file_type().is_symlink() {
            continue;
        }
        let relative = file.strip_prefix(&prefix).unwrap_or(file);
        md5s.push_str(&format!("{}  {relative}\n", md5_file(file)));
    }
    let md5name = format!("{workdir}/md5sums");
    if fs::write(&md5name, md5s).is_err() {
        return false;
    }

    // control.tar.gz is always gzip-compressed, independent of data compression.
    let control_name = format!("{workdir}/control.tar.gz");
    let Some(file) = File::create(&control_name).ok() else {
        return false;
    };
    let Some(mut control_tar) = open_writer(file, Compress::GZip, &archive_type, 0) else {
        return false;
    };
    if !control_tar.open() {
        return false;
    }
    control_tar.set_uid_gid(0, 0);
    control_tar.set_uname_gname("root", "root");
    control_tar.set_permissions(0o644);
    if !control_tar.add_options(&md5name, workdir.len(), Some("."), false)
        || !control_tar.add_options(
            &format!("{workdir}/control"),
            workdir.len(),
            Some("."),
            false,
        )
    {
        return false;
    }
    if is_on(interp, "GEN_CPACK_DEBIAN_PACKAGE_GENERATE_SHLIBS")
        && let Some(shlibs) = option_nonempty(interp, "GEN_CPACK_DEBIAN_PACKAGE_SHLIBS")
    {
        let shlibs_name = format!("{workdir}/shlibs");
        if fs::write(&shlibs_name, format!("{shlibs}\n")).is_err()
            || !control_tar.add_options(&shlibs_name, workdir.len(), Some("."), false)
        {
            return false;
        }
    }
    for (enabled, name) in [
        (
            is_on(interp, "GEN_CPACK_DEBIAN_GENERATE_POSTINST"),
            "postinst",
        ),
        (is_on(interp, "GEN_CPACK_DEBIAN_GENERATE_POSTRM"), "postrm"),
    ] {
        if enabled {
            let path = format!("{workdir}/{name}");
            let text = if name == "postinst" {
                "#!/bin/sh\n\nset -e\n\nif [ \"$1\" = \"configure\" ]; then\n\tldconfig\nfi\n"
            } else {
                "#!/bin/sh\n\nset -e\n\nif [ \"$1\" = \"remove\" ]; then\n\tldconfig\nfi\n"
            };
            let _ = fs::write(&path, text);
            control_tar.set_permissions(0o755);
            if !control_tar.add_options(&path, workdir.len(), Some("."), false) {
                return false;
            }
            control_tar.set_permissions(0o644);
        }
    }
    if let Some(extra) = option_nonempty(interp, "GEN_CPACK_DEBIAN_PACKAGE_CONTROL_EXTRA") {
        for original in util::expand_list(&extra, false) {
            let Some(filename) = Path::new(&original)
                .file_name()
                .map(|v| v.to_string_lossy().into_owned())
            else {
                continue;
            };
            let local = format!("{workdir}/{filename}");
            if fs::copy(&original, &local).is_ok() {
                control_tar.clear_permissions();
                if !control_tar.add_options(&local, workdir.len(), Some("."), false) {
                    return false;
                }
            }
        }
    }
    drop(control_tar);

    // The .deb itself is a BSD ar archive.
    let output_name = option(interp, "GEN_CPACK_OUTPUT_FILE_NAME")
        .or_else(|| option(interp, "CPACK_OUTPUT_FILE_NAME"))
        .unwrap_or_else(|| {
            format!(
                "{}.deb",
                option(interp, "CPACK_PACKAGE_FILE_NAME").unwrap_or_default()
            )
        });
    let output_root =
        option(interp, "CPACK_TOPLEVEL_DIRECTORY").unwrap_or_else(|| generator.toplevel.clone());
    let output_path = format!("{output_root}/{output_name}");
    let Some(file) = File::create(&output_path).ok() else {
        return false;
    };
    let Some(mut deb) = open_writer(file, Compress::None, "arbsd", 0) else {
        return false;
    };
    if !deb.open() {
        return false;
    }
    deb.set_uid_gid(0, 0);
    deb.set_uname_gname("root", "root");
    let tl = format!("{workdir}/");
    if !deb.add_options(&format!("{tl}debian-binary"), tl.len(), None, false)
        || !deb.add_options(&format!("{tl}control.tar.gz"), tl.len(), None, false)
        || !deb.add_options(&format!("{tl}data.tar{suffix}"), tl.len(), None, false)
    {
        log.error(&format!("Error creating debian package:\n#top level directory: {}\n#file: {output_name}\n#error:{}", generator.toplevel, deb.error()));
        return false;
    }
    drop(deb);
    generator.package_file_names.push(format!(
        "{}/{output_name}",
        option(interp, "CPACK_TOPLEVEL_DIRECTORY").unwrap_or_else(|| generator.toplevel.clone())
    ));
    true
}
