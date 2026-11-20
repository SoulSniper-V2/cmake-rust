//! cmGeneratorTarget core: artifact naming, directories, sources, object
//! names, linker language. OWNER: generator-target worker.
//! Spec: cmGeneratorTarget.cxx (GetFullName/GetFullPath/GetDirectory/
//! GetOutputName/GetLibraryNames/GetExecutableNames/GetObjectDirectory/
//! ComputeObjectMapping/GetLinkerLanguage/GetSourceFiles...),
//! cmGeneratorTarget_Sources.cxx, cmLocalGenerator::GetObjectFileNameWithoutTarget.

use std::collections::{BTreeSet, HashSet};
use std::path::Path;

use md5::{Digest, Md5};

use super::ctx::GenCtx;
use crate::messages::{self, Frame, MessageType};
use crate::policy::{self, PolicyStatus};
use crate::state::{Target, TargetKind};
use crate::util::{
    collapse_full_path, expand_list, filename_name, filename_path, is_full_path, is_on_opt,
    is_subdirectory,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ArtifactType {
    /// Executable, shared library, module, or the archive of a static lib.
    Runtime,
    /// Import library (Windows/AIX/macOS-tbd); archive for static libs.
    Import,
}

/// `cmGeneratorTarget::Names`.
#[derive(Clone, Debug, Default)]
pub struct Names {
    pub base: String,
    pub output: String,
    pub real: String,
    pub import_library: String,
    pub pdb: String,
    pub shared_object: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SourceKind {
    /// Compiled to an object file.
    ObjectSource,
    Header,
    ExternalObject,
    /// HEADER_FILE_ONLY or no language.
    Extra,
    CustomCommandOutput,
    ModuleDefinition,
    Resource,
}

#[derive(Clone, Debug)]
pub struct SourceInfo {
    /// Full path.
    pub path: String,
    /// Language (empty when none).
    pub language: String,
    pub kind: SourceKind,
    /// Object file name relative to the target's object directory, e.g.
    /// `src/foo.c.o` (empty for non-object sources).
    pub object_name: String,
    /// Whether this source is GENERATED.
    pub generated: bool,
}

/// Prefix, configured base (including the postfix), and suffix of an artifact.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct NameComponents {
    pub prefix: String,
    pub base: String,
    pub suffix: String,
}

impl NameComponents {
    fn filename(&self) -> String {
        format!("{}{}{}", self.prefix, self.base, self.suffix)
    }
}

fn artifact_kind(kind: TargetKind, artifact: ArtifactType) -> &'static str {
    match (kind, artifact) {
        (TargetKind::Executable, ArtifactType::Runtime) => "RUNTIME",
        (TargetKind::SharedLibrary | TargetKind::ModuleLibrary, ArtifactType::Runtime) => "LIBRARY",
        (TargetKind::Executable | TargetKind::SharedLibrary | TargetKind::ModuleLibrary, _)
        | (TargetKind::StaticLibrary, _) => "ARCHIVE",
        (TargetKind::ObjectLibrary, _) => "OBJECT",
        _ => "",
    }
}

fn memo_key(query: &str, target: &Target, config: &str) -> String {
    format!(
        "target:{query}:{}:{}:{config}",
        target.makefile, target.name
    )
}

fn cached_string(
    ctx: &GenCtx,
    key: String,
    recursion_error: impl FnOnce() -> String,
    compute: impl FnOnce() -> String,
) -> String {
    let cached = ctx.memo.borrow().get(&key).cloned();
    if let Some(value) = cached {
        if value.is_empty() {
            ctx.error(Vec::new(), recursion_error());
        }
        return value;
    }
    ctx.memo.borrow_mut().insert(key.clone(), String::new());
    let result = compute();
    ctx.memo.borrow_mut().insert(key, result.clone());
    result
}

pub fn output_name(ctx: &GenCtx, target: &Target, config: &str, artifact: ArtifactType) -> String {
    let kind = artifact_kind(target.kind, artifact);
    let config_upper = config.to_ascii_uppercase();
    let key = memo_key(&format!("output-name-{artifact:?}"), target, config);
    cached_string(
        ctx,
        key,
        || format!("Target '{}' OUTPUT_NAME depends on itself.", target.name),
        || {
            let mut properties = Vec::new();
            if !kind.is_empty() {
                if !config_upper.is_empty() {
                    properties.push(format!("{kind}_OUTPUT_NAME_{config_upper}"));
                }
                properties.push(format!("{kind}_OUTPUT_NAME"));
            }
            if !config_upper.is_empty() {
                properties.push(format!("OUTPUT_NAME_{config_upper}"));
                properties.push(format!("{config_upper}_OUTPUT_NAME"));
            }
            properties.push("OUTPUT_NAME".to_owned());
            let name = properties
                .iter()
                .find_map(|property| target.props.get(property))
                .filter(|value| !value.is_empty())
                .unwrap_or(&target.name);
            ctx.genex(name, None, None)
        },
    )
}

fn postfix<'a>(target: &'a Target, config: &str) -> &'a str {
    if config.is_empty() {
        return "";
    }
    let config = config.to_ascii_uppercase();
    target
        .props
        .get(&format!("FRAMEWORK_MULTI_CONFIG_POSTFIX_{config}"))
        .filter(|value| !value.is_empty())
        .or_else(|| target.props.get(&format!("{config}_POSTFIX")))
        .unwrap_or_default()
}

fn affix(ctx: &GenCtx, target: &Target, property: &str, variable: &str, language: &str) -> String {
    if let Some(value) = target.props.get(property) {
        return value.to_owned();
    }
    if variable.is_empty() {
        return String::new();
    }
    if !language.is_empty()
        && let Some(value) = ctx.var(target.makefile, &format!("{variable}_{language}"))
    {
        return value;
    }
    ctx.var_safe(target.makefile, variable)
}

pub fn full_name_components(
    ctx: &GenCtx,
    target: &Target,
    config: &str,
    artifact: ArtifactType,
) -> NameComponents {
    let (prefix_var, suffix_var) = match target.kind {
        TargetKind::Executable => ("", "CMAKE_EXECUTABLE_SUFFIX"),
        TargetKind::StaticLibrary => ("CMAKE_STATIC_LIBRARY_PREFIX", "CMAKE_STATIC_LIBRARY_SUFFIX"),
        TargetKind::SharedLibrary => ("CMAKE_SHARED_LIBRARY_PREFIX", "CMAKE_SHARED_LIBRARY_SUFFIX"),
        TargetKind::ModuleLibrary => ("CMAKE_SHARED_MODULE_PREFIX", "CMAKE_SHARED_MODULE_SUFFIX"),
        _ => {
            return NameComponents {
                base: target.name.clone(),
                ..NameComponents::default()
            };
        }
    };
    // NeedImportLibraryName is false for native Linux targets. Imported
    // artifacts use the paths from GetMappedConfig instead.
    if artifact == ArtifactType::Import {
        return NameComponents::default();
    }
    let language = compute_linker_language(ctx, target, config);
    let mut base = output_name(ctx, target, config, artifact);
    base.push_str(postfix(target, config));
    if target.kind == TargetKind::SharedLibrary
        && is_on_opt(
            ctx.var(target.makefile, "CMAKE_SHARED_LIBRARY_NAME_WITH_VERSION")
                .as_deref(),
        )
        && let Some(version) = target.props.get("SOVERSION")
    {
        base.push('-');
        base.push_str(version);
    }
    NameComponents {
        prefix: affix(ctx, target, "PREFIX", prefix_var, &language),
        base,
        suffix: affix(ctx, target, "SUFFIX", suffix_var, &language),
    }
}

