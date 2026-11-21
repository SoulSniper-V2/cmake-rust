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
    /// Source without a build action.
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
    /// For an object file of a linked OBJECT library
    /// (`cmSourceFile::GetObjectLibrary`), that library's name.
    pub object_library: Option<String>,
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
    // The import library names (GetLibraryNames): ComputeVersionedName of the
    // import output with the effective SOVERSION.
    let import_library = if matches!(target.kind, TargetKind::SharedLibrary | TargetKind::ModuleLibrary) {
        let import_output = full_name_components(ctx, target, config, ArtifactType::Import).filename();
        let versioned = has_soname(ctx, target, config)
            && !is_on_opt(ctx.var(target.makefile, "CMAKE_PLATFORM_NO_VERSIONED_SONAME").as_deref());
        let soversion = target.props.get("SOVERSION").or_else(|| target.props.get("VERSION"));
        match soversion.filter(|_| versioned) {
            Some(soversion) => format!("{import_output}.{soversion}"),
            None => import_output,
        }
    } else {
        String::new()
    };
    Names {
        base: parts.base,
        output,
        real,
        import_library,
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

/// `cmGeneratorTarget::ComputePDBOutputDir(kind, config, out)`: `None`
/// when no `<kind>_OUTPUT_DIRECTORY[_<CONFIG>]` property selects one.
fn pdb_output_dir(ctx: &GenCtx, target: &Target, config: &str, kind: &str) -> Option<String> {
    let config_upper = config.to_ascii_uppercase();
    let out = if let Some(value) = target.props.get(&format!("{kind}_OUTPUT_DIRECTORY_{config_upper}")) {
        ctx.genex(value, None, None)
    } else if let Some(value) = target.props.get(&format!("{kind}_OUTPUT_DIRECTORY")) {
        ctx.genex(value, None, None)
    } else {
        String::new()
    };
    if out.is_empty() {
        return None;
    }
    // AppendDirectoryForConfig is a no-op for the single-config Ninja generator.
    Some(collapse_full_path(&out, &ctx.interp.makefiles[target.makefile].binary_dir))
}

/// `cmGeneratorTarget::GetPDBDirectory`.
pub fn pdb_directory(ctx: &GenCtx, target: &Target, config: &str) -> String {
    pdb_output_dir(ctx, target, config, "PDB")
        .unwrap_or_else(|| directory(ctx, target, config, ArtifactType::Runtime))
}

/// `cmGeneratorTarget::GetCompilePDBName`.
pub fn compile_pdb_name(ctx: &GenCtx, target: &Target, config: &str) -> String {
    let per_config = format!("COMPILE_PDB_NAME_{}", config.to_ascii_uppercase());
    let name = target
        .props
        .get(&per_config)
        .filter(|value| !value.is_empty())
        .or_else(|| target.props.get("COMPILE_PDB_NAME").filter(|value| !value.is_empty()));
    match name {
        Some(name) => {
            let pdb = ctx.genex(name, Some(target), None);
            let prefix = full_name_components(ctx, target, config, ArtifactType::Runtime).prefix;
            format!("{prefix}{pdb}.pdb")
        }
        None => String::new(),
    }
}

/// `cmGeneratorTarget::GetCompilePDBPath`.
pub fn compile_pdb_path(ctx: &GenCtx, target: &Target, config: &str) -> String {
    let mut dir = pdb_output_dir(ctx, target, config, "COMPILE_PDB").unwrap_or_default();
    let name = compile_pdb_name(ctx, target, config);
    // HaveWellDefinedOutputFiles: true for every non-imported buildable
    // target type that reaches here.
    if dir.is_empty() && !name.is_empty() {
        dir = pdb_directory(ctx, target, config);
    }
    if !dir.is_empty() {
        dir.push('/');
    }
    dir + &name
}

/// `cmGeneratorTarget::GetSupportDirectory`.
pub fn support_directory(ctx: &GenCtx, target: &Target) -> String {
    object_directory(ctx, target, "").trim_end_matches('/').to_owned()
}

/// `cmCommonTargetGenerator::ComputeTargetCompilePDB` (single-config).
pub fn target_compile_pdb(ctx: &GenCtx, target: &Target, config: &str) -> String {
    if !matches!(
        target.kind,
        TargetKind::Executable
            | TargetKind::StaticLibrary
            | TargetKind::SharedLibrary
            | TargetKind::ModuleLibrary
            | TargetKind::ObjectLibrary
    ) {
        return String::new();
    }
    let path = compile_pdb_path(ctx, target, config);
    if !path.is_empty() {
        return path;
    }
    let mut path = support_directory(ctx, target);
    path.push('/');
    if target.kind == TargetKind::StaticLibrary {
        path.push_str(&target.name);
        path.push_str(".pdb");
    }
    path
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

#[allow(dead_code, reason = "target query for generator consumers")]
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
#[allow(
    dead_code,
    reason = "import metadata shared with link and genex consumers"
)]
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

/// `CM_FOR_EACH_TARGET_POLICY`: policies recorded on the target when it is
/// created (cmTarget's PolicyMap).
const TARGET_POLICIES: &[usize] = &[3, 4, 8, 20, 21, 22, 27, 37, 38, 41, 42, 46, 52, 60, 63, 65, 68, 69, 73, 76, 81, 83, 95, 99, 104, 105, 108, 112, 113, 119, 131, 142, 154, 155, 156, 157, 160, 162, 179, 181, 182, 195];

