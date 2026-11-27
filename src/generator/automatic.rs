//! `cmGlobalGenerator::AddAutomaticSources`: unity builds
//! (`cmLocalGenerator::AddUnityBuild`) and precompiled headers
//! (`cmLocalGenerator::AddPchDependencies`).

use crate::util::relative_path;
use std::collections::{HashMap, HashSet};
use std::path::Path;

use super::ctx::GenCtx;
use super::module_support::ModuleSupport;
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
    /// `OBJECT_OUTPUTS` of PCH sources: (source, config, language, arch).
    /// `GetPchFile` names the object of the PCH source, so the C++ computes
    /// it after `AddSource`; these are evaluated once the sources are added.
    pch_object_outputs: Vec<(String, String, String, String)>,
    /// `cmLocalGenerator::CopyPchCompilePdb` calls: (config, language,
    /// reused target name, extensions).
    pch_pdb_copies: Vec<(String, String, String, Vec<&'static str>)>,
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
    let reuses = |interp: &Interp, key: &str| {
        interp.ext.targets[key]
            .props
            .contains("PRECOMPILE_HEADERS_REUSE_FROM")
    };
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
        interp.issue_at(
            crate::messages::MessageType::FatalError,
            &message,
            &backtrace,
        );
    }
    for (path, name, value, append) in changes.source_props {
        let source = crate::commands::target::get_or_create_source(interp, dir, &path);
        if append {
            source.props.append(&name, &value, false);
        } else {
            source.props.set(&name, value);
        }
    }
    // Every source added here (PCH sources and headers, unity sources, PCH
    // PDB copy rules) was first made a cmSourceFile of the directory by
    // cmMakefile::GetOrCreateSource.
    for (path, _) in &changes.add_sources {
        crate::commands::target::get_or_create_source(interp, dir, path);
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
            let backtrace = [crate::messages::Frame::directory(
                std::rc::Rc::from(file.as_str()),
                target.makefile,
            )];
            target.insert_entry("SOURCES", &path, &backtrace, before);
        }
    }
    for (config, language, reuse_name, extensions) in &changes.pch_pdb_copies {
        copy_pch_compile_pdb(interp, key, config, language, reuse_name, extensions);
    }
    if changes.pch_object_outputs.is_empty() {
        return;
    }
    let outputs: Vec<(String, String)> = {
        let ctx = GenCtx::new(interp);
        let target = &ctx.interp.ext.targets[key];
        changes
            .pch_object_outputs
            .iter()
            .map(|(source, config, language, arch)| {
                let ctx = ctx.for_config(config);
                let pch_file = pch::pch_file(&ctx, target, config, language, arch);
                (source.clone(), format!("$<$<CONFIG:{config}>:{pch_file}>"))
            })
            .collect()
    };
    for (path, value) in outputs {
        let source = crate::commands::target::get_or_create_source(interp, dir, &path);
        source.props.append("OBJECT_OUTPUTS", &value, false);
    }
}

// ---------------------------------------------------------------------
// Precompiled headers

/// `cmLocalGenerator::AddPchDependencies` (for the single architecture of
/// non-Apple platforms).
fn add_pch_dependencies(ctx: &GenCtx, target: &Target) -> Changes {
    let mut changes = Changes::default();
    // Languages whose PCH source an earlier configuration added.
    let mut added = HashSet::new();
    for config in &ctx.configs {
        let ctx = ctx.for_config(config);
        add_pch_dependencies_for_config(&ctx, target, config, &mut added, &mut changes);
    }
    // cmGeneratorTarget::AddSource inserts its entries unconditionally: the
    // last prepended and the first appended one of each source takes effect.
    let mut seen = HashSet::new();
    changes.add_sources.reverse();
    changes
        .add_sources
        .retain(|(path, before)| !before || seen.insert(path.clone()));
    changes.add_sources.reverse();
    changes
}