/// `cmGeneratorTarget::GetLibraryNames` / `GetExecutableNames`.
pub fn names(ctx: &GenCtx, target: &Target, config: &str) -> Names {
    let parts = full_name_components(ctx, target, config, ArtifactType::Runtime);
    let output = parts.filename();
    let pdb_prop = (!config.is_empty())
        .then(|| {
            target
                .props
                .get(&format!("PDB_NAME_{}", config.to_ascii_uppercase()))
        })
        .flatten()
        .or_else(|| target.props.get("PDB_NAME"))
        .filter(|value| !value.is_empty());
    let pdb_base = pdb_prop.map_or_else(
        || {
            format!(
                "{}{}",
                output_name(ctx, target, config, ArtifactType::Runtime),
                postfix(target, config)
            )
        },
        |value| ctx.genex(value, None, None),
    );
    let (real, shared_object) = if target.kind == TargetKind::Executable {
        let real = target
            .props
            .get("VERSION")
            .map_or_else(|| output.clone(), |version| format!("{output}-{version}"));
        (real, String::new())
    } else {
        let versioned = has_soname(ctx, target, config)
            && !is_on_opt(
                ctx.var(target.makefile, "CMAKE_PLATFORM_NO_VERSIONED_SONAME")
                    .as_deref(),
            );
        let version = target.props.get("VERSION");
        let soversion = target.props.get("SOVERSION");
        let versioned_name = |version: Option<&str>| match version.filter(|_| versioned) {
            Some(version) => format!("{output}.{version}"),
            None => output.clone(),
        };
        (
            versioned_name(version.or(soversion)),
            versioned_name(soversion.or(version)),
        )
    };
    Names {
        base: parts.base,
        output,
        real,
        import_library: String::new(),
        pdb: format!("{}{pdb_base}.pdb", parts.prefix),
        shared_object,
    }
}

/// `cmGeneratorTarget::GetFullPath(config, artifact, realname)`.
pub fn full_path(
    ctx: &GenCtx,
    target: &Target,
    config: &str,
    artifact: ArtifactType,
    realname: bool,
) -> String {
    if target.imported {
        return imported_full_path(ctx, target, config, artifact);
    }
    let dir = directory(ctx, target, config, artifact);
    let file = if realname && artifact == ArtifactType::Runtime {
        names(ctx, target, config).real
    } else {
        full_name(ctx, target, config, artifact)
    };
    format!("{dir}/{file}")
}

/// `cmGeneratorTarget::GetDirectory(config, artifact)`.
pub fn directory(ctx: &GenCtx, target: &Target, config: &str, artifact: ArtifactType) -> String {
    if target.imported {
        return filename_path(&imported_full_path(ctx, target, config, artifact));
    }
    output_directory(ctx, target, config, artifact, |value| {
        ctx.genex(value, Some(target), None)
    })
}

fn output_directory(
    ctx: &GenCtx,
    target: &Target,
    config: &str,
    artifact: ArtifactType,
    evaluate: impl FnOnce(&str) -> String,
) -> String {
    let kind = artifact_kind(target.kind, artifact);
    if kind.is_empty() {
        ctx.error(
            Vec::new(),
            format!(
                "cmGeneratorTarget::GetOutputInfo called for {} which has type {}",
                target.name,
                target.kind.type_name()
            ),
        );
        return String::new();
    }
    let config_upper = config.to_ascii_uppercase();
    let key = memo_key(&format!("output-dir-{artifact:?}"), target, &config_upper);
    cached_string(
        ctx,
        key,
        || {
            format!(
                "Target '{}' OUTPUT_DIRECTORY depends on itself.",
                target.name
            )
        },
        || {
            let property = format!("{kind}_OUTPUT_DIRECTORY");
            let out = if let Some(value) = target
                .props
                .get(&format!("{property}_{config_upper}"))
                .or_else(|| target.props.get(&property))
            {
                evaluate(value)
            } else {
                let legacy = match target.kind {
                    TargetKind::Executable => "EXECUTABLE_OUTPUT_PATH",
                    TargetKind::StaticLibrary
                    | TargetKind::SharedLibrary
                    | TargetKind::ModuleLibrary => "LIBRARY_OUTPUT_PATH",
                    _ => "",
                };
                ctx.var_safe(target.makefile, legacy)
            };
            // AppendDirectoryForConfig is a no-op for the single-config Ninja generator.
            collapse_full_path(&out, &ctx.interp.makefiles[target.makefile].binary_dir)
        },
    )
}

/// `cmGeneratorTarget::GetFullName(config, artifact)`.
pub fn full_name(ctx: &GenCtx, target: &Target, config: &str, artifact: ArtifactType) -> String {
    if target.imported {
        return filename_name(&imported_full_path(ctx, target, config, artifact));
    }
    full_name_components(ctx, target, config, artifact).filename()
}

/// `cmGeneratorTarget::GetObjectDirectory` (absolute, with trailing '/').
pub fn object_directory(ctx: &GenCtx, target: &Target, _config: &str) -> String {
    let binary_dir = ctx.interp.makefiles[target.makefile]
        .binary_dir
        .trim_end_matches('/');
    format!("{binary_dir}/CMakeFiles/{}.dir/", target.name)
}

/// `cmGeneratorTarget::HasSOName`.
pub fn has_soname(ctx: &GenCtx, target: &Target, config: &str) -> bool {
    if target.kind != TargetKind::SharedLibrary || is_on_opt(target.props.get("NO_SONAME")) {
        return false;
    }
    let language = compute_linker_language(ctx, target, config);
    let variable = if language.is_empty() {
        "CMAKE_SHARED_LIBRARY_SONAME_FLAG".to_owned()
    } else {
        format!("CMAKE_SHARED_LIBRARY_SONAME_{language}_FLAG")
    };
    ctx.var(target.makefile, &variable).is_some()
}

pub fn is_framework_on_apple(_ctx: &GenCtx, _target: &Target, _config: &str) -> bool {
    false
}

#[derive(Debug)]
pub struct ImportedConfig<'a> {
    /// Includes a leading underscore for a configuration-specific match.
    pub suffix: String,
    pub location: Option<&'a str>,
    pub implib: Option<&'a str>,
}

fn can_have_imported_implib(target: &Target) -> bool {
    target.kind == TargetKind::SharedLibrary
        || (target.kind == TargetKind::Executable && is_on_opt(target.props.get("ENABLE_EXPORTS")))
}

/// Select properties for `cmTarget::GetMappedConfig` in lookup order.
pub fn mapped_import_config<'a>(target: &'a Target, config: &str) -> Option<ImportedConfig<'a>> {
    let desired = if config.is_empty() {
        "NOCONFIG".to_owned()
    } else {
        config.to_ascii_uppercase()
    };
    let location_property = match target.kind {
        TargetKind::InterfaceLibrary => "IMPORTED_LIBNAME",
        TargetKind::ObjectLibrary => "IMPORTED_OBJECTS",
        _ => "IMPORTED_LOCATION",
    };
    let allow_implib = can_have_imported_implib(target);
    let lookup = |choice: &str| {
        let suffix = if choice.is_empty() {
            String::new()
        } else {
            format!("_{}", choice.to_ascii_uppercase())
        };
        let location = target.props.get(&format!("{location_property}{suffix}"));
        let implib = allow_implib
            .then(|| target.props.get(&format!("IMPORTED_IMPLIB{suffix}")))
            .flatten();
        (location.is_some() || implib.is_some()).then_some(ImportedConfig {
            suffix,
            location,
            implib,
        })
    };
    let mapping = target
        .props
        .get(&format!("MAP_IMPORTED_CONFIG_{desired}"))
        .map(|value| expand_list(value, true));
    let matched = if let Some(choices) = mapping {
        choices.iter().find_map(|choice| lookup(choice))
    } else {
        lookup(&desired).or_else(|| lookup("")).or_else(|| {
            expand_list(
                target
                    .props
                    .get("IMPORTED_CONFIGURATIONS")
                    .unwrap_or_default(),
                false,
            )
            .iter()
            .find_map(|choice| lookup(choice))
        })
    };
    matched.or_else(|| {
        (target.kind == TargetKind::InterfaceLibrary).then_some(ImportedConfig {
            suffix: format!("_{desired}"),
            location: None,
            implib: None,
        })
    })
}

