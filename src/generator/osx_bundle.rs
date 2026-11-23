//! `cmOSXBundleGenerator` (cmOSXBundleGenerator.cxx) plus the Apple
//! `Info.plist` configuration of `cmLocalGenerator::GenerateAppleInfoPList`
//! and `GenerateFrameworkInfoPList`, and the `cmGeneratorTarget` source
//! file flags that decide a source's place inside a bundle.

use std::collections::{BTreeSet, HashMap};
use std::io::Read;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use crate::state::Target;
use crate::util::{collapse_full_path, expand_list, filename_name, is_full_path};

use super::ctx::GenCtx;
use super::target as gt;
use super::target::ArtifactType;

/// `cmGeneratorTarget::SourceFileType`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SourceFileType {
    Normal,
    /// Listed in the `PRIVATE_HEADER` target property.
    PrivateHeader,
    /// Listed in the `PUBLIC_HEADER` target property.
    PublicHeader,
    /// In `RESOURCE` or `MACOSX_PACKAGE_LOCATION == "Resources"`.
    Resource,
    /// `MACOSX_PACKAGE_LOCATION` starts with `Resources/`.
    DeepResource,
    /// Any other `MACOSX_PACKAGE_LOCATION`.
    MacContent,
}

/// `cmGeneratorTarget::SourceFileFlags`.
#[derive(Clone, Debug)]
pub struct SourceFileFlags {
    pub kind: SourceFileType,
    /// Location inside the bundle content directory (`MacFolder`).
    pub mac_folder: String,
}

impl Default for SourceFileFlags {
    fn default() -> Self {
        SourceFileFlags {
            kind: SourceFileType::Normal,
            mac_folder: String::new(),
        }
    }
}

/// `cmGlobalGenerator::ShouldStripResourcePath`
/// (`cmMakefile::PlatformIsAppleEmbedded`).
fn should_strip_resource_path(ctx: &GenCtx, target: &Target) -> bool {
    gt::apple_embedded(ctx, target)
}

/// `cmSourceFile::ResolveFullPath`: ambiguous directories resolve
/// against the makefile's source directory.
fn source_path(interp: &crate::interp::Interp, makefile: usize, index: usize) -> String {
    let source = &interp.ext.sources[&makefile][index];
    let path = if source.location.directory.is_empty() {
        source.location.name.clone()
    } else {
        format!("{}/{}", source.location.directory, source.location.name)
    };
    if source.location.ambiguous_directory {
        collapse_full_path(&path, &interp.makefiles[makefile].source_dir)
    } else {
        path
    }
}

/// `cmGeneratorTarget::ConstructSourceFileFlags`: mark sources listed in
/// the PUBLIC_HEADER, PRIVATE_HEADER and RESOURCE target properties.
/// Keys are the sources' resolved full paths.
fn construct_source_file_flags(ctx: &GenCtx, target: &Target) -> HashMap<String, SourceFileFlags> {
    let mut map: HashMap<String, SourceFileFlags> = HashMap::new();
    let strip = should_strip_resource_path(ctx, target);
    let mut mark = |property: &str, folder: &str, kind: SourceFileType| {
        if let Some(files) = target.props.get(property) {
            for file in expand_list(files, false) {
                if let Some(index) =
                    crate::commands::target::find_source_index(ctx.interp, target.makefile, &file)
                {
                    map.insert(
                        source_path(ctx.interp, target.makefile, index),
                        SourceFileFlags {
                            kind,
                            mac_folder: folder.to_owned(),
                        },
                    );
                }
            }
        }
    };
    mark("PUBLIC_HEADER", "Headers", SourceFileType::PublicHeader);
    // Private headers take precedence when a file is listed in both.
    mark(
        "PRIVATE_HEADER",
        "PrivateHeaders",
        SourceFileType::PrivateHeader,
    );
    if let Some(files) = target.props.get("RESOURCE") {
        for file in expand_list(files, false) {
            if let Some(index) =
                crate::commands::target::find_source_index(ctx.interp, target.makefile, &file)
            {
                map.insert(
                    source_path(ctx.interp, target.makefile, index),
                    SourceFileFlags {
                        kind: SourceFileType::Resource,
                        mac_folder: if strip {
                            String::new()
                        } else {
                            "Resources".to_owned()
                        },
                    },
                );
            }
        }
    }
    map
}

