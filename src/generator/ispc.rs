//! ISPC support shared by the generators: `cmLocalGenerator::AddISPCDependencies`
//! and the `detail::ComputeISPC*` helpers.  An ISPC source compiles to its
//! object plus a header (`<name><ISPC_HEADER_SUFFIX>`) and, with several
//! `ISPC_INSTRUCTION_SETS`, one extra object per instruction set.

use super::ctx::GenCtx;
use super::target::{self as gt, SourceKind};
use crate::state::Target;
use crate::util;

/// `detail::ComputeISPCObjectSuffixes`: the instruction-set families
/// (`avx2-i32x8` -> `avx2`; `avx1` is spelled `avx` in output names).
pub(crate) fn object_suffixes(target: &Target) -> Vec<String> {
    let property = target.props.get("ISPC_INSTRUCTION_SETS");
    if util::is_off_opt(property) {
        return Vec::new();
    }
    util::expand_list(property.unwrap_or_default(), false)
        .into_iter()
        .map(|set| {
            let family = set.split('-').next().unwrap_or_default().to_owned();
            if family == "avx1" {
                "avx".to_owned()
            } else {
                family
            }
        })
        .collect()
}

/// `detail::ComputeISPCExtraObjects`.
pub(crate) fn extra_objects(
    object_name: &str,
    build_dir: &str,
    suffixes: &[String],
) -> Vec<String> {
    let dir = util::collapse_full_path(build_dir, "");
    let name = util::filename_name(object_name);
    let extension = match name.rfind('.') {
        Some(dot) => name[dot..].to_owned(),
        None => String::new(),
    };
    let stem = match object_name.rfind('.') {
        Some(dot) => &object_name[..dot],
        None => object_name,
    };
    suffixes
        .iter()
        .map(|suffix| format!("{dir}/{stem}_{suffix}{extension}"))
        .collect()
}

/// `cmSystemTools::GetFilenameWithoutLastExtension` applied twice to an
/// object name (`dir/k.ispc.o` -> `k`).
pub(crate) fn source_stem(object_name: &str) -> String {
    let once = util::filename_without_last_extension(object_name);
    util::filename_without_last_extension(&once)
}

fn ispc_enabled(ctx: &GenCtx) -> bool {
    ctx.interp.langs.enabled.iter().any(|lang| lang == "ISPC")
}

/// The ISPC sources of `target` for `config`, with their object names.
fn ispc_sources(ctx: &GenCtx, target: &Target, config: &str) -> Vec<String> {
    gt::sources(ctx, target, config)
        .iter()
        .filter(|source| source.language == "ISPC" && source.kind == SourceKind::ObjectSource)
        .map(|source| source.object_name.clone())
        .collect()
}

/// `cmGeneratorTarget::GetGeneratedISPCHeaders` as filled by
/// `AddISPCDependencies`.
pub(crate) fn generated_headers(ctx: &GenCtx, target: &Target, config: &str) -> Vec<String> {
    if !ispc_enabled(ctx) {
        return Vec::new();
    }
    let suffix = target.props.get("ISPC_HEADER_SUFFIX").unwrap_or_default();
    let root = gt::object_directory(ctx, target, config);
    let header_dir = match target.props.get("ISPC_HEADER_DIRECTORY") {
        Some(dir) => {
            util::collapse_full_path(&format!("{}/{dir}", ctx.interp.makefiles[0].binary_dir), "")
        }
        None => root,
    };
    ispc_sources(ctx, target, config)
        .iter()
        .map(|object| format!("{header_dir}/{}{suffix}", source_stem(object)))
        .collect()
}

/// `cmGeneratorTarget::GetGeneratedISPCObjects` as filled by
/// `AddISPCDependencies` (only with more than one instruction set).
pub(crate) fn generated_objects(ctx: &GenCtx, target: &Target, config: &str) -> Vec<String> {
    if !ispc_enabled(ctx) {
        return Vec::new();
    }
    let suffixes = object_suffixes(target);
    if suffixes.len() <= 1 {
        return Vec::new();
    }
    let root = gt::object_directory(ctx, target, config);
    ispc_sources(ctx, target, config)
        .iter()
        .flat_map(|object| extra_objects(object, &root, &suffixes))
        .collect()
}

/// The tail of `cmGeneratorTarget::GetTargetObjectNames`: each extra ISPC
/// object relative to the collapsed object directory (so it keeps the
/// leading `/`).
pub(crate) fn target_object_names(ctx: &GenCtx, target: &Target, config: &str) -> Vec<String> {
    let root = util::collapse_full_path(&gt::object_directory(ctx, target, config), "");
    generated_objects(ctx, target, config)
        .into_iter()
        .map(|object| object.get(root.len()..).unwrap_or_default().to_owned())
        .collect()
}