fn import_prop<'a>(target: &'a Target, property: &str, suffix: &str) -> Option<&'a str> {
    target
        .props
        .get(&format!("{property}{suffix}"))
        .or_else(|| target.props.get(property))
}

/// Imported artifact metadata shared with the link generator.
#[derive(Debug)]
pub struct ImportInfo<'a> {
    pub suffix: String,
    pub location: &'a str,
    pub implib: &'a str,
    pub libname: &'a str,
    pub soname: &'a str,
    pub no_soname: bool,
    pub link_interface: &'a str,
    pub shared_deps: &'a str,
    pub languages: &'a str,
}

pub fn import_info<'a>(target: &'a Target, config: &str) -> Option<ImportInfo<'a>> {
    if !target.imported {
        return None;
    }
    let mapped = mapped_import_config(target, config)?;
    let prop = |name| import_prop(target, name, &mapped.suffix).unwrap_or_default();
    let interface = target.kind == TargetKind::InterfaceLibrary;
    let location = if interface {
        ""
    } else {
        mapped.location.unwrap_or_else(|| prop("IMPORTED_LOCATION"))
    };
    let implib = if can_have_imported_implib(target) {
        mapped.implib.unwrap_or_else(|| prop("IMPORTED_IMPLIB"))
    } else {
        ""
    };
    if !interface && location.is_empty() && implib.is_empty() {
        return None;
    }
    let shared = target.kind == TargetKind::SharedLibrary;
    let archive = target.kind == TargetKind::StaticLibrary;
    Some(ImportInfo {
        location,
        implib,
        libname: if interface {
            mapped.location.unwrap_or_default()
        } else {
            ""
        },
        soname: if shared { prop("IMPORTED_SONAME") } else { "" },
        no_soname: shared && is_on_opt(Some(prop("IMPORTED_NO_SONAME"))),
        link_interface: target
            .props
            .get("INTERFACE_LINK_LIBRARIES")
            .unwrap_or_else(|| {
                if interface {
                    ""
                } else {
                    prop("IMPORTED_LINK_INTERFACE_LIBRARIES")
                }
            }),
        shared_deps: if interface {
            ""
        } else {
            prop("IMPORTED_LINK_DEPENDENT_LIBRARIES")
        },
        languages: if archive {
            prop("IMPORTED_LINK_INTERFACE_LANGUAGES")
        } else {
            ""
        },
        suffix: mapped.suffix,
    })
}

fn directory_policy(ctx: &GenCtx, makefile: usize, index: usize) -> PolicyStatus {
    ctx.interp.makefiles[makefile]
        .policies
        .iter()
        .rev()
        .find_map(|frame| frame.map.get(index))
        .unwrap_or(PolicyStatus::Warn)
}

fn warning(ctx: &GenCtx, kind: MessageType, text: &str) {
    // INTEGRATION: use generation's typed diagnostic queue once available.
    if kind == MessageType::AuthorWarning {
        if ctx.interp.dev_warnings_as_errors {
            ctx.error(Vec::new(), text);
            return;
        }
        if ctx.interp.suppress_dev_warnings {
            return;
        }
    }
    eprintln!(
        "{}",
        messages::render(kind, text, &[], Some(&ctx.interp.home_source))
    );
}

fn imported_full_path(
    ctx: &GenCtx,
    target: &Target,
    config: &str,
    artifact: ArtifactType,
) -> String {
    let key = memo_key(&format!("import-path-{artifact:?}"), target, config);
    if let Some(value) = ctx.memo.borrow().get(&key) {
        return value.clone();
    }
    let mut result = None;
    if let Some(found) = mapped_import_config(target, config)
        && target.kind != TargetKind::InterfaceLibrary
    {
        let implib = || {
            can_have_imported_implib(target)
                .then(|| import_prop(target, "IMPORTED_IMPLIB", &found.suffix))
                .flatten()
        };
        result = match artifact {
            ArtifactType::Runtime => found.location.or(found.implib).or_else(|| {
                import_prop(target, "IMPORTED_LOCATION", &found.suffix)
                    .filter(|path| !path.is_empty())
                    .or_else(implib)
            }),
            ArtifactType::Import => found.implib.or_else(implib),
        };
    }
    let result = if let Some(path) = result.filter(|value| !value.is_empty()) {
        path.to_owned()
    } else {
        if target.kind != TargetKind::InterfaceLibrary {
            let property = match (target.kind, artifact) {
                (_, ArtifactType::Import) => "IMPORTED_IMPLIB",
                (TargetKind::SharedLibrary, _) => "IMPORTED_LOCATION or IMPORTED_IMPLIB",
                _ => "IMPORTED_LOCATION",
            };
            let configuration = if config.is_empty() {
                String::new()
            } else {
                format!(" configuration \"{config}\"")
            };
            let message = format!(
                "{property} not set for imported target \"{}\"{configuration}.",
                target.name
            );
            // INTEGRATION: read the target's creation-time policy and backtrace
            // from the target model when those are exposed to the generator.
            match directory_policy(ctx, target.makefile, 111) {
                PolicyStatus::New => ctx.error(
                    vec![Frame {
                        file: format!(
                            "{}/CMakeLists.txt",
                            ctx.interp.makefiles[target.makefile].source_dir
                        )
                        .into(),
                        line: 0,
                        name: None,
                        deferred: false,
                    }],
                    message,
                ),
                PolicyStatus::Warn => warning(
                    ctx,
                    MessageType::AuthorWarning,
                    &format!("{}\n{message}", policy::policy_warning(111)),
                ),
                PolicyStatus::Old => {}
            }
        }
        format!("{}-NOTFOUND", target.name)
    };
    ctx.memo.borrow_mut().insert(key, result.clone());
    result
}

/// Find a target as seen from a local generator's directory.
pub(super) fn target_in_directory<'a>(
    ctx: &GenCtx<'a>,
    makefile: usize,
    name: &str,
) -> Option<&'a Target> {
    let target = ctx.target(name)?;
    if !target.imported || target.imported_global {
        return Some(target);
    }
    let mut directory = Some(makefile);
    while let Some(index) = directory {
        if index == target.makefile {
            return Some(target);
        }
        directory = ctx.interp.makefiles[index].parent;
    }
    None
}

// INTEGRATION: Look up the SourceFile in the named directory, including the
// global GENERATED registry. For CUSTOM_COMMAND return ON when the rule is
// attached to this source (a generated output can instead be an ObjectSource).
pub(super) fn source_prop(
    _ctx: &GenCtx,
    _dir_index: usize,
    _path: &str,
    _name: &str,
) -> Option<String> {
    None
}

const LEGACY_SOURCE_EXTENSIONS: &[&str] = &[
    "c", "C", "c++", "cc", "cpp", "cxx", "cu", "mpp", "m", "M", "mm", "ixx", "cppm", "ccm", "cxxm",
    "c++m", "h", "hh", "h++", "hm", "hpp", "hxx", "in", "txx", "f", "F", "for", "f77", "f90",
    "f95", "f03", "hip", "ispc",
];

