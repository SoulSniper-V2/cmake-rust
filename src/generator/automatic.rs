//! `cmGlobalGenerator::AddAutomaticSources`: unity builds
//! (`cmLocalGenerator::AddUnityBuild`) and precompiled headers
//! (`cmLocalGenerator::AddPchDependencies` with the file writes of
//! `cmGeneratorTarget::GetPchHeader`/`GetPchSource`).

use std::path::Path;

use super::ctx::GenCtx;
use super::pch;
use super::target as gt;
use crate::interp::Interp;
use crate::state::{Target, TargetKind};
use crate::util::{collapse_full_path, expand_list, is_on_opt};

/// Changes computed against a generation context, applied afterwards.
#[derive(Default)]
struct Changes {
    /// `cmGeneratorTarget::AddSource(src, before)`, in call order.
    add_sources: Vec<(String, bool)>,
    /// Source properties to set (`append` for AppendProperty).
    source_props: Vec<(String, String, String, bool)>,
    /// `AddSourceFileToUnityBatch`.
    unity_batched: Vec<String>,
    /// Target properties appended (name, value, as_string).
    target_appends: Vec<(String, String, bool)>,
    /// Diagnostics with their backtraces.
    errors: Vec<(Vec<crate::messages::Frame>, String)>,
}

/// `cmTarget::CanCompileSources`.
fn can_compile_sources(target: &Target) -> bool {
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

pub fn add_automatic_sources(interp: &mut Interp) {
    let any_unity = interp
        .ext
        .targets
        .values()
        .any(|t| can_compile_sources(t) && is_on_opt(t.props.get("UNITY_BUILD")));
    let any_pch = interp.ext.targets.values().any(|t| {
        t.props.contains("PRECOMPILE_HEADERS")
            || t.props.contains("INTERFACE_PRECOMPILE_HEADERS")
            || t.props.contains("PRECOMPILE_HEADERS_REUSE_FROM")
    });
    if !any_unity && !any_pch {
        return;
    }
    let mut keys = Vec::new();
    for mf in &interp.makefiles {
        for key in &mf.targets {
            if interp.ext.targets.get(key).is_some_and(can_compile_sources) {
                keys.push(key.clone());
            }
        }
    }
    let reuses = |interp: &Interp, key: &str| interp.ext.targets[key].props.contains("PRECOMPILE_HEADERS_REUSE_FROM");
    for key in &keys {
        if any_unity && is_on_opt(interp.ext.targets[key].props.get("UNITY_BUILD")) {
            let changes = {
                let ctx = GenCtx::new(interp);
                add_unity_build(&ctx, &ctx.interp.ext.targets[key])
            };
            apply(interp, key, changes);
        }
        if any_pch && !reuses(interp, key) {
            let changes = {
                let ctx = GenCtx::new(interp);
                add_pch_dependencies(&ctx, &ctx.interp.ext.targets[key])
            };
            apply(interp, key, changes);
        }
    }
    // Targets that reuse a PCH from an above-handled target.
    if any_pch {
        for key in &keys {
            if reuses(interp, key) {
                let changes = {
                    let ctx = GenCtx::new(interp);
                    add_pch_dependencies(&ctx, &ctx.interp.ext.targets[key])
                };
                apply(interp, key, changes);
            }
        }
    }
}

fn apply(interp: &mut Interp, key: &str, changes: Changes) {
    let dir = interp.ext.targets[key].makefile;
    for (backtrace, message) in changes.errors {
        interp.issue_at(crate::messages::MessageType::FatalError, &message, &backtrace);
    }
    for (path, name, value, append) in changes.source_props {
        let source = crate::commands::target::get_or_create_source(interp, dir, &path);
        if append {
            source.props.append(&name, &value, false);
        } else {
            source.props.set(&name, value);
        }
    }
    let target = interp.ext.targets.get_mut(key).expect("target");
    for (name, value, as_string) in changes.target_appends {
        target.props.append(&name, &value, as_string);
    }
    target.unity_batched.extend(changes.unity_batched);
    for (path, before) in changes.add_sources {
        // cmTarget::AddSource: only if no entry already names the file.
        let existing = target.props.get("SOURCES").unwrap_or_default();
        let present = expand_list(existing, false)
            .iter()
            .any(|entry| entry == &path || collapse_full_path(entry, "") == path);
        if !present {
            // Sources injected at generate time carry the makefile's
            // persistent file-scope frame (cmLocalGenerator::DirectoryBacktrace).
            let makefile = &interp.makefiles[target.makefile];
            let file = makefile
                .list_files
                .first()
                .cloned()
                .unwrap_or_else(|| format!("{}/CMakeLists.txt", makefile.source_dir));
            let backtrace = [crate::messages::Frame {
                file: std::rc::Rc::from(file.as_str()),
                line: 0,
                name: None,
                deferred: false,
            }];
            target.insert_entry("SOURCES", &path, &backtrace, before);
        }
    }
}

// ---------------------------------------------------------------------
// Precompiled headers

/// `cmFileTimes::Copy` of the modification time.
fn copy_file_times(from: &str, to: &str) {
    if let Ok(meta) = std::fs::metadata(from)
        && let Ok(modified) = meta.modified()
        && let Ok(file) = std::fs::File::options().write(true).open(to)
    {
        let accessed = meta.accessed().unwrap_or(modified);
        let times = std::fs::FileTimes::new().set_modified(modified).set_accessed(accessed);
        let _ = file.set_times(times);
    }
}

/// `cmSystemTools::MoveFileIfDifferent`.
fn move_file_if_different(from: &str, to: &str) {
    let same = match (std::fs::read(from), std::fs::read(to)) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    };
    if same {
        let _ = std::fs::remove_file(from);
    } else {
        let _ = std::fs::rename(from, to);
    }
}