/// `cmGeneratorTarget::GetPolicyStatus*`: target policies come from the
/// target's creation-time snapshot; others from its directory's final state.
pub fn target_policy(ctx: &GenCtx, target: &Target, index: usize) -> PolicyStatus {
    if crate::policy_table::POLICIES[index].removed {
        return PolicyStatus::New;
    }
    if TARGET_POLICIES.contains(&index) {
        return target.policies.get(index).unwrap_or(PolicyStatus::Warn);
    }
    directory_policy(ctx, target.makefile, index)
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

/// The non-diagnosing part of `cmTarget::ImportedGetFullPath` (shared by
/// configure-time LOCATION reads and generation).
pub fn imported_artifact_path<'a>(target: &'a Target, config: &str, artifact: ArtifactType) -> Option<&'a str> {
    if target.kind == TargetKind::InterfaceLibrary {
        return None;
    }
    let found = mapped_import_config(target, config)?;
    let implib = || {
        can_have_imported_implib(target)
            .then(|| import_prop(target, "IMPORTED_IMPLIB", &found.suffix))
            .flatten()
    };
    match artifact {
        ArtifactType::Runtime => found.location.or(found.implib).or_else(|| {
            import_prop(target, "IMPORTED_LOCATION", &found.suffix)
                .filter(|path| !path.is_empty())
                .or_else(implib)
        }),
        ArtifactType::Import => found.implib.or_else(implib),
    }
    .filter(|value| !value.is_empty())
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
            // INTEGRATION: the target's creation backtrace.
            match target_policy(ctx, target, 111) {
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

#[cfg(test)]
type TestSourceProps = Option<Box<dyn Fn(usize, &str, &str) -> Option<String>>>;

#[cfg(test)]
thread_local! {
    /// Test-only stand-in for the source-file model consulted by `source_prop`.
    pub(super) static TEST_SOURCE_PROPS: std::cell::RefCell<TestSourceProps> =
        const { std::cell::RefCell::new(None) };
}

/// A property of the cmSourceFile for `path` in directory `dir_index`
/// (including the global GENERATED registry and `$<TARGET_OBJECTS>` files).
/// `CUSTOM_COMMAND` is ON when a rule is attached to the source.
pub(super) fn source_prop(
    ctx: &GenCtx,
    dir_index: usize,
    path: &str,
    name: &str,
) -> Option<String> {
    #[cfg(test)]
    if let Some(value) = TEST_SOURCE_PROPS.with(|slot| {
        slot.borrow()
            .as_ref()
            .map(|lookup| lookup(dir_index, path, name))
    }) {
        return value;
    }
    if matches!(name, "GENERATED" | "EXTERNAL_OBJECT")
        && ctx.target_objects.borrow().contains_key(&(dir_index, path.to_owned()))
    {
        return Some("1".to_owned());
    }
    if name == "CUSTOM_COMMAND" {
        let index = crate::commands::target::find_known_source(ctx.interp, dir_index, path)?;
        return ctx.interp.ext.sources[&dir_index][index]
            .custom_command
            .as_ref()
            .map(|_| "1".to_owned());
    }
    crate::properties::get(ctx.interp, crate::properties::Object::Source(dir_index, path), name)
}

/// The OBJECT library whose `$<TARGET_OBJECTS>` named `path` in `dir`.
fn target_object_library(ctx: &GenCtx, dir: usize, path: &str) -> Option<String> {
    ctx.target_objects.borrow().get(&(dir, path.to_owned())).cloned()
}

const LEGACY_SOURCE_EXTENSIONS: &[&str] = &[
    "c", "C", "c++", "cc", "cpp", "cxx", "cu", "mpp", "m", "M", "mm", "ixx", "cppm", "ccm", "cxxm",
    "c++m", "h", "hh", "h++", "hm", "hpp", "hxx", "in", "txx", "f", "F", "for", "f77", "f90",
    "f95", "f03", "hip", "ispc",
];

/// `cmSourceFile::ResolveFullPath` for a source listed by `target`.
fn resolve_source(
    ctx: &GenCtx,
    target: &Target,
    source: &str,
    property: &impl Fn(&str, &str) -> Option<String>,
) -> Option<(String, bool)> {
    let makefile = target.makefile;
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
        // INTEGRATION: attach the source entry's backtrace when the target
        // model exposes it alongside SOURCES/INTERFACE_SOURCES.
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

/// `cmSystemTools::SplitPath` for Unix paths: the root ("/") followed by the
/// remaining components (empty components are kept, as kwsys does).
fn split_path(path: &str) -> Vec<&str> {
    match path.strip_prefix('/') {
        Some("") => vec!["/"],
        Some(rest) => std::iter::once("/").chain(rest.split('/')).collect(),
        None => path.split('/').collect(),
    }
}

/// `cmSystemTools::ForceToRelativePath(local, remote)`.
pub(super) fn force_to_relative_path(local: &str, remote: &str) -> String {
    if !is_full_path(remote) {
        return remote.to_owned();
    }
    let local_parts = split_path(local);
    let remote_parts = split_path(remote);
    let common = local_parts
        .iter()
        .zip(&remote_parts)
        .take_while(|(a, b)| a == b)
        .count();
    if common == 0 {
        return remote.to_owned();
    }
    if common == remote_parts.len() && common == local_parts.len() {
        return ".".to_owned();
    }
    if common + 1 == remote_parts.len()
        && remote_parts[common].is_empty()
        && common == local_parts.len()
    {
        return "./".to_owned();
    }
    let mut relative = vec![".."; local_parts.len() - common];
    relative.extend(&remote_parts[common..]);
    relative.join("/")
}

/// `relativeIfUnder` in cmLocalGenerator.cxx.
fn relative_from_tree(top: &str, dir: &str, path: &str) -> String {
    if is_subdirectory(path, dir) || (is_subdirectory(dir, top) && is_subdirectory(path, top)) {
        force_to_relative_path(dir, path)
    } else {
        path.to_owned()
    }
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
    if property(path, "UNITY_SOURCE_FILE").is_some() || pch.is_some() || name.contains("cmake_pch")
    {
        let regex = crate::cmregex::Regex::new("(CMakeFiles/[^/]+.dir/)")
            .expect("static regular expression");
        if let Some((start, end)) = regex.find(&name).and_then(|found| found.groups[0]) {
            name.replace_range(start..end, "");
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

    // cmGlobalNinjaGenerator::ComputeTargetObjectDirectory appends the
    // configuration intermediate directory "." (`<support>/./`).
    let directory = format!("{}./", object_directory(ctx, target, config));
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
        let (value, property) = if dep.imported {
            (
                import_info(dep, config).map_or("", |info| info.link_interface),
                if dep.props.contains("INTERFACE_LINK_LIBRARIES") {
                    "INTERFACE_LINK_LIBRARIES"
                } else {
                    "IMPORTED_LINK_INTERFACE_LIBRARIES"
                },
            )
        } else {
            (
                dep.props.get("INTERFACE_LINK_LIBRARIES").unwrap_or_default(),
                "INTERFACE_LINK_LIBRARIES",
            )
        };
        pending.extend(
            expand_list(&ctx.genex_with_dag(value, dep, target, property, None), false)
                .into_iter()
                .rev()
                .filter_map(|name| target_in_directory(ctx, dep.makefile, &name)),
        );
    }
    closure
}

fn evaluated_links(ctx: &GenCtx, provider: &Target, head: &Target, property: &str) -> Vec<String> {
    expand_list(
        &ctx.genex_with_dag(provider.props.get(property).unwrap_or_default(), provider, head, property, None),
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

/// One evaluated item of `GetSourceFilePaths`.
enum Listed {
    /// A source path and, for INTERFACE_SOURCES, the providing target.
    Path {
        path: String,
        provider: Option<String>,
    },
    /// `$<TARGET_OBJECTS:lib>` of a directly linked OBJECT library.
    Object { path: String, library: String },
}

/// `cmGeneratorTarget::GetSourceFilePaths` + `ComputeKindedSources`.
fn collect_sources(
    ctx: &GenCtx,
    target: &Target,
    config: &str,
    compile_closure: &[&Target],
    property: &impl Fn(&str, &str) -> Option<String>,
) -> Vec<SourceInfo> {
    let path_entry = |path: String, provider: Option<&Target>| Listed::Path {
        path,
        provider: provider.map(|provider| provider.name.clone()),
    };
    // Each group is one processSources call, with one entry per provider.
    let mut groups: Vec<Vec<Vec<Listed>>> = Vec::new();
    groups.push(vec![
        evaluated_links(ctx, target, target, "SOURCES")
            .into_iter()
            .map(|source| path_entry(source, None))
            .collect(),
    ]);
    // AddInterfaceEntries(INTERFACE_SOURCES) over the compile closure.
    groups.push(
        compile_closure
            .iter()
            .map(|provider| {
                evaluated_links(ctx, provider, target, "INTERFACE_SOURCES")
                    .into_iter()
                    .map(|source| path_entry(source, Some(provider)))
                    .collect()
            })
            .collect(),
    );
    // AddObjectEntries: objects of OBJECT libraries in the link implementation.
    if target.kind != TargetKind::ObjectLibrary {
        let mut entries = Vec::new();
        for item in evaluated_links(ctx, target, target, "LINK_LIBRARIES") {
            let Some(library) = target_in_directory(ctx, target.makefile, &item)
                .filter(|dep| dep.kind == TargetKind::ObjectLibrary && !std::ptr::eq(*dep, target))
            else {
                continue;
            };
            let objects = if library.imported {
                let list = mapped_import_config(library, config).and_then(|info| info.location);
                expand_list(list.unwrap_or_default(), false)
            } else {
                let object_dir = object_directory(ctx, library, config);
                sources(ctx, library, config)
                    .iter()
                    .filter(|source| source.kind == SourceKind::ObjectSource)
                    .map(|source| format!("{object_dir}{}", source.object_name))
                    .collect()
            };
            entries.push(
                objects
                    .into_iter()
                    .map(|path| Listed::Object {
                        path,
                        library: library.name.clone(),
                    })
                    .collect(),
            );
        }
        groups.push(entries);
    }
    let mut header_set = HashSet::new();
    let mut set_files = Vec::new();
    for (file, header) in file_sets(ctx, target) {
        if header {
            // addFileSetEntry marks HEADERS members HEADER_FILE_ONLY.
            header_set.insert(file.clone());
        }
        set_files.push(path_entry(file, None));
    }
    groups.push(vec![set_files]);

    let mut found = Vec::new();
    let mut seen = HashSet::new();
    let mut bad_objects = Vec::new();
    let mut unity_objects = false;
    'groups: for group in groups {
        for entry in group {
            // Resolve the whole entry first; an unresolvable source drops the
            // entry and ends this processSources call.
            let mut resolved = Vec::new();
            for item in entry {
                let (path, generated, listed, object_library) = match item {
                    Listed::Object { path, library } => (path.clone(), true, path, Some(library)),
                    Listed::Path {
                        path: source,
                        provider,
                    } => {
                        let Some((path, generated)) =
                            resolve_source(ctx, target, &source, property)
                        else {
                            continue 'groups;
                        };
                        if let Some(provider) = provider
                            && !is_full_path(&source)
                        {
                            ctx.error(
                            Vec::new(),
                            format!(
                                "Target \"{provider}\" contains relative path in its INTERFACE_SOURCES:\n  \"{source}\""
                            ),
                        );
                            continue 'groups;
                        }
                        let library = target_object_library(ctx, target.makefile, &path);
                        (path, generated, source, library)
                    }
                };
                resolved.push((path, generated, listed, object_library));
            }
            for (path, generated, listed, object_library) in resolved {
                if !seen.insert(path.clone()) {
                    continue;
                }
                if object_library.is_some() {
                    found.push(SourceInfo {
                        path,
                        language: String::new(),
                        kind: SourceKind::ExternalObject,
                        object_name: String::new(),
                        generated,
                        object_library,
                    });
                    continue;
                }
                let language = source_language(ctx, &path, property);
                let set_kind = header_set.contains(&path).then_some(SourceKind::Header);
                let kind = classify_source(target, &path, &language, set_kind, property);
                if target.kind == TargetKind::ObjectLibrary
                    && (kind == SourceKind::ModuleDefinition
                        || (kind == SourceKind::Resource
                            && source_extension(&path).eq_ignore_ascii_case("idl")))
                {
                    bad_objects.push(filename_name(&listed));
                }
                let object_name = if kind == SourceKind::ObjectSource {
                    unity_objects |=
                        !is_on_opt(property(&path, "SKIP_UNITY_BUILD_INCLUSION").as_deref());
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
                    object_library: None,
                });
            }
        }
    }
    if !bad_objects.is_empty() {
        ctx.error(Vec::new(), format!(
            "OBJECT library \"{}\" contains:\n  {}\nbut may contain only sources that compile, header files, and other files that would not affect linking of a normal library.",
            target.name, bad_objects.join("\n  ")
        ));
    }
    if unity_objects && is_on_opt(target.props.get("UNITY_BUILD")) {
        // cmLocalGenerator::AddUnityBuild is not ported; fail loudly rather
        // than silently generating a non-unity build.
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
pub fn sources(ctx: &GenCtx, target: &Target, config: &str) -> std::rc::Rc<Vec<SourceInfo>> {
    let key = memo_key("sources", target, config);
    if let Some(cached) = ctx.sources.borrow().get(&key) {
        return cached.clone();
    }
    let computed = std::rc::Rc::new(sources_with_link_closure(
        ctx,
        target,
        config,
        &link_closure(ctx, target, config),
    ));
    ctx.sources.borrow_mut().insert(key, computed.clone());
    computed
}

/// `cmTarget::CanCompileSources`.
pub fn can_compile_sources(target: &Target) -> bool {
    !target.imported
        && matches!(
            target.kind,
            TargetKind::Executable
                | TargetKind::StaticLibrary
                | TargetKind::SharedLibrary
                | TargetKind::ModuleLibrary
                | TargetKind::ObjectLibrary
        )
}

/// `cmGlobalGenerator::CheckTargetsForMissingSources`. Returns true when an
/// error was reported (generation must stop).
#[allow(dead_code, reason = "preflight query for the generator driver")]
pub fn check_targets_for_missing_sources(ctx: &GenCtx) -> bool {
    let mut failed = false;
    for target in ctx.targets_in_order() {
        if !can_compile_sources(target) || is_on_opt(target.props.get("ghs_integrity_app")) {
            continue;
        }
        if sources(ctx, target, &ctx.config).is_empty() {
            ctx.error(
                Vec::new(),
                format!("No SOURCES given to target: {}", target.name),
            );
            failed = true;
        }
    }
    failed
}

/// `cmGeneratorTarget::GetLanguages`: languages of all sources plus those of
/// OBJECT libraries whose objects are among them.
fn target_languages(
    ctx: &GenCtx,
    target: &Target,
    config: &str,
    sources_found: &[SourceInfo],
    languages: &mut BTreeSet<String>,
) {
    if !can_compile_sources(target) {
        return;
    }
    let mut libraries = BTreeSet::new();
    for source in sources_found {
        if !source.language.is_empty() {
            languages.insert(source.language.clone());
        }
        if let Some(library) = &source.object_library {
            libraries.insert(library.clone());
        }
    }
    for library in libraries {
        if let Some(library) = target_in_directory(ctx, target.makefile, &library) {
            let found = sources(ctx, library, config);
            target_languages(ctx, library, config, &found, languages);
        }
    }
}

/// `cmGeneratorTarget::ComputeLinkClosure` + `cmTargetSelectLinker`.
fn select_linker_language(
    ctx: &GenCtx,
    target: &Target,
    config: &str,
    sources: &[SourceInfo],
    link_closure: &[&Target],
) -> String {
    let mut own = BTreeSet::new();
    target_languages(ctx, target, config, sources, &mut own);
    // cmTargetCollectLinkLanguages: link interface languages, which only
    // static libraries propagate (LinkLanguagePropagatesToDependents).
    let mut languages = own.clone();
    for provider in link_closure {
        if provider.kind != TargetKind::StaticLibrary {
            continue;
        }
        if provider.imported {
            if let Some(import) = import_info(provider, config) {
                languages.extend(expand_list(import.languages, false));
            }
        } else {
            let found = self::sources(ctx, provider, config);
            target_languages(ctx, provider, config, &found, &mut languages);
        }
    }
    let preference = |language: &str| {
        ctx.interp
            .langs
            .linker_preference
            .get(language)
            .copied()
            .unwrap_or(0)
    };
    let mut best = 0;
    let mut preferred = BTreeSet::new();
    let mut consider = |language: &str| {
        let value = preference(language);
        if value > best {
            best = value;
            preferred.clear();
        }
        if value == best {
            preferred.insert(language.to_owned());
        }
    };
    for language in &own {
        consider(language);
    }
    for language in &languages {
        let variable = format!("CMAKE_{language}_LINKER_PREFERENCE_PROPAGATES");
        if is_on_opt(ctx.var(target.makefile, &variable).as_deref()) {
            consider(language);
        }
    }
    if preferred.len() > 1 {
        let list: Vec<_> = preferred.iter().map(String::as_str).collect();
        ctx.error(Vec::new(), format!(
            "Target {} contains multiple languages with the highest linker preference ({best}):\n  {}\nSet the LINKER_LANGUAGE property for this target.",
            target.name, list.join("\n  ")
        ));
    }
    preferred.into_iter().next().unwrap_or_default()
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
#[allow(
    dead_code,
    reason = "entry point for an externally computed link closure"
)]
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
    // GetLinkClosure: no link implementation for targets that cannot
    // compile sources.
    if !can_compile_sources(target) {
        return String::new();
    }
    if let Some(language) = fixed_linker_language(target) {
        return language.to_owned();
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
        let sources = sources(ctx, target, config);
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
    let (language, sensitive) = compute();
    ctx.memo.borrow_mut().insert(key, language.clone());
    if sensitive {
        let (second, _) = compute();
        if second != language {
            // cmSystemTools::Error (no backtrace).
            ctx.error(Vec::new(), format!(
                "Evaluation of $<LINK_LANGUAGE:...> or $<LINK_LAND_AND_ID:...> changes\nthe linker language for target \"{}\" (from '{language}' to '{second}') which is invalid.",
                target.name
            ));
        }
    }
    language
}

/// `cmGeneratorTarget::GetLinkerLanguage(config)` (empty if none). Like
/// upstream this never reports an error; the Ninja target generator reports
/// "CMake can not determine linker language for target: ...".
pub fn linker_language(ctx: &GenCtx, target: &Target, config: &str) -> String {
    compute_linker_language(ctx, target, config)
}

#[cfg(test)]
mod corpus_tests {
    //! Replays the oracle ground truth in tests/data/generator/target/*.json.
    //! Each fixture describes a small project as a model; this harness rebuilds
    //! that model in memory and compares artifact paths, object files, external
    //! objects, linker languages, custom-command lines and diagnostics.

    use std::collections::{BTreeMap, BTreeSet, HashMap};
    use std::fs;

    use serde_json::Value;

    use super::*;
    use crate::generator::custom::{CustomCommandSpec, evaluate_spec};
    use crate::interp::{Interp, Makefile, Mode};
    use crate::props::Props;

    /// Stands in for the oracle's CMAKE_COMMAND (fixtures record `<CMAKE>`).
    const CMAKE: &str = "/opt/cmake-oracle/bin/cmake";

    fn fixtures() -> Vec<(String, Value)> {
        let dir = format!("{}/tests/data/generator/target", env!("CARGO_MANIFEST_DIR"));
        let mut out = Vec::new();
        for entry in fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            if path.extension().is_none_or(|ext| ext != "json") {
                continue;
            }
            let json: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
            if json.get("case").is_some() {
                out.push((json["case"]["name"].as_str().unwrap().to_owned(), json));
            }
        }
        out.sort_by(|a, b| a.0.cmp(&b.0));
        out
    }

    fn kind_from(name: &str) -> TargetKind {
        match name {
            "EXECUTABLE" => TargetKind::Executable,
            "STATIC_LIBRARY" => TargetKind::StaticLibrary,
            "SHARED_LIBRARY" => TargetKind::SharedLibrary,
            "MODULE_LIBRARY" => TargetKind::ModuleLibrary,
            "OBJECT_LIBRARY" => TargetKind::ObjectLibrary,
            "INTERFACE_LIBRARY" => TargetKind::InterfaceLibrary,
            "UNKNOWN_LIBRARY" => TargetKind::UnknownLibrary,
            other => panic!("unknown kind {other}"),
        }
    }

    fn strings(value: &Value) -> Vec<String> {
        value
            .as_array()
            .map(|items| {
                items
                    .iter()
                    .map(|v| v.as_str().unwrap().to_owned())
                    .collect()
            })
            .unwrap_or_default()
    }

    type SourceProps = Vec<(Vec<String>, BTreeMap<String, String>)>;

    struct Project {
        interp: Interp,
        dirs: HashMap<String, usize>,
        root: String,
        source_props: SourceProps,
    }

    fn build_project(name: &str, json: &Value) -> Project {
        let case = &json["case"];
        let expected = &json["expected"];
        // The oracle ran under a root of the same length, so object path length
        // limits reproduce exactly.
        let root_len = json["root_len"].as_u64().unwrap() as usize;
        let prefix = format!("/tmp/gtr-{}", std::process::id());
        let suffix = format!("/{name}");
        assert!(
            prefix.len() + suffix.len() <= root_len,
            "root too long for {name}"
        );
        let root = format!(
            "{prefix}{}{suffix}",
            "x".repeat(root_len - prefix.len() - suffix.len())
        );
        let _ = fs::remove_dir_all(&root);
        for file in strings(&case["files"]) {
            let path = format!("{root}/{file}");
            fs::create_dir_all(filename_path(&path)).unwrap();
            fs::write(&path, "").unwrap();
        }
        fs::create_dir_all(format!("{root}/build")).unwrap();
        let subst = |text: &str| text.replace("<ROOT>", &root);

        let mut interp = Interp::new(
            Mode::Project,
            &format!("{root}/src"),
            &format!("{root}/build"),
            "",
        );
        interp.cmake_command = CMAKE.to_owned();
        let config = case["config"].as_str().unwrap_or("Debug");
        interp.set_def("CMAKE_BUILD_TYPE", config);

        let mut paths: Vec<String> = case["dirs"].as_object().unwrap().keys().cloned().collect();
        paths.sort();
        let mut dirs = HashMap::new();
        for path in &paths {
            let index = if path.is_empty() {
                0
            } else {
                let parent = path.rsplit_once('/').map_or("", |(parent, _)| parent);
                let parent = dirs[parent];
                interp.makefiles.push(Makefile {
                    source_dir: format!("{root}/src/{path}"),
                    binary_dir: format!("{root}/build/{path}"),
                    parent: Some(parent),
                    props: Props::default(),
                    policies: interp.makefiles[0].policies.clone(),
                    list_files: Vec::new(),
                    targets: Vec::new(),
                    children: Vec::new(),
                    exclude_from_all: false,
                    deferred: Vec::new(),
                    defer_counter: 0,
                });
                let index = interp.makefiles.len() - 1;
                interp.makefiles[parent].children.push(index);
                index
            };
            dirs.insert(path.clone(), index);
            let recorded = &expected["dirs"][path.as_str()];
            let frame = interp.makefiles[index].policies.last_mut().unwrap();
            for (id, status) in recorded["policies"].as_object().unwrap() {
                let status = match status.as_str().unwrap() {
                    "NEW" => PolicyStatus::New,
                    "OLD" => PolicyStatus::Old,
                    _ => PolicyStatus::Warn,
                };
                frame.map.set(policy::index_of(id).unwrap(), status);
            }
            if interp.dir_final_vars.len() <= index {
                interp.dir_final_vars.resize(index + 1, HashMap::new());
            }
            interp.dir_final_vars[index] = recorded["vars"]
                .as_object()
                .unwrap()
                .iter()
                .map(|(key, value)| (key.clone(), Some(subst(value.as_str().unwrap()))))
                .collect();
        }

        let root_vars = &expected["dirs"][""]["vars"];
        let languages = case["languages"].as_str().unwrap_or("C CXX");
        for language in languages.split_whitespace() {
            let var = |suffix: &str| root_vars[format!("CMAKE_{language}_{suffix}")].as_str();
            for extension in expand_list(var("SOURCE_FILE_EXTENSIONS").unwrap_or_default(), false) {
                interp
                    .langs
                    .extension_to_language
                    .insert(extension, language.to_owned());
            }
            for extension in expand_list(var("IGNORE_EXTENSIONS").unwrap_or_default(), false) {
                interp.langs.ignore_extensions.insert(extension);
            }
            if let Some(extension) = var("OUTPUT_EXTENSION") {
                interp
                    .langs
                    .output_extension
                    .insert(language.to_owned(), extension.to_owned());
            }
            if let Some(preference) = var("LINKER_PREFERENCE") {
                interp
                    .langs
                    .linker_preference
                    .insert(language.to_owned(), preference.parse().unwrap_or(0));
            }
        }

        for target in case["targets"].as_array().unwrap() {
            let mut props = Props::default();
            for field in ["props", "rust_props"] {
                if let Some(map) = target[field].as_object() {
                    for (key, value) in map {
                        props.set(key, subst(value.as_str().unwrap()));
                    }
                }
            }
            let name = target["name"].as_str().unwrap().to_owned();
            let makefile = dirs[target["dir"].as_str().unwrap_or("")];
            interp.makefiles[makefile].targets.push(name.clone());
            interp.ext.target_order.push(name.clone());
            interp.ext.targets.insert(
                name.clone(),
                Target {
                    name,
                    kind: kind_from(target["kind"].as_str().unwrap()),
                    makefile,
                    imported: target["imported"].as_bool().unwrap_or(false),
                    imported_global: target["global"].as_bool().unwrap_or(false),
                    alias_of: None,
                    props,
                    provenance: Default::default(),
                    backtrace: Vec::new(),
                    ..crate::state::Target::new("", crate::state::TargetKind::Executable, 0)
                },
            );
        }

        let mut source_props = Vec::new();
        for entry in case["source_props"].as_array().into_iter().flatten() {
            let makefile = &interp.makefiles[dirs[entry["dir"].as_str().unwrap_or("")]];
            let props: BTreeMap<String, String> = entry["props"]
                .as_object()
                .unwrap()
                .iter()
                .map(|(k, v)| (k.clone(), v.as_str().unwrap().to_owned()))
                .collect();
            for file in strings(&entry["files"]) {
                // cmSourceFileLocation: a relative name matches in either tree.
                let candidates = vec![
                    collapse_full_path(&file, &makefile.source_dir),
                    collapse_full_path(&file, &makefile.binary_dir),
                    file.clone(),
                ];
                source_props.push((candidates, props.clone()));
            }
        }
        Project {
            interp,
            dirs,
            root,
            source_props,
        }
    }

    fn install_source_props(props: &SourceProps) {
        let props = props.clone();
        TEST_SOURCE_PROPS.with(|slot| {
            *slot.borrow_mut() = Some(Box::new(move |_dir, path, name| {
                props
                    .iter()
                    .find(|(candidates, _)| candidates.iter().any(|c| c == path))
                    .and_then(|(_, props)| props.get(name).cloned())
            }));
        });
    }

    /// Bodies of "CMake Error" diagnostics (the indented lines after the header).
    fn error_bodies(stderr: &str) -> BTreeSet<String> {
        let mut bodies = BTreeSet::new();
        let lines: Vec<&str> = stderr.lines().collect();
        let mut index = 0;
        while index < lines.len() {
            if let Some(text) = lines[index].strip_prefix("CMake Error: ") {
                // cmSystemTools::Error: a single unindented line.
                bodies.insert(text.to_owned());
                index += 1;
            } else if lines[index].starts_with("CMake Error") {
                let mut body = Vec::new();
                index += 1;
                while index < lines.len()
                    && (lines[index].starts_with("  ") || lines[index].is_empty())
                {
                    if lines[index].is_empty()
                        && lines
                            .get(index + 1)
                            .is_none_or(|next| !next.starts_with("  "))
                    {
                        break;
                    }
                    body.push(lines[index]);
                    index += 1;
                }
                bodies.insert(body.join("\n"));
            } else {
                index += 1;
            }
        }
        bodies
    }

    fn check_case(name: &str, json: &Value) -> Vec<String> {
        let project = build_project(name, json);
        install_source_props(&project.source_props);
        let root = project.root.clone();
        let subst = |text: &str| text.replace("<ROOT>", &root).replace("<CMAKE>", CMAKE);
        let interp = &project.interp;
        let ctx = GenCtx::new(interp);
        let config = ctx.config.clone();
        let expected = &json["expected"];
        let mut failures = Vec::new();
        let mut check = |what: String, got: String, want: String| {
            if got != want {
                failures.push(format!("{name}: {what}\n    got:  {got}\n    want: {want}"));
            }
        };

        for (target_name, want) in expected["targets"].as_object().unwrap() {
            let target = &interp.ext.targets[target_name];
            let found = sources(&ctx, target, &config);
            let object_dir = object_directory(&ctx, target, &config);
            let objects: Vec<String> = found
                .iter()
                .filter(|source| source.kind == SourceKind::ObjectSource)
                .map(|source| {
                    format!(
                        "{object_dir}{} <- {} [{}]",
                        source.object_name, source.path, source.language
                    )
                })
                .collect();
            let want_objects: Vec<String> = want["objects"]
                .as_array()
                .unwrap()
                .iter()
                .map(|o| {
                    subst(&format!(
                        "{} <- {} [{}]",
                        o["object"].as_str().unwrap(),
                        o["source"].as_str().unwrap(),
                        o["language"].as_str().unwrap()
                    ))
                })
                .collect();
            check(
                format!("{target_name} objects"),
                objects.join("\n          "),
                want_objects.join("\n          "),
            );
            if let Some(output) = want["link_output"].as_str() {
                let externals: Vec<String> = found
                    .iter()
                    .filter(|source| source.kind == SourceKind::ExternalObject)
                    .map(|source| source.path.clone())
                    .collect();
                let want_externals: Vec<String> = strings(&want["externals"])
                    .iter()
                    .map(|e| subst(e))
                    .collect();
                check(
                    format!("{target_name} externals"),
                    externals.join(" "),
                    want_externals.join(" "),
                );
                check(
                    format!("{target_name} linker language"),
                    linker_language(&ctx, target, &config),
                    want["linker_language"].as_str().unwrap().to_owned(),
                );
                check(
                    format!("{target_name} link output"),
                    full_path(&ctx, target, &config, ArtifactType::Runtime, true),
                    subst(output),
                );
            }
        }

        for (target_name, want) in expected["artifacts"].as_object().unwrap() {
            let target = &interp.ext.targets[target_name];
            if let Some(file) = want["file"].as_str() {
                check(
                    format!("{target_name} TARGET_FILE"),
                    full_path(&ctx, target, &config, ArtifactType::Runtime, true),
                    subst(file),
                );
            }
            if let Some(file) = want["linker"].as_str() {
                check(
                    format!("{target_name} TARGET_LINKER_FILE"),
                    full_path(&ctx, target, &config, ArtifactType::Runtime, false),
                    subst(file),
                );
            }
            if let Some(file) = want["soname"].as_str() {
                let soname = if target.imported {
                    import_info(target, &config)
                        .map_or("", |info| info.soname)
                        .to_owned()
                } else {
                    names(&ctx, target, &config).shared_object
                };
                check(
                    format!("{target_name} TARGET_SONAME_FILE"),
                    format!(
                        "{}/{soname}",
                        directory(&ctx, target, &config, ArtifactType::Runtime)
                    ),
                    subst(file),
                );
            }
        }

        let case = &json["case"];
        let customs = expected["custom"].as_array().unwrap();
        for spec_json in case["custom"].as_array().into_iter().flatten() {
            let makefile = project.dirs[spec_json["dir"].as_str().unwrap_or("")];
            let list = |field: &str| {
                strings(&spec_json[field])
                    .iter()
                    .map(|v| subst(v))
                    .collect::<Vec<_>>()
            };
            let mut depends = list("depends");
            if let Some(main) = spec_json["main_dependency"].as_str() {
                depends.insert(0, subst(main));
            }
            let spec = CustomCommandSpec {
                makefile,
                target: None,
                command_lines: spec_json["commands"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|line| {
                        strings(line)
                            .iter()
                            .map(|arg| subst(&arg.replace("${CMAKE_COMMAND}", CMAKE)))
                            .collect()
                    })
                    .collect(),
                outputs: list("outputs"),
                byproducts: list("byproducts"),
                depends,
                working_directory: subst(spec_json["working_directory"].as_str().unwrap_or("")),
                comment: spec_json["comment"].as_str().map(&subst),
                depfile: subst(spec_json["depfile"].as_str().unwrap_or("")),
                verbatim: spec_json["verbatim"].as_bool().unwrap_or(false),
                command_expand_lists: spec_json["command_expand_lists"].as_bool().unwrap_or(false),
                ..CustomCommandSpec::default()
            };
            let result = evaluate_spec(&ctx, &spec);
            let first = result.outputs.first().cloned().unwrap_or_default();
            let Some(want) = customs
                .iter()
                .find(|c| c["outputs"][0].as_str().map(&subst).as_deref() == Some(first.as_str()))
            else {
                check(
                    "custom command output".to_owned(),
                    first,
                    "<none in oracle>".to_owned(),
                );
                continue;
            };
            let outputs: Vec<String> = result
                .outputs
                .iter()
                .chain(&result.byproducts)
                .cloned()
                .collect();
            let want_list = |field: &str| {
                strings(&want[field])
                    .iter()
                    .map(|v| subst(v))
                    .collect::<Vec<_>>()
            };
            check(
                format!("{first} outputs"),
                outputs.join(" "),
                want_list("outputs").join(" "),
            );
            check(
                format!("{first} depends"),
                result.depends.join(" "),
                want_list("inputs").join(" "),
            );
            check(
                format!("{first} COMMAND"),
                result.shell_lines(&ctx, makefile).join(" && "),
                subst(want["command"].as_str().unwrap()),
            );
            check(
                format!("{first} DESC"),
                result.description(&ctx, makefile, ""),
                subst(want["desc"].as_str().unwrap()),
            );
        }

        if expected["exit"].as_i64() == Some(0) {
            let errors = format!("{:?}", ctx.errors.borrow());
            check("errors".to_owned(), errors, "[]".to_owned());
        } else {
            // Drive the queries generation performs for every target:
            // cmGlobalGenerator::Compute, then the Ninja target generators.
            let mut generator_errors = String::new();
            let compute_failed = check_targets_for_missing_sources(&ctx);
            for target in ctx.targets_in_order() {
                if compute_failed {
                    break;
                }
                if target.imported {
                    continue;
                }
                sources(&ctx, target, &config);
                if matches!(
                    target.kind,
                    TargetKind::Executable
                        | TargetKind::StaticLibrary
                        | TargetKind::SharedLibrary
                        | TargetKind::ModuleLibrary
                ) {
                    if linker_language(&ctx, target, &config).is_empty() {
                        // cmNinjaNormalTargetGenerator::Generate.
                        generator_errors.push_str(&format!(
                            "CMake Error: CMake can not determine linker language for target: {}\n",
                            target.name
                        ));
                        continue;
                    }
                    full_path(&ctx, target, &config, ArtifactType::Runtime, true);
                    for dep in expand_list(
                        target.props.get("LINK_LIBRARIES").unwrap_or_default(),
                        false,
                    ) {
                        if let Some(dep) = target_in_directory(&ctx, target.makefile, &dep)
                            && dep.imported
                        {
                            full_path(&ctx, dep, &config, ArtifactType::Runtime, true);
                        }
                    }
                }
            }
            let rendered: String = generator_errors
                + &ctx
                    .errors
                    .borrow()
                    .iter()
                    .map(|(_, message)| {
                        crate::messages::render(MessageType::FatalError, message, &[], None)
                    })
                    .collect::<String>();
            let want = error_bodies(&subst(expected["stderr"].as_str().unwrap()));
            let got = error_bodies(&rendered);
            check("errors".to_owned(), format!("{got:?}"), format!("{want:?}"));
        }
        TEST_SOURCE_PROPS.with(|slot| slot.borrow_mut().take());
        drop(ctx);
        let _ = fs::remove_dir_all(filename_path(&project.root));
        failures
    }

    #[test]
    fn oracle_corpus() {
        let fixtures = fixtures();
        assert!(
            fixtures.len() >= 40,
            "corpus has {} projects",
            fixtures.len()
        );
        let mut failures = Vec::new();
        for (name, json) in &fixtures {
            failures.extend(check_case(name, json));
        }
        assert!(
            failures.is_empty(),
            "{} mismatches:\n{}",
            failures.len(),
            failures.join("\n")
        );
    }
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
            ..crate::state::Target::new("", crate::state::TargetKind::Executable, 0)
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

        type Case<'a> = (&'a str, TargetKind, &'a [(&'a str, &'a str)]);
        let cases: &[Case<'_>] = &[
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
                    sources(&ctx, &target, &ctx.config).as_ref().clone(),
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