fn resolve_source(
    ctx: &GenCtx,
    makefile: usize,
    source: &str,
    property: &impl Fn(&str, &str) -> Option<String>,
) -> Option<(String, bool)> {
    let directory = &ctx.interp.makefiles[makefile];
    let source_path = collapse_full_path(source, &directory.source_dir);
    let binary_path = collapse_full_path(source, &directory.binary_dir);
    let generated = |path: &str| is_on_opt(property(path, "GENERATED").as_deref());
    if generated(source) || generated(&binary_path) {
        return Some((binary_path, true));
    }
    if generated(&source_path) {
        return Some((source_path, true));
    }

    let policy = directory_policy(ctx, makefile, 115);
    for path in [source_path, binary_path] {
        if Path::new(&path).exists() {
            return Some((path, false));
        }
        if policy != PolicyStatus::New {
            for extension in LEGACY_SOURCE_EXTENSIONS {
                let candidate = format!("{path}.{extension}");
                if generated(&candidate) || Path::new(&candidate).exists() {
                    if policy == PolicyStatus::Warn {
                        warning(
                            ctx,
                            MessageType::AuthorWarning,
                            &format!("{}\nFile:\n  {candidate}", policy::policy_warning(115)),
                        );
                    }
                    let is_generated = generated(&candidate);
                    return Some((candidate, is_generated));
                }
            }
        }
    }

    let mut error = format!("Cannot find source file:\n  {source}");
    if policy != PolicyStatus::New {
        error.push_str(&format!(
            "\nTried extensions .{}",
            LEGACY_SOURCE_EXTENSIONS.join(" .")
        ));
    }
    if source == "FILE_SET" {
        error.push_str("\nHint: the FILE_SET keyword may only appear after a visibility specifier or another FILE_SET within the target_sources() command.");
    }
    let key = format!("target:missing-source:{makefile}:{source}");
    if ctx.memo.borrow_mut().insert(key, String::new()).is_none() {
        ctx.error(Vec::new(), error);
    }
    None
}

fn source_extension(path: &str) -> &str {
    path.rsplit('/')
        .next()
        .and_then(|file| file.rsplit_once('.'))
        .map_or("", |(_, extension)| extension)
}

fn source_language(
    ctx: &GenCtx,
    path: &str,
    property: &impl Fn(&str, &str) -> Option<String>,
) -> String {
    property(path, "LANGUAGE")
        .filter(|language| !language.is_empty())
        .or_else(|| {
            ctx.interp
                .langs
                .extension_to_language
                .get(source_extension(path))
                .cloned()
        })
        .unwrap_or_default()
}

fn classify_source(
    target: &Target,
    path: &str,
    language: &str,
    file_set_kind: Option<SourceKind>,
    property: &impl Fn(&str, &str) -> Option<String>,
) -> SourceKind {
    let enabled = |name| is_on_opt(property(path, name).as_deref());
    if enabled("CUSTOM_COMMAND") {
        return SourceKind::CustomCommandOutput;
    }
    if matches!(
        target.kind,
        TargetKind::Utility | TargetKind::InterfaceLibrary
    ) {
        return SourceKind::Extra;
    }
    let extension = source_extension(path);
    if file_set_kind == Some(SourceKind::Header) || enabled("HEADER_FILE_ONLY") {
        SourceKind::Header
    } else if enabled("EXTERNAL_OBJECT") || matches!(extension, "obj" | "o" | "lo") {
        SourceKind::ExternalObject
    } else if !language.is_empty() {
        SourceKind::ObjectSource
    } else {
        // Ignored extensions have no separate kind. Known headers and
        // resources retain their classification even if the compiler ignores them.
        match extension.to_ascii_lowercase().as_str() {
            "def" => SourceKind::ModuleDefinition,
            "idl" | "resx" | "appxmanifest" | "manifest" | "pfx" | "xaml" => SourceKind::Resource,
            _ if matches!(
                extension,
                "h" | "hh" | "h++" | "hm" | "hpp" | "hxx" | "in" | "txx" | "inl"
            ) =>
            {
                SourceKind::Header
            }
            _ => SourceKind::Extra,
        }
    }
}

fn relative_from_tree(top: &str, dir: &str, path: &str) -> String {
    if !(is_subdirectory(path, dir) || (is_subdirectory(dir, top) && is_subdirectory(path, top))) {
        return path.to_owned();
    }
    let mut from = dir.split('/').peekable();
    let mut to = path.split('/').peekable();
    while from.peek().is_some() && from.peek() == to.peek() {
        from.next();
        to.next();
    }
    std::iter::repeat_n("..", from.count())
        .chain(to)
        .collect::<Vec<_>>()
        .join("/")
}

fn object_stem(ctx: &GenCtx, target: &Target, path: &str) -> String {
    let directory = &ctx.interp.makefiles[target.makefile];
    let from_source = relative_from_tree(&ctx.interp.home_source, &directory.source_dir, path);
    let from_binary = relative_from_tree(&ctx.interp.home_binary, &directory.binary_dir, path);
    let rank = |path: &str| {
        let relative = !is_full_path(path);
        (
            relative,
            relative && !path.starts_with('.'),
            std::cmp::Reverse(path.len()),
        )
    };
    let chosen = if rank(&from_binary) > rank(&from_source) {
        from_binary
    } else {
        from_source
    };
    if is_full_path(&chosen) && ctx.interp.in_try_compile {
        filename_name(path)
    } else {
        chosen
    }
}

fn object_path_max(ctx: &GenCtx, directory: usize) -> usize {
    let Some(value) = ctx
        .var(directory, "CMAKE_OBJECT_PATH_MAX")
        .filter(|value| !value.is_empty())
    else {
        return 1000;
    };
    let mut max = 0;
    let parsed = std::ffi::CString::new(value.as_str()).is_ok_and(|input| {
        // SAFETY: input is NUL terminated and %u takes a pointer to c_uint.
        unsafe { libc::sscanf(input.as_ptr(), c"%u".as_ptr(), &mut max) == 1 }
    });
    if parsed && max >= 128 {
        return max as usize;
    }
    let key = format!("target:object-max-path:{directory}");
    if ctx.memo.borrow_mut().insert(key, String::new()).is_none() {
        let why = if parsed {
            format!("{max}, which is less than the minimum of 128.")
        } else {
            format!("\"{value}\", which fails to parse as a positive integer.")
        };
        warning(
            ctx,
            MessageType::AuthorWarning,
            &format!("CMAKE_OBJECT_PATH_MAX is set to {why}  The value will be ignored."),
        );
    }
    1000
}

fn shorten_object_name(name: &mut String, directory_len: usize, limit: usize) -> bool {
    if directory_len >= limit {
        return false;
    }
    let available = limit - directory_len;
    if name.len() <= available {
        return true;
    }
    let cutoff = name.len() - available + 32;
    let slash = name
        .bytes()
        .enumerate()
        .skip(cutoff)
        .find(|&(_, byte)| byte == b'/')
        .or_else(|| {
            name.bytes()
                .enumerate()
                .take(cutoff.saturating_add(1))
                .rfind(|&(_, byte)| byte == b'/')
        })
        .map(|(index, _)| index);
    let Some(slash) = slash.filter(|&index| index >= cutoff || index > 32) else {
        return false;
    };
    *name = format!(
        "{:x}{}",
        Md5::digest(&name.as_bytes()[..slash]),
        &name[slash..]
    );
    slash >= cutoff
}

fn cuda_object_extension(ctx: &GenCtx, target: &Target) -> Option<&'static str> {
    if ctx
        .var_safe(target.makefile, "CMAKE_CUDA_COMPILER_ID")
        .is_empty()
    {
        return None;
    }
    [
        ("CUDA_PTX_COMPILATION", ".ptx"),
        ("CUDA_CUBIN_COMPILATION", ".cubin"),
        ("CUDA_FATBIN_COMPILATION", ".fatbin"),
        ("CUDA_OPTIX_COMPILATION", ".optixir"),
    ]
    .into_iter()
    .find_map(|(property, extension)| is_on_opt(target.props.get(property)).then_some(extension))
}