/// The file writes of `GetPchHeader` and `GetPchSource`.
fn write_pch_files(ctx: &GenCtx, target: &Target, config: &str, language: &str) {
    if target.props.contains("PRECOMPILE_HEADERS_REUSE_FROM") {
        return;
    }
    let header = pch::pch_header(ctx, target, config, language);
    if header.is_empty() {
        return;
    }
    let dir = target.makefile;
    let mut text = String::from("/* generated by CMake */\n\n");
    if let Some(prologue) = ctx.var(dir, "CMAKE_PCH_PROLOGUE") {
        text += &format!("{prologue}\n");
    }
    if language == "CXX" {
        text += "#ifdef __cplusplus\n";
    }
    let mut first_on_disk = None;
    for value in pch::precompile_headers(ctx, target, config, language) {
        if value.is_empty() {
            continue;
        }
        if value.starts_with(['<', '"']) {
            text += &format!("#include {value}\n");
        } else {
            text += &format!("#include \"{value}\"\n");
        }
        if first_on_disk.is_none() && Path::new(&value).exists() {
            first_on_disk = Some(value);
        }
    }
    if language == "CXX" {
        text += "#endif // __cplusplus\n";
    }
    if let Some(epilogue) = ctx.var(dir, "CMAKE_PCH_EPILOGUE") {
        text += &format!("{epilogue}\n");
    }
    if let Some(parent) = Path::new(&header).parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let tmp = format!("{header}.tmp");
    let _ = std::fs::write(&tmp, &text);
    if let Some(first) = &first_on_disk {
        copy_file_times(first, &tmp);
    }
    move_file_if_different(&tmp, &header);

    let source = pch::pch_source(ctx, target, config, language);
    let tmp = format!("{source}.tmp");
    let _ = std::fs::write(&tmp, "/* generated by CMake */\n");
    copy_file_times(&header, &tmp);
    move_file_if_different(&tmp, &source);
}