/// `cmGeneratorTarget::GetTargetSourceFileFlags`: the flags of the source
/// whose resolved full path is `path` (a `SourceInfo::path`).
pub fn target_source_file_flags(
    ctx: &GenCtx,
    target: &Target,
    path: &str,
    props: Option<&crate::props::Props>,
) -> SourceFileFlags {
    let map = construct_source_file_flags(ctx, target);
    if let Some(flags) = map.get(&collapse_full_path(path, "")) {
        return flags.clone();
    }
    // The MACOSX_PACKAGE_LOCATION property applies to sources not listed
    // in one of the other lists.
    let location = props
        .and_then(|props| props.get("MACOSX_PACKAGE_LOCATION").map(str::to_owned))
        .or_else(|| {
            crate::commands::target::find_known_source(ctx.interp, target.makefile, path).and_then(
                |index| {
                    ctx.interp.ext.sources[&target.makefile][index]
                        .props
                        .get("MACOSX_PACKAGE_LOCATION")
                        .map(str::to_owned)
                },
            )
        });
    let Some(location) = location else {
        return SourceFileFlags::default();
    };
    let strip = should_strip_resource_path(ctx, target);
    let mut mac_folder = location.clone();
    let kind = if location == "Resources" {
        if strip {
            mac_folder.clear();
        }
        SourceFileType::Resource
    } else if let Some(rest) = location.strip_prefix("Resources/") {
        if strip {
            mac_folder = rest.to_owned();
        }
        SourceFileType::DeepResource
    } else {
        SourceFileType::MacContent
    };
    SourceFileFlags { kind, mac_folder }
}

/// `cmOSXBundleGenerator::InitMacOSXContentDirectory`: create (and record)
/// the content subdirectory, returning its full path.
pub fn init_mac_osx_content_directory(
    ctx: &GenCtx,
    target: &Target,
    pkgloc: &str,
    config: &str,
    folders: &mut BTreeSet<String>,
) -> String {
    let macdir = format!(
        "{}/{}",
        gt::mac_content_directory(ctx, target, config, ArtifactType::Runtime),
        pkgloc
    );
    let _ = std::fs::create_dir_all(&macdir);
    // Only the first level directory is recorded.
    folders.insert(pkgloc.split('/').next().unwrap_or(pkgloc).to_owned());
    macdir
}

/// `cmOSXBundleGenerator` `SkipParts`.
#[derive(Clone, Copy, Debug)]
pub struct SkipParts {
    pub info_plist: bool,
    pub text_stubs: bool,
}

impl Default for SkipParts {
    fn default() -> Self {
        // Upstream defaults TextStubs to true and enables it only for
        // targets with an import library.
        SkipParts {
            info_plist: false,
            text_stubs: true,
        }
    }
}

/// `cmSystemTools::CreateSymlink` (with `RemoveFile` first, like the
/// bundle generator's call sites): a symlink on Unix, a file copy
/// elsewhere, where a directory needs `CopyDirectoryIfDifferent` anyway —
/// the bundle generator only links files, so a plain copy matches.
fn create_symlink(old: &str, new: &str) {
    let _ = std::fs::remove_file(new);
    #[cfg(unix)]
    {
        let _ = std::os::unix::fs::symlink(old, new);
    }
    #[cfg(not(unix))]
    {
        let _ = std::fs::copy(old, new);
    }
}

/// `cmOSXBundleGenerator::MustSkip`
/// (`cmGeneratorTarget::HaveWellDefinedOutputFiles`).
fn must_skip(target: &Target) -> bool {
    !matches!(
        target.kind,
        crate::state::TargetKind::StaticLibrary
            | crate::state::TargetKind::SharedLibrary
            | crate::state::TargetKind::ModuleLibrary
            | crate::state::TargetKind::ObjectLibrary
            | crate::state::TargetKind::Executable
    )
}

/// `cmOSXBundleGenerator::CreateAppBundle`: create the bundle directory
/// and its Info.plist; returns the directory that now contains the
/// executable (`outpath` after the upstream `outpath = out` update).
pub fn create_app_bundle(
    ctx: &GenCtx,
    target: &Target,
    target_name: &str,
    outpath: &str,
    config: &str,
) -> String {
    if must_skip(target) {
        return outpath.to_owned();
    }
    let out = format!(
        "{outpath}/{}",
        gt::app_bundle_directory(ctx, target, config, gt::BundleLevel::Full)
    );
    let _ = std::fs::create_dir_all(&out);
    ctx.cmake_output_files.borrow_mut().push(out.clone());
    // Configure the Info.plist file (needs the executable name set).
    let plist = format!(
        "{outpath}/{}/Info.plist",
        gt::app_bundle_directory(ctx, target, config, gt::BundleLevel::Content)
    );
    generate_apple_info_plist(ctx, target, target_name, &plist);
    ctx.cmake_output_files.borrow_mut().push(plist);
    out
}