fn add_pch_dependencies_for_config(
    ctx: &GenCtx,
    target: &Target,
    config: &str,
    added: &mut HashSet<&'static str>,
    changes: &mut Changes,
) {
    let dir = target.makefile;
    let sources = gt::sources(ctx, target, config);
    for language in pch::PCH_LANGUAGES {
        let count = sources
            .iter()
            .filter(|source| {
                source.language == language
                    && crate::commands::target::find_source(ctx.interp, dir, &source.path)
                        .is_none_or(|sf| !sf.props.contains("SKIP_PRECOMPILE_HEADERS"))
            })
            .count();
        if count == 0 && !added.contains(language) {
            continue;
        }
        let archs = pch::pch_archs(ctx, target, config, language);
        // Multi-arch PCH: the use options carry every architecture's
        // -Xarch_-prefixed include flag via the USE_PCH property.
        if archs.len() > 1 {
            let mut use_multi_arch_pch = String::new();
            for arch in &archs {
                let pch_header = pch::pch_header(ctx, target, config, language, arch);
                if !pch_header.is_empty() {
                    use_multi_arch_pch += &format!(";-Xarch_{arch};-include{pch_header}");
                }
            }
            if !use_multi_arch_pch.is_empty() {
                changes.target_appends.push((
                    format!("{language}_COMPILE_OPTIONS_USE_PCH"),
                    format!("$<$<CONFIG:{config}>:{use_multi_arch_pch}>"),
                    false,
                ));
            }
        }
        for arch in &archs {
            let pch_source = pch::pch_source(ctx, target, config, language, arch);
            let pch_header = pch::pch_header(ctx, target, config, language, arch);
            if pch_source.is_empty() || pch_header.is_empty() {
                continue;
            }
            let pch_extension = ctx.var_safe(dir, "CMAKE_PCH_EXTENSION");
            if pch_extension.is_empty() {
                continue;
            }
            let reuse_from = target.props.get("PRECOMPILE_HEADERS_REUSE_FROM");
            // PCH sources cannot contain C++ module references.
            changes.source_props.push((
                pch_source.clone(),
                "CXX_SCAN_FOR_MODULES".into(),
                "0".into(),
                false,
            ));
            if reuse_from.is_none() {
                changes.add_sources.push((pch_source.clone(), true));
                added.insert(language);
            }
            if is_on_opt(ctx.var(dir, "CMAKE_LINK_PCH").as_deref()) {
                match reuse_from {
                    None => changes.pch_object_outputs.push((
                        pch_source.clone(),
                        config.to_owned(),
                        language.to_owned(),
                        arch.to_owned(),
                    )),
                    Some(reuse_name) => {
                        let Some(reuse) = pch::reuse_target(ctx, target) else {
                            continue;
                        };
                        if is_on_opt(ctx.var(dir, "CMAKE_PCH_COPY_COMPILE_PDB").as_deref()) {
                            let compiler_id =
                                ctx.var_safe(dir, &format!("CMAKE_{language}_COMPILER_ID"));
                            let compiler_version =
                                ctx.var_safe(dir, &format!("CMAKE_{language}_COMPILER_VERSION"));
                            let lang_flags = ctx.var_safe(
                                dir,
                                &format!("CMAKE_{language}_FLAGS_{}", config.to_ascii_uppercase()),
                            );
                            let (edit_and_continue, program_database) =
                                match crate::generator::flags::msvc_debug_format_name(
                                    ctx, target, config,
                                ) {
                                    Some(format) if !format.is_empty() => {
                                        (format == "EditAndContinue", format == "ProgramDatabase")
                                    }
                                    _ => (
                                        lang_flags.contains("/ZI") || lang_flags.contains("-ZI"),
                                        lang_flags.contains("/Zi") || lang_flags.contains("-Zi"),
                                    ),
                                };
                            // MSVC 2008 is producing both .pdb and .idb files
                            // with /Zi, but not when used via toolset -Tv90.
                            let msvc2008_or_less = crate::util::version_compare(
                                crate::util::CompareOp::Less,
                                &compiler_version,
                                "16.0",
                            ) && compiler_id == "MSVC"
                                && ctx.var_safe(dir, "CMAKE_VS_PLATFORM_TOOLSET") != "v90";
                            let extensions = if edit_and_continue || msvc2008_or_less {
                                vec![".pdb", ".idb"]
                            } else if program_database {
                                vec![".pdb"]
                            } else {
                                Vec::new()
                            };
                            if !extensions.is_empty() {
                                changes.pch_pdb_copies.push((
                                    config.to_owned(),
                                    language.to_owned(),
                                    reuse_name.to_owned(),
                                    extensions,
                                ));
                            }
                        }
                        let object = pch::pch_file_object(ctx, reuse, config, language, arch);
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
                            changes
                                .target_appends
                                .push((property, format!(" {escaped}"), true));
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
                changes.source_props.push((
                    pch_source.clone(),
                    "PCH_EXTENSION".into(),
                    pch_extension,
                    false,
                ));
            }
            // The header is grouped as "Precompile Header File".
            changes.add_sources.push((pch_header, false));
        }
    }
}

/// `cmLocalGenerator::CopyPchCompilePdb`: a script copying the reused
/// target's compiler program database next to this target's objects, run
/// by a custom command whose output becomes a source of the target.
fn copy_pch_compile_pdb(
    interp: &mut Interp,
    key: &str,
    config: &str,
    language: &str,
    reuse_from: &str,
    extensions: &[&str],
) {
    let multi_config = interp.ext.multi_config;
    let (target_name, dir, prefix, reuse_dir, pch_file) = {
        let ctx = GenCtx::new(interp);
        let target = &ctx.interp.ext.targets[key];
        let Some(reuse) = pch::reuse_target(&ctx, target) else {
            return;
        };
        let ctx = ctx.for_config(config);
        (
            target.name.clone(),
            target.makefile,
            target.props.get("PREFIX").unwrap_or_default().to_owned(),
            ctx.interp.makefiles[reuse.makefile].binary_dir.clone(),
            pch::pch_file(&ctx, reuse, config, language, ""),
        )
    };
    let current_binary = interp.makefiles[dir].binary_dir.clone();
    let pdb_prefix = if multi_config {
        format!("{config}/")
    } else {
        String::new()
    };
    let target_compile_pdb_dir = format!("{current_binary}/{target_name}.dir/");
    let copy_script = format!("{target_compile_pdb_dir}copy_idb_pdb_{config}.cmake");
    let mut file = String::from(
        "# CMake generated file\n\
         # The compiler generated pdb file needs to be written to disk\n\
         # by mspdbsrv. The foreach retry loop is needed to make sure\n\
         # the pdb file is ready to be copied.\n\n",
    );
    for extension in extensions {
        let from_file =
            format!("{reuse_dir}/{reuse_from}.dir/${{PDB_PREFIX}}{reuse_from}{extension}");
        let to_dir = format!("{current_binary}/{target_name}.dir/${{PDB_PREFIX}}");
        let to_file = format!("{to_dir}{reuse_from}{extension}");
        let dest_file = if prefix.is_empty() {
            to_file.clone()
        } else {
            format!("{to_dir}{prefix}{reuse_from}{extension}")
        };
        file.push_str("foreach(retry RANGE 1 30)\n");
        file.push_str(&format!(
            "  if (EXISTS \"{from_file}\" AND (NOT EXISTS \"{dest_file}\" OR NOT \"{dest_file}  \" IS_NEWER_THAN \"{from_file}\"))\n"
        ));
        file.push_str(&format!(
            "    execute_process(COMMAND ${{CMAKE_COMMAND}} -E copy \"{from_file}\" \"{to_dir}\" RESULT_VARIABLE result  ERROR_QUIET)\n"
        ));
        file.push_str(
            "    if (NOT result EQUAL 0)\n      execute_process(COMMAND ${CMAKE_COMMAND} -E sleep 1)\n    else()\n",
        );
        if !prefix.is_empty() {
            file.push_str(&format!("  file(REMOVE \"{dest_file}\")\n"));
            file.push_str(&format!("  file(RENAME \"{to_file}\" \"{dest_file}\")\n"));
        }
        file.push_str("      break()\n    endif()\n");
        file.push_str(&format!(
            "  elseif(NOT EXISTS \"{from_file}\")\n    execute_process(COMMAND ${{CMAKE_COMMAND}} -E sleep 1)\n  endif()\n"
        ));
        file.push_str("endforeach()\n");
    }
    // cmGeneratedFileStream: written only when the content changes.
    let _ = std::fs::create_dir_all(&target_compile_pdb_dir);
    let _ = crate::fsutil::write_if_different(std::path::Path::new(&copy_script), &file);

    let config_genex = |expr: &str| {
        if multi_config {
            format!("$<$<CONFIG:{config}>:{expr}>")
        } else {
            expr.to_owned()
        }
    };
    let output = config_genex(&format!(
        "{target_compile_pdb_dir}{pdb_prefix}{reuse_from}.pdb"
    ));
    let command = crate::state::CustomCommand {
        makefile: dir,
        command_lines: vec![vec![
            config_genex(&interp.cmake_command),
            config_genex(&format!("-DPDB_PREFIX={pdb_prefix}")),
            config_genex("-P"),
            config_genex(&copy_script),
        ]],
        outputs: vec![output.clone()],
        depends: vec![pch_file],
        comment: Some(String::new()),
        escape_allow_make_vars: true,
        ..Default::default()
    };
    for output in &command.outputs {
        crate::commands::target::mark_generated(interp, dir, output);
    }
    crate::commands::custom_actions::run_action(
        interp,
        &mut super::expand_output_genex,
        crate::state::GeneratorAction::Output {
            command,
            replace: false,
        },
    );
    // The copy rule's output: not scanned for modules, and a target source.
    // A per-configuration output stays a generator expression, which the
    // target's sources evaluate for each configuration.
    let rule = if output.contains("$<") {
        output.clone()
    } else {
        crate::util::collapse_full_path(&output, &current_binary)
    };
    let source = crate::commands::target::get_or_create_source(interp, dir, &rule);
    source.props.set("CXX_SCAN_FOR_MODULES", "0".to_owned());
    // Sources injected at generate time carry the makefile's persistent
    // file-scope frame (cmLocalGenerator::DirectoryBacktrace).
    let makefile = &interp.makefiles[dir];
    let file = makefile
        .list_files
        .first()
        .cloned()
        .unwrap_or_else(|| format!("{}/CMakeLists.txt", makefile.source_dir));
    let backtrace = [crate::messages::Frame::directory(
        std::rc::Rc::from(file.as_str()),
        dir,
    )];
    let target = interp.ext.targets.get_mut(key).expect("target");
    target.insert_entry("SOURCES", &rule, &backtrace, false);
    target
        .props
        .set("COMPILE_PDB_OUTPUT_DIRECTORY", target_compile_pdb_dir);
}

// ---------------------------------------------------------------------
// Unity builds

/// `cmLocalGenerator::UnityBatchedSource`.
struct UnityBatchedSource {
    path: String,
    language: String,
    /// Indices of the generator configurations that compile the source.
    configs: Vec<usize>,
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

/// `cmLocalGenerator::WriteUnitySourceInclude`.
#[allow(clippy::too_many_arguments)]
fn write_unity_include(
    ctx: &GenCtx,
    out: &mut String,
    condition: Option<&str>,
    full_path: &str,
    before: Option<&str>,
    after: Option<&str>,
    unique_id: Option<&str>,
    relocatable: bool,
    unity_dir: &str,
) {
    if let Some(condition) = condition {
        *out += &format!("#if {condition}\n");
    }
    // cmLocalGenerator::Get{Source,Binary}Directory are the top-level ones.
    let top = &ctx.interp.makefiles[0];
    let eq_or_sub = |a: &str, b: &str| a == b || crate::util::is_subdirectory(a, b);
    let path = crate::util::filename_path(full_path);
    let (include, to_hash) = if eq_or_sub(&path, &top.binary_dir) {
        (
            relative_path(unity_dir, full_path),
            format!("BLD_{}", relative_path(&top.binary_dir, full_path)),
        )
    } else if eq_or_sub(&path, &top.source_dir) {
        let rel = relative_path(&top.source_dir, full_path);
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
    if condition.is_some() {
        *out += "#endif\n";
    }
    out.push('\n');
}

/// `cmLocalGenerator::AddUnityBuild`.
fn add_unity_build(ctx: &GenCtx, target: &Target) -> Changes {
    let mut changes = Changes::default();
    let dir = target.makefile;
    let prop = |name: &str| target.props.get(name).map(str::to_owned);
    let source_prop = |path: &str, name: &str| -> Option<String> {
        crate::commands::target::find_source(ctx.interp, dir, path)
            .and_then(|sf| sf.props.get(name).map(str::to_owned))
    };
    let mut unity_sources: Vec<UnityBatchedSource> = Vec::new();
    let mut index: HashMap<String, usize> = HashMap::new();
    for (config_index, config) in ctx.configs.iter().enumerate() {
        let ctx = ctx.for_config(config);
        let modules = ModuleSupport::new(&ctx, target, config);
        for source in gt::sources(&ctx, target, config).iter() {
            // Files which need C++ scanning cannot participate in unity
            // builds.
            if source.language == "CXX" && modules.need_dyndep_for_source(source) {
                continue;
            }
            let at = *index.entry(source.path.clone()).or_insert_with(|| {
                unity_sources.push(UnityBatchedSource {
                    path: source.path.clone(),
                    language: source.language.clone(),
                    configs: Vec::new(),
                });
                unity_sources.len() - 1
            });
            unity_sources[at].configs.push(config_index);
        }
    }
    let base = format!("{}/Unity/", gt::support_directory(ctx, target));
    let batch_size: usize = prop("UNITY_BUILD_BATCH_SIZE")
        .map(|value| {
            // std::atoi
            let digits: String = value
                .trim_start()
                .chars()
                .take_while(|c| c.is_ascii_digit())
                .collect();
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
                let size = if batch_size == 0 {
                    filtered.len()
                } else {
                    batch_size
                };
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
                let groups: Vec<(String, Vec<&UnityBatchedSource>)> = order
                    .into_iter()
                    .filter_map(|index| groups[index].take())
                    .collect();
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
            let mut per_config = false;
            for ubs in batched {
                let condition = (ubs.configs.len() != ctx.configs.len()).then(|| {
                    per_config = true;
                    ubs.configs
                        .iter()
                        .map(|index| {
                            format!(
                                "defined(CMAKE_UNITY_CONFIG_{})",
                                ctx.configs[*index].to_ascii_uppercase()
                            )
                        })
                        .collect::<Vec<_>>()
                        .join(" || ")
                });
                changes.unity_batched.push(ubs.path.clone());
                changes.source_props.push((
                    ubs.path.clone(),
                    "UNITY_SOURCE_FILE".into(),
                    name.clone(),
                    false,
                ));
                write_unity_include(
                    ctx,
                    &mut text,
                    condition.as_deref(),
                    &ubs.path,
                    before.as_deref(),
                    after.as_deref(),
                    unique_id.as_deref(),
                    relocatable,
                    &base,
                );
            }
            let _ = crate::fsutil::write_if_different(Path::new(name), &text);
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
            if per_config {
                changes.source_props.push((
                    name.clone(),
                    "COMPILE_DEFINITIONS".into(),
                    "CMAKE_UNITY_CONFIG_$<UPPER_CASE:$<CONFIG>>".into(),
                    false,
                ));
            }
            if relocatable {
                let source_dir = ctx.interp.makefiles[0].source_dir.clone();
                changes.source_props.push((
                    name.clone(),
                    "INCLUDE_DIRECTORIES".into(),
                    source_dir,
                    true,
                ));
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
        assert_eq!(
            order,
            [
                "grp231", "grp224", "grp252", "grp203", "grp84", "grp77", "grp42", "grp35",
                "grp28", "grp119", "grp245", "grp140", "grp259", "grp14", "grp175", "grp56",
                "grp7", "grp133", "grp273", "grp70", "grp21", "grp49", "grp91", "grp105", "grp217",
                "grp0", "grp98", "grp168", "grp63", "grp112", "grp182", "grp126", "grp210",
                "grp154", "grp266", "grp147", "grp238", "grp189", "grp161", "grp196"
            ]
        );
    }
}