/// `cmLocalGenerator::AddPchDependencies` for one configuration (and the
/// single architecture of non-Apple platforms).
fn add_pch_dependencies(ctx: &GenCtx, target: &Target) -> Changes {
    let mut changes = Changes::default();
    let config = ctx.config.clone();
    let dir = target.makefile;
    let sources = gt::sources(ctx, target, &config);
    for language in pch::PCH_LANGUAGES {
        let count = sources
            .iter()
            .filter(|source| {
                source.language == language
                    && crate::commands::target::find_source(ctx.interp, dir, &source.path)
                        .is_none_or(|sf| !sf.props.contains("SKIP_PRECOMPILE_HEADERS"))
            })
            .count();
        if count == 0 {
            continue;
        }
        write_pch_files(ctx, target, &config, language);
        let pch_source = pch::pch_source(ctx, target, &config, language);
        let pch_header = pch::pch_header(ctx, target, &config, language);
        if pch_source.is_empty() || pch_header.is_empty() {
            continue;
        }
        let pch_extension = ctx.var_safe(dir, "CMAKE_PCH_EXTENSION");
        if pch_extension.is_empty() {
            continue;
        }
        let reuse_from = target.props.get("PRECOMPILE_HEADERS_REUSE_FROM");
        // PCH sources cannot contain C++ module references.
        changes.source_props.push((pch_source.clone(), "CXX_SCAN_FOR_MODULES".into(), "0".into(), false));
        if reuse_from.is_none() {
            changes.add_sources.push((pch_source.clone(), true));
        }
        if is_on_opt(ctx.var(dir, "CMAKE_LINK_PCH").as_deref()) {
            let pch_file = pch::pch_file(ctx, target, &config, language);
            match reuse_from {
                None => changes.source_props.push((
                    pch_source.clone(),
                    "OBJECT_OUTPUTS".into(),
                    format!("$<$<CONFIG:{config}>:{pch_file}>"),
                    true,
                )),
                Some(reuse_name) => {
                    if is_on_opt(ctx.var(dir, "CMAKE_PCH_COPY_COMPILE_PDB").as_deref()) {
                        changes.errors.push((gt::directory_backtrace(ctx, dir), format!(
                            "PRECOMPILE_HEADERS_REUSE_FROM \"{reuse_name}\" requires copying the compiler program database (CMAKE_PCH_COPY_COMPILE_PDB), which cmake-rust does not support."
                        )));
                        continue;
                    }
                    let Some(reuse) = pch::reuse_target(ctx, target) else {
                        continue;
                    };
                    let object = pch::pch_file_object(ctx, reuse, &config, language);
                    let config_upper = config.to_ascii_uppercase();
                    if target.kind != TargetKind::ObjectLibrary {
                        let property = if target.kind == TargetKind::StaticLibrary {
                            format!("STATIC_LIBRARY_FLAGS_{config_upper}")
                        } else {
                            format!("LINK_FLAGS_{config_upper}")
                        };
                        let escaped = crate::outconv::escape_for_shell(
                            &object,
                            crate::outconv::NINJA_UNIX,
                        );
                        changes.target_appends.push((property, format!(" {escaped}"), true));
                    } else if reuse.kind == TargetKind::ObjectLibrary {
                        changes.target_appends.push((
                            "INTERFACE_LINK_LIBRARIES".into(),
                            format!("$<$<CONFIG:{config}>:$<LINK_ONLY:{object}>>"),
                            false,
                        ));
                    }
                }
            }
        } else {
            changes.source_props.push((pch_source.clone(), "PCH_EXTENSION".into(), pch_extension, false));
        }
        // The header is grouped as "Precompile Header File".
        changes.add_sources.push((pch_header, false));
    }
    changes
}

// ---------------------------------------------------------------------
// Unity builds

struct UnityBatchedSource {
    path: String,
    language: String,
}

fn unity_file_extension(language: &str) -> &'static str {
    match language {
        "C" => "_c.c",
        "CXX" => "_cxx.cxx",
        "CUDA" => "_cu.cu",
        "OBJC" => "_m.m",
        "OBJCXX" => "_mm.mm",
        _ => "",
    }
}

fn relative_path(from_dir: &str, to: &str) -> String {
    let from: Vec<&str> = from_dir.split('/').filter(|p| !p.is_empty()).collect();
    let to_parts: Vec<&str> = to.split('/').filter(|p| !p.is_empty()).collect();
    let common = from.iter().zip(&to_parts).take_while(|(a, b)| a == b).count();
    let mut parts: Vec<&str> = std::iter::repeat_n("..", from.len() - common).collect();
    parts.extend(&to_parts[common..]);
    parts.join("/")
}