/// `cmOSXBundleGenerator::CreateFramework`.
pub fn create_framework(
    ctx: &GenCtx,
    target: &Target,
    target_name: &str,
    outpath: &str,
    config: &str,
    skip: SkipParts,
    folders: &mut BTreeSet<String>,
) {
    if must_skip(target) {
        return;
    }
    // The top-level foo.framework directory and its versioned content.
    let contentdir = format!(
        "{outpath}/{}/",
        gt::framework_directory(ctx, target, config, gt::BundleLevel::Content)
    );
    let newoutpath = format!(
        "{outpath}/{}",
        gt::framework_directory(ctx, target, config, gt::BundleLevel::Full)
    );
    let framework_version = gt::framework_version(target);
    let name = filename_name(target_name);
    if !skip.info_plist {
        let mut plist = newoutpath.clone();
        if !gt::apple_embedded(ctx, target) {
            // Put the Info.plist file into the Resources directory.
            folders.insert("Resources".to_owned());
            plist.push_str("/Resources");
        }
        plist.push_str("/Info.plist");
        generate_framework_info_plist(ctx, target, &name, &plist);
    }
    // The Versions layout exists only for non-embedded frameworks.
    if gt::apple_embedded(ctx, target) {
        return;
    }
    let versions = format!("{contentdir}Versions");
    let _ = std::fs::create_dir_all(&versions);
    let _ = std::fs::create_dir_all(&newoutpath);
    // Current -> version
    let current = format!("{versions}/Current");
    create_symlink(framework_version, &current);
    ctx.cmake_output_files.borrow_mut().push(current);
    // foo -> Versions/Current/foo
    let link = format!("{contentdir}{name}");
    create_symlink(&format!("Versions/Current/{name}"), &link);
    ctx.cmake_output_files.borrow_mut().push(link);
    if !skip.text_stubs {
        // foo.tbd -> Versions/Current/foo.tbd
        let tbd = ctx
            .var_safe(target.makefile, "CMAKE_APPLE_IMPORT_FILE_SUFFIX")
            .to_owned();
        let link = format!("{contentdir}{name}{tbd}");
        create_symlink(&format!("Versions/Current/{name}{tbd}"), &link);
        ctx.cmake_output_files.borrow_mut().push(link);
    }
    for folder in ["Resources", "Headers", "PrivateHeaders"] {
        if folders.contains(folder) {
            let link = format!("{contentdir}{folder}");
            create_symlink(&format!("Versions/Current/{folder}"), &link);
            ctx.cmake_output_files.borrow_mut().push(link);
        }
    }
}

/// `cmOSXBundleGenerator::CreateCFBundle`.
pub fn create_cf_bundle(
    ctx: &GenCtx,
    target: &Target,
    target_name: &str,
    root: &str,
    config: &str,
) {
    if must_skip(target) {
        return;
    }
    let out = format!(
        "{root}/{}",
        gt::cf_bundle_directory(ctx, target, config, gt::BundleLevel::Full)
    );
    let _ = std::fs::create_dir_all(&out);
    ctx.cmake_output_files.borrow_mut().push(out);
    let plist = format!(
        "{root}/{}/Info.plist",
        gt::cf_bundle_directory(ctx, target, config, gt::BundleLevel::Content)
    );
    let name = filename_name(target_name);
    generate_apple_info_plist(ctx, target, &name, &plist);
    ctx.cmake_output_files.borrow_mut().push(plist);
}

/// `cmMakefile::CanIWriteThisFile`.
fn can_write(ctx: &GenCtx, makefile: usize, file: &str) -> bool {
    if !ctx
        .var(makefile, "CMAKE_DISABLE_SOURCE_CHANGES")
        .is_some_and(|value| crate::util::is_on(&value))
    {
        return true;
    }
    let source = &ctx.interp.home_source;
    let binary = &ctx.interp.home_binary;
    let same = Path::new(source)
        .canonicalize()
        .ok()
        .zip(Path::new(binary).canonicalize().ok())
        .is_some_and(|(a, b)| a == b);
    if same {
        return !ctx
            .var(makefile, "CMAKE_DISABLE_IN_SOURCE_BUILD")
            .is_some_and(|value| crate::util::is_on(&value));
    }
    !crate::util::is_subdirectory(file, source)
        || crate::util::is_subdirectory(file, binary)
        || Path::new(file)
            .canonicalize()
            .ok()
            .zip(Path::new(binary).canonicalize().ok())
            .is_some_and(|(a, b)| a == b)
}