fn object_file_name(
    ctx: &GenCtx,
    target: &Target,
    config: &str,
    path: &str,
    language: &str,
    property: &impl Fn(&str, &str) -> Option<String>,
) -> String {
    let mut name = object_stem(ctx, target, path);
    let pch = property(path, "PCH_EXTENSION");
    if (pch.is_some() || name.contains("cmake_pch"))
        && let Some(start) = name.find("CMakeFiles/")
        && let Some(end) = name[start + "CMakeFiles/".len()..].find('/')
    {
        let end = start + "CMakeFiles/".len() + end;
        if name[..end].ends_with(".dir") {
            name.replace_range(start..=end, "");
        }
    }
    let custom_extension = pch
        .as_deref()
        .or_else(|| cuda_object_extension(ctx, target));
    if !is_on_opt(property(path, "KEEP_EXTENSION").as_deref()) {
        let variable = format!("CMAKE_{language}_OUTPUT_EXTENSION_REPLACE");
        if (custom_extension.is_some() || is_on_opt(ctx.var(target.makefile, &variable).as_deref()))
            && let Some(dot) = name.rfind('.')
        {
            name.truncate(dot);
        }
        let extension = custom_extension.or_else(|| {
            ctx.interp
                .langs
                .output_extension
                .get(language)
                .map(String::as_str)
        });
        if let Some(extension) = extension {
            name.push_str(extension);
        }
    }

    let prefix = format!("target:object-name:{}:", target.makefile);
    let key = format!("{prefix}{name}");
    if let Some(cached) = ctx.memo.borrow().get(&key) {
        return cached.clone();
    }
    name = name
        .trim_start_matches('/')
        .replace([':', ' '], "_")
        .replace("../", "__/");
    if is_on_opt(
        ctx.var(target.makefile, "CMAKE_MANGLE_OBJECT_FILE_NAMES")
            .as_deref(),
    ) {
        name = name.replace('+', "_p_");
        let mut index = 0;
        while name.contains("_p_")
            && ctx
                .memo
                .borrow()
                .iter()
                .any(|(key, existing)| key.starts_with(&prefix) && *existing == name)
        {
            name = name.replace("_p_", &format!("_p{index}_"));
            index += 1;
        }
    }

    let directory = object_directory(ctx, target, config);
    let max = object_path_max(ctx, target.makefile);
    if !shorten_object_name(&mut name, directory.len(), max) {
        let warning_key = format!("target:object-path-violation:{directory}");
        if ctx
            .memo
            .borrow_mut()
            .insert(warning_key, String::new())
            .is_none()
        {
            warning(
                ctx,
                MessageType::Warning,
                &format!(
                    "The object file directory\n  {directory}\nhas {} characters.  The maximum full path to an object file is {max} characters (see CMAKE_OBJECT_PATH_MAX).  Object file\n  {name}\ncannot be safely placed under this directory.  The build may not work correctly.",
                    directory.len()
                ),
            );
        }
    }
    ctx.memo.borrow_mut().insert(key, name.clone());
    name
}

/// Default link discovery until the generator's compile/link usage closures
/// are available. The public *_with_link_closure queries accept those closures.
fn link_closure<'a>(ctx: &GenCtx<'a>, target: &Target, config: &str) -> Vec<&'a Target> {
    let mut seen = HashSet::from([(target.makefile, target.name.as_str())]);
    let mut pending = evaluated_links(ctx, target, target, "LINK_LIBRARIES")
        .into_iter()
        .filter_map(|name| target_in_directory(ctx, target.makefile, &name))
        .rev()
        .collect::<Vec<_>>();
    let mut closure = Vec::new();
    while let Some(dep) = pending.pop() {
        if !seen.insert((dep.makefile, &dep.name)) {
            continue;
        }
        closure.push(dep);
        let property = if dep.imported {
            import_info(dep, config).map_or("", |info| info.link_interface)
        } else {
            dep.props
                .get("INTERFACE_LINK_LIBRARIES")
                .unwrap_or_default()
        };
        pending.extend(
            expand_list(&ctx.genex(property, Some(target), None), false)
                .into_iter()
                .rev()
                .filter_map(|name| target_in_directory(ctx, dep.makefile, &name)),
        );
    }
    closure
}

fn evaluated_links(ctx: &GenCtx, provider: &Target, head: &Target, property: &str) -> Vec<String> {
    expand_list(
        &ctx.genex(
            provider.props.get(property).unwrap_or_default(),
            Some(head),
            None,
        ),
        false,
    )
}

fn file_sets(ctx: &GenCtx, target: &Target) -> Vec<(String, bool)> {
    let mut files = Vec::new();
    let source_dir = &ctx.interp.makefiles[target.makefile].source_dir;
    // INTEGRATION: project the model's private/public cmFileSet entries onto
    // these target properties, or supply them here directly.
    for (names, property, bases, default, header) in [
        ("HEADER_SETS", "HEADER_SET", "HEADER_DIRS", "HEADERS", true),
        (
            "CXX_MODULE_SETS",
            "CXX_MODULE_SET",
            "CXX_MODULE_DIRS",
            "CXX_MODULES",
            false,
        ),
    ] {
        for name in expand_list(target.props.get(names).unwrap_or_default(), false) {
            let suffix = if name == default {
                String::new()
            } else {
                format!("_{name}")
            };
            let text = target
                .props
                .get(&format!("{property}{suffix}"))
                .unwrap_or_default();
            let base_dirs = target
                .props
                .get(&format!("{bases}{suffix}"))
                .unwrap_or(source_dir);
            let mut directories = Vec::<String>::new();
            for dir in expand_list(&ctx.genex(base_dirs, Some(target), None), false) {
                let dir = collapse_full_path(&dir, source_dir);
                if let Some(prior) = directories
                    .iter()
                    .find(|prior| is_subdirectory(&dir, prior) || is_subdirectory(prior, &dir))
                {
                    ctx.error(Vec::new(), format!(
                        "Base directories in file set cannot be subdirectories of each other:\n  {prior}\n  {dir}"
                    ));
                    break;
                }
                directories.push(dir);
            }
            for file in expand_list(&ctx.genex(text, Some(target), None), false) {
                let file = collapse_full_path(&file, source_dir);
                if directories.iter().any(|dir| is_subdirectory(&file, dir)) {
                    files.push((file, header));
                } else {
                    let mut error = format!(
                        "File:\n  {file}\nmust be in one of the file set's base directories:"
                    );
                    for dir in &directories {
                        error.push_str("\n  ");
                        error.push_str(dir);
                    }
                    ctx.error(Vec::new(), error);
                }
            }
        }
    }
    files
}

/// Get sources in CMake order using the compile usage-requirement closure.
pub fn sources_with_link_closure(
    ctx: &GenCtx,
    target: &Target,
    config: &str,
    compile_closure: &[&Target],
) -> Vec<SourceInfo> {
    collect_sources(ctx, target, config, compile_closure, &|path, name| {
        source_prop(ctx, target.makefile, path, name)
    })
}