/// `cmLocalGenerator::WriteUnitySourceInclude`.
#[allow(clippy::too_many_arguments)]
fn write_unity_include(
    ctx: &GenCtx,
    dir: usize,
    out: &mut String,
    full_path: &str,
    before: Option<&str>,
    after: Option<&str>,
    unique_id: Option<&str>,
    relocatable: bool,
    unity_dir: &str,
) {
    let mf = &ctx.interp.makefiles[dir];
    let eq_or_sub = |a: &str, b: &str| a == b || crate::util::is_subdirectory(a, b);
    let path = crate::util::filename_path(full_path);
    let (include, to_hash) = if eq_or_sub(&path, &mf.binary_dir) {
        (
            relative_path(unity_dir, full_path),
            format!("BLD_{}", relative_path(&mf.binary_dir, full_path)),
        )
    } else if eq_or_sub(&path, &mf.source_dir) {
        let rel = relative_path(&mf.source_dir, full_path);
        (rel.clone(), format!("SRC_{rel}"))
    } else {
        (full_path.to_owned(), format!("ABS_{full_path}"))
    };
    if let Some(id) = unique_id.filter(|id| !id.is_empty()) {
        use md5::Digest;
        let hash: String = md5::Md5::digest(to_hash.as_bytes())
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        *out += &format!("/* {to_hash} */\n#undef {id}\n#define {id} unity_{hash}\n");
    }
    if let Some(before) = before {
        *out += &format!("{before}\n");
    }
    *out += "/* NOLINTNEXTLINE(bugprone-suspicious-include,misc-include-cleaner) */\n";
    if relocatable {
        *out += &format!("#include \"{include}\"\n");
    } else {
        *out += &format!("#include \"{full_path}\"\n");
    }
    if let Some(after) = after {
        *out += &format!("{after}\n");
    }
    out.push('\n');
}

/// `cmLocalGenerator::AddUnityBuild` (single configuration: every batched
/// source belongs to all configurations, so no per-config guards).
fn add_unity_build(ctx: &GenCtx, target: &Target) -> Changes {
    let mut changes = Changes::default();
    let config = ctx.config.clone();
    let dir = target.makefile;
    let prop = |name: &str| target.props.get(name).map(str::to_owned);
    let source_prop = |path: &str, name: &str| -> Option<String> {
        crate::commands::target::find_source(ctx.interp, dir, path)
            .and_then(|sf| sf.props.get(name).map(str::to_owned))
    };
    // Files needing C++ module scanning cannot be batched; cmake-rust's
    // Ninja generator does not scan modules (NeedDyndepForSource is false).
    let unity_sources: Vec<UnityBatchedSource> = gt::sources(ctx, target, &config)
        .iter()
        .map(|source| UnityBatchedSource {
            path: source.path.clone(),
            language: source.language.clone(),
        })
        .collect();
    let base = format!("{}/Unity/", gt::support_directory(ctx, target));
    let batch_size: usize = prop("UNITY_BUILD_BATCH_SIZE")
        .map(|value| {
            // std::atoi
            let digits: String = value.trim_start().chars().take_while(|c| c.is_ascii_digit()).collect();
            digits.parse().unwrap_or(0)
        })
        .unwrap_or(0);
    let before = prop("UNITY_BUILD_CODE_BEFORE_INCLUDE");
    let after = prop("UNITY_BUILD_CODE_AFTER_INCLUDE");
    let mode = prop("UNITY_BUILD_MODE");
    let relocatable = is_on_opt(target.props.get("UNITY_BUILD_RELOCATABLE"));
    let unique_id = prop("UNITY_BUILD_UNIQUE_ID");
    for language in ["C", "CXX", "OBJC", "OBJCXX", "CUDA"] {
        let on = |path: &str, name: &str| is_on_opt(source_prop(path, name).as_deref());
        let filtered: Vec<&UnityBatchedSource> = unity_sources
            .iter()
            .filter(|ubs| {
                ubs.language == language
                    && !on(&ubs.path, "SKIP_UNITY_BUILD_INCLUSION")
                    && !on(&ubs.path, "HEADER_FILE_ONLY")
                    && source_prop(&ubs.path, "COMPILE_OPTIONS").is_none()
                    && source_prop(&ubs.path, "COMPILE_DEFINITIONS").is_none()
                    && source_prop(&ubs.path, "COMPILE_FLAGS").is_none()
                    && source_prop(&ubs.path, "INCLUDE_DIRECTORIES").is_none()
            })
            .collect();
        // (file name, batched sources)
        let mut files: Vec<(String, Vec<&UnityBatchedSource>)> = Vec::new();
        match mode.as_deref() {
            None | Some("BATCH") => {
                let size = if batch_size == 0 { filtered.len() } else { batch_size };
                for (batch, chunk) in filtered.chunks(size.max(1)).enumerate() {
                    let name = format!("{base}unity_{batch}{}", unity_file_extension(language));
                    files.push((name, chunk.to_vec()));
                }
            }
            Some("GROUP") => {
                // Sources by UNITY_GROUP; ungrouped sources are dropped.
                let mut groups: Vec<(String, Vec<&UnityBatchedSource>)> = Vec::new();
                for ubs in &filtered {
                    if let Some(group) = source_prop(&ubs.path, "UNITY_GROUP") {
                        match groups.iter_mut().find(|(name, _)| *name == group) {
                            Some((_, list)) => list.push(ubs),
                            None => groups.push((group, vec![ubs])),
                        }
                    }
                }
                // CMake iterates a std::unordered_map<std::string, ...>.
                let order = unordered_map_order(groups.iter().map(|(name, _)| name.as_str()));
                let mut groups: Vec<Option<(String, Vec<&UnityBatchedSource>)>> =
                    groups.into_iter().map(Some).collect();
                let groups: Vec<(String, Vec<&UnityBatchedSource>)> =
                    order.into_iter().filter_map(|index| groups[index].take()).collect();
                for (group, list) in groups {
                    let name = format!("{base}unity_{group}{}", unity_file_extension(language));
                    files.push((name, list));
                }
            }
            Some(other) => {
                // cmLocalGenerator::IssueMessage
                changes.errors.push((
                    gt::directory_backtrace(ctx, dir),
                    format!(
                        "Invalid UNITY_BUILD_MODE value of {other} assigned to target {}. Acceptable values are BATCH and GROUP.",
                        target.name
                    ),
                ));
            }
        }
        for (name, batched) in &files {
            // WriteUnitySource
            let mut text = String::from("/* generated by CMake */\n\n");
            for ubs in batched {
                changes.unity_batched.push(ubs.path.clone());
                changes
                    .source_props
                    .push((ubs.path.clone(), "UNITY_SOURCE_FILE".into(), name.clone(), false));
                write_unity_include(
                    ctx,
                    dir,
                    &mut text,
                    &ubs.path,
                    before.as_deref(),
                    after.as_deref(),
                    unique_id.as_deref(),
                    relocatable,
                    &base,
                );
            }
            let _ = crate::fsutil::write_if_different(Path::new(name), text.as_bytes());
            changes.add_sources.push((name.clone(), true));
            for (prop_name, value) in [
                ("SKIP_UNITY_BUILD_INCLUSION", "ON"),
                ("UNITY_SOURCE_FILE", name.as_str()),
                ("CXX_SCAN_FOR_MODULES", "0"),
            ] {
                changes
                    .source_props
                    .push((name.clone(), prop_name.into(), value.into(), false));
            }
            if relocatable {
                let source_dir = ctx.interp.makefiles[dir].source_dir.clone();
                changes
                    .source_props
                    .push((name.clone(), "INCLUDE_DIRECTORIES".into(), source_dir, true));
            }
        }
    }
    changes
}