/// `cmMakefile::GetModulesFile`: find `name` in every CMAKE_MODULE_PATH
/// directory, then in `<cmake_root>/Modules`.
fn modules_file(ctx: &GenCtx, makefile: usize, name: &str) -> String {
    if let Some(paths) = ctx.var(makefile, "CMAKE_MODULE_PATH") {
        for directory in expand_list(&paths, false) {
            let candidate = format!("{}/{name}", directory.trim_end_matches('/'));
            if Path::new(&candidate).exists() {
                return candidate;
            }
        }
    }
    let candidate = format!("{}/Modules/{name}", ctx.interp.cmake_root);
    if Path::new(&candidate).exists() {
        return candidate;
    }
    String::new()
}

/// The variable substitutions of `cmMakefile::ConfigureString`
/// (`#cmakedefine` plus `ExpandVariablesInString` with `@VAR@` and the
/// `$ENV{}`/`$CACHE{}` domains). `defs` are the pushed scope bindings of
/// `cmLGInfoProp`; unlisted names fall through to the directory's final
/// variable state, matching `cmMakefile::GetDefinition`.
fn configure_plist(
    ctx: &GenCtx,
    makefile: usize,
    text: &str,
    defs: &HashMap<String, String>,
) -> Vec<u8> {
    let mut output = Vec::new();
    for line in text.as_bytes().split_inclusive(|byte| *byte == b'\n') {
        let have_newline = line.ends_with(b"\n");
        let line = line.strip_suffix(b"\n").unwrap_or(line);
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        for chunk in line.utf8_chunks() {
            let text = expand_plist(ctx, makefile, chunk.valid(), defs);
            output.extend(text.as_bytes());
            output.extend(chunk.invalid());
        }
        if have_newline {
            output.push(b'\n');
        }
    }
    output
}

/// `cmMakefile::ExpandVariablesInString` with the domains reachable from
/// `configure_file` input: `${VAR}` (scope defs then the directory's
/// final state), `$ENV{VAR}`, `$CACHE{VAR}` and `@VAR@`.
fn expand_plist(
    ctx: &GenCtx,
    makefile: usize,
    source: &str,
    defs: &HashMap<String, String>,
) -> String {
    let lookup = |name: &str| -> String {
        defs.get(name)
            .cloned()
            .or_else(|| ctx.var(makefile, name))
            .unwrap_or_default()
    };
    let mut out = String::with_capacity(source.len());
    let bytes = source.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let Some(off) = source[i..].find(['$', '@']) else {
            out.push_str(&source[i..]);
            break;
        };
        let pos = i + off;
        out.push_str(&source[i..pos]);
        let ch = bytes[pos];
        if ch == b'$' {
            if bytes.get(pos + 1) == Some(&b'{') {
                if let Some(end) = source[pos + 2..].find('}') {
                    let name = &source[pos + 2..pos + 2 + end];
                    out.push_str(&lookup(name));
                    i = pos + 3 + end;
                    continue;
                }
            } else if source[pos + 1..].starts_with("ENV{") {
                if let Some(end) = source[pos + 5..].find('}') {
                    let name = &source[pos + 5..pos + 5 + end];
                    out.push_str(
                        &std::env::var_os(name)
                            .map(|v| v.to_string_lossy().into_owned())
                            .unwrap_or_default(),
                    );
                    i = pos + 6 + end;
                    continue;
                }
            } else if source[pos + 1..].starts_with("CACHE{") {
                if let Some(end) = source[pos + 7..].find('}') {
                    let name = &source[pos + 7..pos + 7 + end];
                    out.push_str(ctx.interp.cache.value(name).unwrap_or_default());
                    i = pos + 8 + end;
                    continue;
                }
            }
            out.push('$');
            i = pos + 1;
        } else {
            // @VAR@
            if let Some(end) = source[pos + 1..].find('@') {
                let name = &source[pos + 1..pos + 1 + end];
                out.push_str(&lookup(name));
                i = pos + 2 + end;
            } else {
                out.push('@');
                i = pos + 1;
            }
        }
    }
    out
}

