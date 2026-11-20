//! cmGeneratorTarget core: artifact naming, directories, sources, object
//! names, linker language. OWNER: generator-target worker.
//! Spec: cmGeneratorTarget.cxx (GetFullName/GetFullPath/GetDirectory/
//! GetOutputName/GetLibraryNames/GetExecutableNames/GetObjectDirectory/
//! ComputeObjectMapping/GetLinkerLanguage/GetSourceFiles...),
//! cmGeneratorTarget_Sources.cxx, cmLocalGenerator::GetObjectFileNameWithoutTarget.

use super::ctx::GenCtx;
use crate::state::Target;

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

/// `cmGeneratorTarget::GetLibraryNames` / `GetExecutableNames`.
pub fn names(ctx: &GenCtx, target: &Target, config: &str) -> Names {
    let _ = (ctx, target, config);
    todo!("generator::target::names")
}

/// `cmGeneratorTarget::GetFullPath(config, artifact, realname)`.
pub fn full_path(ctx: &GenCtx, target: &Target, config: &str, artifact: ArtifactType, realname: bool) -> String {
    let _ = (ctx, target, config, artifact, realname);
    todo!("generator::target::full_path")
}

/// `cmGeneratorTarget::GetDirectory(config, artifact)`.
pub fn directory(ctx: &GenCtx, target: &Target, config: &str, artifact: ArtifactType) -> String {
    let _ = (ctx, target, config, artifact);
    todo!("generator::target::directory")
}

/// `cmGeneratorTarget::GetFullName(config, artifact)`.
pub fn full_name(ctx: &GenCtx, target: &Target, config: &str, artifact: ArtifactType) -> String {
    let _ = (ctx, target, config, artifact);
    todo!("generator::target::full_name")
}

/// `cmGeneratorTarget::GetObjectDirectory` (absolute, with trailing '/').
/// For Ninja: `<binary dir>/CMakeFiles/<target>.dir/`.
pub fn object_directory(ctx: &GenCtx, target: &Target, config: &str) -> String {
    let _ = (ctx, target, config);
    todo!("generator::target::object_directory")
}

/// All sources of the target for `config` with classification and object
/// names (`GetSourceFiles` + `ComputeObjectMapping`).
pub fn sources(ctx: &GenCtx, target: &Target, config: &str) -> Vec<SourceInfo> {
    let _ = (ctx, target, config);
    todo!("generator::target::sources")
}

/// `cmGeneratorTarget::GetLinkerLanguage(config)` (empty if none).
pub fn linker_language(ctx: &GenCtx, target: &Target, config: &str) -> String {
    let _ = (ctx, target, config);
    todo!("generator::target::linker_language")
}

/// `cmGeneratorTarget::HasSOName`, `IsDLLPlatform`... and other boolean
/// queries live here too as needed.
pub fn has_soname(ctx: &GenCtx, target: &Target, config: &str) -> bool {
    let _ = (ctx, target, config);
    todo!("generator::target::has_soname")
}