fn collect_sources(
    ctx: &GenCtx,
    target: &Target,
    config: &str,
    compile_closure: &[&Target],
    property: &impl Fn(&str, &str) -> Option<String>,
) -> Vec<SourceInfo> {
    let mut listed = evaluated_links(ctx, target, target, "SOURCES");
    for provider in compile_closure {
        for source in evaluated_links(ctx, provider, target, "INTERFACE_SOURCES") {
            if is_full_path(&source) {
                listed.push(source);
            } else {
                ctx.error(Vec::new(), format!(
                    "Target \"{}\" contains relative path in its INTERFACE_SOURCES:\n  \"{source}\"",
                    provider.name
                ));
            }
        }
    }

    let sets = file_sets(ctx, target);
    let mut header_set = HashSet::new();
    for (file, header) in sets {
        if header {
            header_set.insert(file.clone());
        }
        listed.push(file);
    }

    let mut found = Vec::new();
    let mut seen = HashSet::new();
    let mut bad_objects = Vec::new();
    let mut unity_objects = false;
    for source in listed {
        let Some((path, generated)) = resolve_source(ctx, target.makefile, &source, property)
        else {
            continue;
        };
        if !seen.insert(path.clone()) {
            continue;
        }
        let language = source_language(ctx, &path, property);
        let set_kind = header_set.contains(&path).then_some(SourceKind::Header);
        let kind = classify_source(target, &path, &language, set_kind, property);
        if target.kind == TargetKind::ObjectLibrary
            && (kind == SourceKind::ModuleDefinition
                || source_extension(&path).eq_ignore_ascii_case("idl"))
        {
            bad_objects.push(source);
        }
        let object_name = if kind == SourceKind::ObjectSource {
            unity_objects |= !is_on_opt(property(&path, "SKIP_UNITY_BUILD_INCLUSION").as_deref());
            object_file_name(ctx, target, config, &path, &language, property)
        } else {
            String::new()
        };
        found.push(SourceInfo {
            path,
            language,
            kind,
            object_name,
            generated,
        });
    }
    if !bad_objects.is_empty() {
        ctx.error(Vec::new(), format!(
            "OBJECT library \"{}\" contains:\n  {}\nbut may contain only sources that compile, header files, and other files that would not affect linking of a normal library.",
            target.name, bad_objects.join("\n  ")
        ));
    }
    if unity_objects && is_on_opt(target.props.get("UNITY_BUILD")) {
        let key = memo_key("unity-build", target, config);
        if ctx.memo.borrow_mut().insert(key, String::new()).is_none() {
            ctx.error(
                Vec::new(),
                format!(
                    "UNITY_BUILD is not supported for target \"{}\".",
                    target.name
                ),
            );
        }
    }
    found
}

/// `GetSourceFiles` and `ComputeObjectMapping`.
pub fn sources(ctx: &GenCtx, target: &Target, config: &str) -> Vec<SourceInfo> {
    sources_with_link_closure(ctx, target, config, &link_closure(ctx, target, config))
}

fn select_linker_language(
    ctx: &GenCtx,
    target: &Target,
    config: &str,
    sources: &[SourceInfo],
    link_closure: &[&Target],
) -> String {
    let mut languages: BTreeSet<_> = sources
        .iter()
        .filter(|source| !source.language.is_empty())
        .map(|source| source.language.clone())
        .collect();
    for provider in link_closure {
        if !matches!(
            provider.kind,
            TargetKind::StaticLibrary | TargetKind::ObjectLibrary
        ) {
            continue;
        }
        let propagated = if provider.imported {
            import_info(provider, config)
                .map(|import| expand_list(import.languages, false))
                .unwrap_or_default()
        } else {
            let property = |path: &str, name: &str| source_prop(ctx, provider.makefile, path, name);
            collect_sources(ctx, provider, config, &[], &property)
                .into_iter()
                .filter(|source| !source.language.is_empty())
                .map(|source| source.language)
                .collect()
        };
        for language in propagated {
            let variable = format!("CMAKE_{language}_LINKER_PREFERENCE_PROPAGATES");
            if provider.kind == TargetKind::ObjectLibrary
                || is_on_opt(ctx.var(target.makefile, &variable).as_deref())
            {
                languages.insert(language);
            }
        }
    }
    let preference = |language: &String| {
        ctx.interp
            .langs
            .linker_preference
            .get(language)
            .copied()
            .unwrap_or(0)
    };
    let best = languages.iter().map(preference).max().unwrap_or(0).max(0);
    let best_languages = languages
        .into_iter()
        .filter(|language| preference(language) == best)
        .collect::<Vec<_>>();
    if best_languages.len() > 1 {
        ctx.error(Vec::new(), format!(
            "Target {} contains multiple languages with the highest linker preference ({best}):\n  {}\nSet the LINKER_LANGUAGE property for this target.",
            target.name, best_languages.join("\n  ")
        ));
    }
    best_languages.into_iter().next().unwrap_or_default()
}

fn fixed_linker_language(target: &Target) -> Option<&str> {
    if target.props.contains("HAS_CXX") {
        Some("CXX")
    } else {
        target
            .props
            .get("LINKER_LANGUAGE")
            .filter(|language| !language.is_empty())
    }
}

/// Select a linker using the supplied evaluated sources and link usage closure.
pub fn linker_language_with_link_closure(
    ctx: &GenCtx,
    target: &Target,
    config: &str,
    sources: &[SourceInfo],
    link_closure: &[&Target],
) -> String {
    fixed_linker_language(target).map_or_else(
        || select_linker_language(ctx, target, config, sources, link_closure),
        str::to_owned,
    )
}

fn compute_linker_language(ctx: &GenCtx, target: &Target, config: &str) -> String {
    if let Some(language) = fixed_linker_language(target) {
        return language.to_owned();
    }
    if target.imported
        || matches!(
            target.kind,
            TargetKind::Utility | TargetKind::InterfaceLibrary | TargetKind::GlobalTarget
        )
    {
        return String::new();
    }
    let key = memo_key("linker-language", target, config);
    if let Some(value) = ctx.memo.borrow().get(&key) {
        return value.clone();
    }
    // Expose the first-pass language to a LINK_LANGUAGE expression during the
    // second pass. Most targets require only the first pass.
    ctx.memo.borrow_mut().insert(key.clone(), String::new());
    let compute = || {
        let closure = link_closure(ctx, target, config);
        let sources = sources_with_link_closure(ctx, target, config, &closure);
        let sensitive = std::iter::once(target)
            .chain(closure.iter().copied())
            .any(|provider| {
                ["LINK_LIBRARIES", "INTERFACE_LINK_LIBRARIES"]
                    .into_iter()
                    .any(|prop| {
                        provider.props.get(prop).is_some_and(|value| {
                            value.contains("$<LINK_LANGUAGE:")
                                || value.contains("$<LINK_LANG_AND_ID:")
                        })
                    })
            });
        (
            select_linker_language(ctx, target, config, &sources, &closure),
            sensitive,
        )
    };
    let in_progress = memo_key("linker-language-in-progress", target, config);
    ctx.memo
        .borrow_mut()
        .insert(in_progress.clone(), String::new());
    let (language, sensitive) = compute();
    ctx.memo.borrow_mut().insert(key, language.clone());
    if sensitive {
        let (second, _) = compute();
        if second != language {
            ctx.error(Vec::new(), format!(
                "Evaluation of $<LINK_LANGUAGE:...> or $<LINK_LAND_AND_ID:...> changes\nthe linker language for target \"{}\" (from '{language}' to '{second}') which is invalid.",
                target.name
            ));
        }
    }
    ctx.memo.borrow_mut().remove(&in_progress);
    language
}

/// `cmGeneratorTarget::GetLinkerLanguage(config)` (empty if none).
pub fn linker_language(ctx: &GenCtx, target: &Target, config: &str) -> String {
    let language = compute_linker_language(ctx, target, config);
    if language.is_empty()
        && !ctx
            .memo
            .borrow()
            .contains_key(&memo_key("linker-language-in-progress", target, config))
        && !target.imported
        && matches!(
            target.kind,
            TargetKind::Executable
                | TargetKind::SharedLibrary
                | TargetKind::ModuleLibrary
                | TargetKind::StaticLibrary
        )
    {
        let key = memo_key("missing-linker-language", target, config);
        if ctx.memo.borrow_mut().insert(key, String::new()).is_none() {
            ctx.error(
                Vec::new(),
                format!(
                    "CMake can not determine linker language for target: {}",
                    target.name
                ),
            );
        }
    }
    language
}