// ---------------------------------------------------------------------
// libstdc++ std::unordered_map<std::string, T> iteration order

/// `std::_Hash_bytes` (libstdc++ hash_bytes.cc, 64-bit) with the
/// `std::hash<std::string>` seed.
fn libstdcxx_hash(bytes: &[u8]) -> u64 {
    const MUL: u64 = (0xc6a4a793u64 << 32) + 0x5bd1e995;
    let shift_mix = |v: u64| v ^ (v >> 47);
    let len = bytes.len();
    let aligned = len & !7;
    let mut hash = 0xc70f6907u64 ^ (len as u64).wrapping_mul(MUL);
    for chunk in bytes[..aligned].chunks(8) {
        let data = u64::from_le_bytes(chunk.try_into().expect("8 bytes"));
        let data = shift_mix(data.wrapping_mul(MUL)).wrapping_mul(MUL);
        hash ^= data;
        hash = hash.wrapping_mul(MUL);
    }
    if len & 7 != 0 {
        let mut data = 0u64;
        for &b in bytes[aligned..].iter().rev() {
            data = (data << 8) + u64::from(b);
        }
        hash ^= data;
        hash = hash.wrapping_mul(MUL);
    }
    hash = shift_mix(hash).wrapping_mul(MUL);
    shift_mix(hash)
}