/// `cmMakefile::ConfigureFile(infile, outfile, false, false, false)` at
/// generation time: record the dependency and output, then write the
/// configured template.
fn configure_file(
    ctx: &GenCtx,
    makefile: usize,
    infile: &str,
    outfile: &str,
    defs: &HashMap<String, String>,
) {
    if !can_write(ctx, makefile, outfile) {
        ctx.system_error(format!(
            "Attempt to write file: {outfile} into a source directory."
        ));
        return;
    }
    ctx.cmake_depend_files.borrow_mut().push(infile.to_owned());
    ctx.cmake_output_files.borrow_mut().push(outfile.to_owned());
    let mut data = Vec::new();
    let mut input = match std::fs::File::open(infile) {
        Ok(input) => input,
        Err(_) => {
            ctx.system_error(format!("File {infile} does not exist."));
            return;
        }
    };
    if input.read_to_end(&mut data).is_err() {
        ctx.system_error(format!("File {infile} does not exist."));
        return;
    }
    if let Some(parent) = Path::new(outfile).parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let text = String::from_utf8_lossy(&data);
    let configured = configure_plist(ctx, makefile, &text, defs);
    if crate::fsutil::write_generated(Path::new(outfile), &configured).is_err() {
        ctx.system_error(format!(
            "Could not open file for write in copy operation {outfile}.tmp"
        ));
        return;
    }
    // ConfigureFile copies the source file's permissions.
    let mode = std::fs::metadata(infile)
        .map(|meta| meta.permissions().mode())
        .unwrap_or(0o644);
    let _ = std::fs::set_permissions(outfile, std::fs::Permissions::from_mode(mode));
}

/// `cmLocalGenerator::GenerateAppleInfoPList`.
pub fn generate_apple_info_plist(ctx: &GenCtx, target: &Target, target_name: &str, fname: &str) {
    let mut infile = target
        .props
        .get("MACOSX_BUNDLE_INFO_PLIST")
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| "MacOSXBundleInfo.plist.in".to_owned());
    if !is_full_path(&infile) {
        let resolved = modules_file(ctx, target.makefile, &infile);
        if !resolved.is_empty() {
            infile = resolved;
        }
    }
    if !Path::new(&infile).is_file() {
        ctx.system_error(format!(
            "Target {} Info.plist template \"{infile}\" could not be found.",
            target.name
        ));
        return;
    }
    // The target's MACOSX_BUNDLE_* properties become configure variables;
    // set properties override directory-level values.
    let mut defs: HashMap<String, String> = HashMap::new();
    defs.insert(
        "MACOSX_BUNDLE_EXECUTABLE_NAME".to_owned(),
        target_name.to_owned(),
    );
    for name in [
        "MACOSX_BUNDLE_INFO_STRING",
        "MACOSX_BUNDLE_ICON_FILE",
        "MACOSX_BUNDLE_GUI_IDENTIFIER",
        "MACOSX_BUNDLE_LONG_VERSION_STRING",
        "MACOSX_BUNDLE_BUNDLE_NAME",
        "MACOSX_BUNDLE_SHORT_VERSION_STRING",
        "MACOSX_BUNDLE_BUNDLE_VERSION",
        "MACOSX_BUNDLE_COPYRIGHT",
    ] {
        // cmLGInfoProp: the property, or the variable of the same name.
        if let Some(value) = target
            .props
            .get(name)
            .map(str::to_owned)
            .or_else(|| ctx.var(target.makefile, name))
        {
            defs.insert(name.to_owned(), value);
        }
    }
    configure_file(ctx, target.makefile, &infile, fname, &defs);
}

/// `cmLocalGenerator::GenerateFrameworkInfoPList`.
pub fn generate_framework_info_plist(
    ctx: &GenCtx,
    target: &Target,
    target_name: &str,
    fname: &str,
) {
    let mut infile = target
        .props
        .get("MACOSX_FRAMEWORK_INFO_PLIST")
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| "MacOSXFrameworkInfo.plist.in".to_owned());
    if !is_full_path(&infile) {
        let resolved = modules_file(ctx, target.makefile, &infile);
        if !resolved.is_empty() {
            infile = resolved;
        }
    }
    if !Path::new(&infile).is_file() {
        ctx.system_error(format!(
            "Target {} Info.plist template \"{infile}\" could not be found.",
            target.name
        ));
        return;
    }
    let mut defs: HashMap<String, String> = HashMap::new();
    defs.insert("MACOSX_FRAMEWORK_NAME".to_owned(), target_name.to_owned());
    for name in [
        "MACOSX_FRAMEWORK_ICON_FILE",
        "MACOSX_FRAMEWORK_IDENTIFIER",
        "MACOSX_FRAMEWORK_SHORT_VERSION_STRING",
        "MACOSX_FRAMEWORK_BUNDLE_NAME",
        "MACOSX_FRAMEWORK_BUNDLE_VERSION",
    ] {
        if let Some(value) = target
            .props
            .get(name)
            .map(str::to_owned)
            .or_else(|| ctx.var(target.makefile, name))
        {
            defs.insert(name.to_owned(), value);
        }
    }
    configure_file(ctx, target.makefile, &infile, fname, &defs);
}