#[cfg(test)]
pub(super) mod tests {
    use std::fs;

    use serde_json::Value;

    use super::*;
    use crate::interp::{Interp, Makefile, Mode};
    use crate::props::Props;

    pub(in crate::generator) fn test_target(
        name: &str,
        kind: TargetKind,
        makefile: usize,
        properties: &[(&str, &str)],
    ) -> Target {
        let mut props = Props::default();
        for &(name, value) in properties {
            props.set(name, value.to_owned());
        }
        Target {
            name: name.to_owned(),
            kind,
            makefile,
            imported: false,
            imported_global: false,
            alias_of: None,
            props,
            provenance: Default::default(),
            backtrace: Vec::new(),
        }
    }

    pub(in crate::generator) fn oracle_fixture() -> Value {
        serde_json::from_str(include_str!(
            "../../tests/data/generator/target/linux-ninja.json"
        ))
        .unwrap()
    }

    pub(in crate::generator) fn fixture_interp(root: &str) -> Interp {
        let mut interp = Interp::new(
            Mode::Project,
            &format!("{root}/src"),
            &format!("{root}/build"),
            "",
        );
        interp.set_def("CMAKE_BUILD_TYPE", "RelWithDebInfo");
        interp.record_directory_vars();
        interp.makefiles.push(Makefile {
            source_dir: format!("{root}/src/nested"),
            binary_dir: format!("{root}/build/nested"),
            parent: Some(0),
            props: Props::default(),
            policies: interp.makefiles[0].policies.clone(),
            list_files: Vec::new(),
            targets: Vec::new(),
            children: Vec::new(),
            exclude_from_all: false,
            deferred: Vec::new(),
            defer_counter: 0,
        });
        interp.dir_final_vars.push(
            [
                ("CMAKE_OBJECT_PATH_MAX", "128"),
                ("CMAKE_STATIC_LIBRARY_PREFIX", "lib"),
                ("CMAKE_STATIC_LIBRARY_SUFFIX", ".a"),
                ("CMAKE_STATIC_LIBRARY_PREFIX_C", "lang-"),
                ("CMAKE_STATIC_LIBRARY_SUFFIX_C", ".arc"),
                ("CMAKE_SHARED_LIBRARY_PREFIX", "lib"),
                ("CMAKE_SHARED_LIBRARY_SUFFIX", ".so"),
                ("CMAKE_SHARED_LIBRARY_SONAME_C_FLAG", "-Wl,-soname,"),
                ("LIBRARY_OUTPUT_PATH", "oldlib"),
                ("EXECUTABLE_OUTPUT_PATH", "oldbin"),
            ]
            .into_iter()
            .map(|(key, value)| (key.to_owned(), Some(value.to_owned())))
            .collect(),
        );
        interp
            .langs
            .extension_to_language
            .insert("c".to_owned(), "C".to_owned());
        interp
            .langs
            .output_extension
            .insert("C".to_owned(), ".o".to_owned());
        interp.langs.linker_preference.insert("C".to_owned(), 10);
        interp
    }

    #[test]
    fn oracle_ninja_artifacts_and_objects() {
        let fixture = oracle_fixture();
        let root = std::env::temp_dir().join(format!("cmake-rust-objects-{}", std::process::id()));
        let root_str = root.to_str().unwrap();
        let long_source = fixture["objects"]["long"][0]["source"].as_str().unwrap();
        for path in [
            "src/nested/local.c",
            "src/common.c",
            "src/nested/asset.data",
            long_source,
        ] {
            let path = root.join(path);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, "").unwrap();
        }
        let mut interp = fixture_interp(root_str);
        interp.makefiles[1].policies[0]
            .map
            .set(115, PolicyStatus::New);
        let ctx = GenCtx::new(&interp);