/// The iteration order of a libstdc++ unordered_map after inserting
/// distinct `keys` in order (`_M_insert_bucket_begin`, `_M_rehash_aux`),
/// as indices into `keys`.
fn unordered_map_order<'k>(keys: impl Iterator<Item = &'k str>) -> Vec<usize> {
    // Bucket counts after the _Prime_rehash_policy growth steps: the first
    // insertion allocates 13 buckets, then (element count -> buckets).
    const GROWTH: [(usize, usize); 13] = [
        (14, 29),
        (30, 59),
        (60, 127),
        (128, 257),
        (258, 541),
        (542, 1109),
        (1110, 2357),
        (2358, 5087),
        (5088, 10273),
        (10274, 20753),
        (20754, 42043),
        (42044, 85229),
        (85230, 172933),
    ];
    #[derive(Clone, Copy, PartialEq)]
    enum Before {
        Begin,
        Node(usize),
    }
    let hashes: Vec<u64> = keys.map(|k| libstdcxx_hash(k.as_bytes())).collect();
    let mut next: Vec<Option<usize>> = vec![None; hashes.len()];
    let mut begin: Option<usize> = None;
    let mut buckets: Vec<Option<Before>> = vec![None; 13];
    let index = |hash: u64, count: usize| (hash % count as u64) as usize;
    for node in 0..hashes.len() {
        let count = node + 1;
        if let Some(&(_, size)) = GROWTH.iter().find(|(at, _)| *at == count) {
            // _M_rehash_aux (unique keys)
            let mut new_buckets: Vec<Option<Before>> = vec![None; size];
            let mut p = begin;
            begin = None;
            let mut bbegin_bkt = 0;
            while let Some(n) = p {
                let after = next[n];
                let bkt = index(hashes[n], size);
                match new_buckets[bkt] {
                    None => {
                        next[n] = begin;
                        begin = Some(n);
                        new_buckets[bkt] = Some(Before::Begin);
                        if next[n].is_some() {
                            new_buckets[bbegin_bkt] = Some(Before::Node(n));
                        }
                        bbegin_bkt = bkt;
                    }
                    Some(before) => {
                        next[n] = match before {
                            Before::Begin => begin,
                            Before::Node(b) => next[b],
                        };
                        match before {
                            Before::Begin => begin = Some(n),
                            Before::Node(b) => next[b] = Some(n),
                        }
                    }
                }
                p = after;
            }
            buckets = new_buckets;
        }
        // _M_insert_bucket_begin
        let size = buckets.len();
        let bkt = index(hashes[node], size);
        match buckets[bkt] {
            Some(before) => {
                next[node] = match before {
                    Before::Begin => begin,
                    Before::Node(b) => next[b],
                };
                match before {
                    Before::Begin => begin = Some(node),
                    Before::Node(b) => next[b] = Some(node),
                }
            }
            None => {
                next[node] = begin;
                begin = Some(node);
                if let Some(following) = next[node] {
                    buckets[index(hashes[following], size)] = Some(Before::Node(node));
                }
                buckets[bkt] = Some(Before::Begin);
            }
        }
    }
    let mut order = Vec::with_capacity(hashes.len());
    let mut p = begin;
    while let Some(n) = p {
        order.push(n);
        p = next[n];
    }
    order
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn libstdcxx_unordered_map_order() {
        assert_eq!(libstdcxx_hash(b"first"), 0x7585900b71d0dc3e);
        let keys = ["first", "second", "b", "a", "zz", "x1", "x2"];
        let order: Vec<&str> = unordered_map_order(keys.iter().copied())
            .into_iter()
            .map(|i| keys[i])
            .collect();
        assert_eq!(order, ["x2", "x1", "zz", "a", "b", "second", "first"]);
    }

    #[test]
    fn libstdcxx_unordered_map_order_rehash() {
        let keys: Vec<String> = (0..40).map(|i| format!("grp{}", i * 7)).collect();
        let order: Vec<&str> = unordered_map_order(keys.iter().map(String::as_str))
            .into_iter()
            .map(|i| keys[i].as_str())
            .collect();
        assert_eq!(order, ["grp231", "grp224", "grp252", "grp203", "grp84", "grp77", "grp42", "grp35", "grp28", "grp119", "grp245", "grp140", "grp259", "grp14", "grp175", "grp56", "grp7", "grp133", "grp273", "grp70", "grp21", "grp49", "grp91", "grp105", "grp217", "grp0", "grp98", "grp168", "grp63", "grp112", "grp182", "grp126", "grp210", "grp154", "grp266", "grp147", "grp238", "grp189", "grp161", "grp196"]);
    }
}