        let cases: &[(&str, TargetKind, &[(&str, &str)])] = &[
            (
                "statik",
                TargetKind::StaticLibrary,
                &[
                    ("SOURCES", "local.c;../common.c;gen/generated.c"),
                    ("HEADER_SETS", "HEADERS"),
                    ("HEADER_SET", "asset.data"),
                    ("OUTPUT_NAME_RELWITHDEBINFO", "ignored"),
                    ("ARCHIVE_OUTPUT_NAME", "core"),
                    ("RELWITHDEBINFO_POSTFIX", "_rw"),
                ],
            ),
            (
                "shared",
                TargetKind::SharedLibrary,
                &[
                    ("LIBRARY_OUTPUT_NAME_RELWITHDEBINFO", "actual"),
                    ("OUTPUT_NAME", "alternate"),
                    ("PREFIX", "pre-"),
                    ("SUFFIX", ".plugin"),
                    ("VERSION", "3.4"),
                    ("SOVERSION", "3"),
                    ("LIBRARY_OUTPUT_DIRECTORY", "ignored"),
                    ("LIBRARY_OUTPUT_DIRECTORY_RELWITHDEBINFO", "../products"),
                ],
            ),
            ("only_so", TargetKind::SharedLibrary, &[("SOVERSION", "8")]),
            (
                "no_so",
                TargetKind::SharedLibrary,
                &[("VERSION", "3.4"), ("NO_SONAME", "TRUE")],
            ),
            (
                "tool",
                TargetKind::Executable,
                &[
                    ("VERSION", "2"),
                    ("OUTPUT_NAME_RELWITHDEBINFO", "runner"),
                    ("SUFFIX", ".bin"),
                ],
            ),
            (
                "gendir",
                TargetKind::Executable,
                &[("RUNTIME_OUTPUT_DIRECTORY", "bin/$<LOWER_CASE:$<CONFIG>>")],
            ),
            ("long", TargetKind::StaticLibrary, &[]),
        ];
        for &(name, kind, properties) in cases {
            let mut target = test_target(name, kind, 1, properties);
            if name == "long" {
                target
                    .props
                    .set("SOURCES", format!("{root_str}/{long_source}"));
            } else if name != "statik" {
                target.props.set("SOURCES", "local.c".to_owned());
            }
            if name == "gendir" {
                // The production call uses GenCtx::genex. Supply its result
                // here while the generator-expression engine is integrated.
                output_directory(&ctx, &target, &ctx.config, ArtifactType::Runtime, |text| {
                    assert_eq!(text, "bin/$<LOWER_CASE:$<CONFIG>>");
                    "bin/relwithdebinfo".to_owned()
                });
            }
            if name == "statik" {
                // The production accessor will obtain this property from SourceFile.
                let source_info =
                    collect_sources(&ctx, &target, &ctx.config, &[], &|path, property| {
                        (property == "GENERATED" && path == "gen/generated.c")
                            .then(|| "1".to_owned())
                    });
                assert_eq!(source_info[3].kind, SourceKind::Header);
                assert!(source_info[2].generated);
                assert_objects(&ctx, &target, source_info, &fixture, root_str);
                fs::create_dir_all(root.join("build/nested/gen")).unwrap();
                fs::write(root.join("build/nested/gen/generated.c"), "").unwrap();
            } else {
                assert_objects(
                    &ctx,
                    &target,
                    sources(&ctx, &target, &ctx.config),
                    &fixture,
                    root_str,
                );
            }
            assert_eq!(linker_language(&ctx, &target, &ctx.config), "C");
            let artifact = &fixture["artifacts"][name];
            for (field, real) in [("real", true), ("output", false)] {
                let path = full_path(&ctx, &target, &ctx.config, ArtifactType::Runtime, real);
                let path = path.strip_prefix(&format!("{root_str}/build/")).unwrap();
                assert_eq!(path, artifact[field].as_str().unwrap(), "target {name}");
            }
            if let Some(soname) = artifact["soname"].as_str() {
                assert_eq!(names(&ctx, &target, &ctx.config).shared_object, soname);
            }
        }
        assert!(ctx.errors.borrow().is_empty(), "{:?}", ctx.errors.borrow());
        fs::remove_dir_all(root).unwrap();
    }

    fn assert_objects(
        ctx: &GenCtx,
        target: &Target,
        result: Vec<SourceInfo>,
        fixture: &Value,
        root: &str,
    ) {
        let directory = object_directory(ctx, target, &ctx.config);
        let object_dir = directory.strip_prefix(&format!("{root}/build/")).unwrap();
        let objects: Vec<_> = result
            .into_iter()
            .filter(|source| source.kind == SourceKind::ObjectSource)
            .collect();
        let expected = fixture["objects"][&target.name].as_array().unwrap();
        assert_eq!(objects.len(), expected.len(), "target {}", target.name);
        for (source, expected) in objects.iter().zip(expected) {
            assert_eq!(
                format!("{object_dir}{}", source.object_name),
                expected["object"].as_str().unwrap()
            );
            assert_eq!(
                source.path.strip_prefix(&format!("{root}/")).unwrap(),
                expected["source"].as_str().unwrap()
            );
        }
    }

    #[test]
    fn oracle_import_config_and_missing_location() {
        let mut interp = fixture_interp("/project");
        interp.makefiles[0].policies[0]
            .map
            .set(111, PolicyStatus::New);
        let ctx = GenCtx::new(&interp);
        let mut imported = test_target(
            "dependency",
            TargetKind::StaticLibrary,
            0,
            &[
                ("IMPORTED_CONFIGURATIONS", "DEBUG;RELEASE"),
                ("IMPORTED_LOCATION_DEBUG", "/project/src/import-debug.a"),
                ("IMPORTED_LOCATION_RELEASE", "/project/src/import-release.a"),
                ("IMPORTED_LOCATION", "/project/src/import-generic.a"),
                ("MAP_IMPORTED_CONFIG_RELWITHDEBINFO", "missing;release"),
            ],
        );
        imported.imported = true;
        let fixture = oracle_fixture();
        assert_eq!(
            full_path(&ctx, &imported, &ctx.config, ArtifactType::Runtime, true),
            format!(
                "/project/{}",
                fixture["imported_location"].as_str().unwrap()
            )
        );
        assert_eq!(
            full_name(&ctx, &imported, "Debug", ArtifactType::Runtime),
            "import-debug.a"
        );
        assert_eq!(
            full_name(&ctx, &imported, "MinSizeRel", ArtifactType::Runtime),
            "import-generic.a"
        );
        imported.props.remove("IMPORTED_LOCATION");
        assert_eq!(
            full_name(&ctx, &imported, "", ArtifactType::Runtime),
            "import-debug.a"
        );
        imported
            .props
            .set("MAP_IMPORTED_CONFIG_RELEASE", String::new());
        assert!(mapped_import_config(&imported, "Release").is_none());

        let fixture: Value = serde_json::from_str(include_str!(
            "../../tests/data/generator/target/import-errors.json"
        ))
        .unwrap();
        for (name, kind) in [
            ("missing_shared", TargetKind::SharedLibrary),
            ("missing_static", TargetKind::StaticLibrary),
        ] {
            let mut missing = test_target(name, kind, 0, &[]);
            missing.imported = true;
            if kind == TargetKind::SharedLibrary {
                missing
                    .props
                    .set("MAP_IMPORTED_CONFIG_DEBUG", "MISSING".to_owned());
                missing
                    .props
                    .set("IMPORTED_LOCATION_RELEASE", "/fallback.so".to_owned());
            }
            assert_eq!(
                full_name(&ctx, &missing, "Debug", ArtifactType::Runtime),
                format!("{name}-NOTFOUND")
            );
            assert_eq!(
                ctx.errors.borrow().last().unwrap().1,
                fixture[name].as_str().unwrap()
            );
        }

        let mut shared = test_target(
            "shim",
            TargetKind::SharedLibrary,
            0,
            &[
                ("IMPORTED_IMPLIB_RELEASE", "/sdk/libshim.so"),
                ("IMPORTED_SONAME_RELEASE", "libshim.so.4"),
            ],
        );
        shared.imported = true;
        let info = import_info(&shared, "Release").unwrap();
        assert_eq!(info.soname, "libshim.so.4");
        assert_eq!(info.suffix, "_RELEASE");
        for artifact in [ArtifactType::Runtime, ArtifactType::Import] {
            assert_eq!(
                full_path(&ctx, &shared, "Release", artifact, true),
                "/sdk/libshim.so"
            );
        }
    }

    #[test]
    fn source_properties_and_link_language() {
        let mut interp = fixture_interp("/project");
        let vars = &mut interp.dir_final_vars[1];
        vars.insert(
            "CMAKE_CXX_OUTPUT_EXTENSION_REPLACE".to_owned(),
            Some("ON".to_owned()),
        );
        vars.insert(
            "CMAKE_CXX_LINKER_PREFERENCE_PROPAGATES".to_owned(),
            Some("ON".to_owned()),
        );
        interp
            .langs
            .output_extension
            .insert("CXX".to_owned(), ".o".to_owned());
        interp.langs.linker_preference.insert("CXX".to_owned(), 30);
        let ctx = GenCtx::new(&interp);
        let interface = test_target(
            "headers",
            TargetKind::InterfaceLibrary,
            1,
            &[("INTERFACE_SOURCES", "/project/build/nested/shared.h")],
        );
        let mut dep = test_target(
            "cpp",
            TargetKind::StaticLibrary,
            1,
            &[
                ("IMPORTED_LOCATION", "/project/libcpp.a"),
                ("IMPORTED_LINK_INTERFACE_LANGUAGES", "CXX"),
            ],
        );
        dep.imported = true;
        let mut consumer = test_target(
            "consumer",
            TargetKind::Executable,
            1,
            &[(
                "SOURCES",
                "foo.data;ext.o;symbols.def;attached.rule;ignored.c",
            )],
        );
        let properties = |path: &str, prop: &str| match (path.rsplit('/').next().unwrap(), prop) {
            (_, "GENERATED")
            | ("attached.rule", "CUSTOM_COMMAND")
            | ("ignored.c", "HEADER_FILE_ONLY") => Some("ON".to_owned()),
            ("foo.data", "LANGUAGE") => Some("CXX".to_owned()),
            _ => None,
        };
        let result = collect_sources(
            &ctx,
            &consumer,
            &ctx.config,
            &[&interface, &dep],
            &properties,
        );
        let kinds: Vec<_> = result.iter().map(|source| source.kind).collect();
        assert_eq!(
            kinds,
            [
                SourceKind::ObjectSource,
                SourceKind::ExternalObject,
                SourceKind::ModuleDefinition,
                SourceKind::CustomCommandOutput,
                SourceKind::Header,
                SourceKind::Header,
            ]
        );
        assert_eq!(result[0].object_name, "foo.o");
        assert_eq!(
            linker_language_with_link_closure(&ctx, &consumer, &ctx.config, &result[1..], &[&dep]),
            "CXX"
        );
        consumer.props.set("LINKER_LANGUAGE", "C".to_owned());
        assert_eq!(
            linker_language_with_link_closure(&ctx, &consumer, &ctx.config, &result, &[&dep]),
            "C"
        );
        assert!(ctx.errors.borrow().is_empty());
    }
}
